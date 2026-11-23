use std::{
    ffi::OsString, fs, io, mem::offset_of, os::windows::ffi::OsStringExt, path::Path, sync::Arc,
};

use windows_sys::Win32::{
    Foundation::ERROR_NO_MORE_FILES,
    Storage::FileSystem::{
        FileIdBothDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
        FILE_LIST_DIRECTORY,
    },
};

use super::{invalid, Checkpoint, Namespace, Reader};
use crate::{
    folder::{make_entry, Exclusions},
    windows::Handle,
    Entry, RootCoverage,
};

#[derive(Debug, PartialEq, Eq)]
struct ScopedLink {
    id: u64,
    parent_id: u64,
    entry: Arc<Entry>,
}

/// Only names confirmed by directory enumeration under the current token are
/// retained. The raw volume namespace never enters the published index.
#[derive(Debug, PartialEq, Eq)]
pub struct ScopedSnapshot {
    pub checkpoint: Checkpoint,
    pub enumeration_batches: usize,
    pub hard_link_aliases: usize,
    links: Vec<ScopedLink>,
}

impl ScopedSnapshot {
    pub(crate) fn entries(&self) -> impl Iterator<Item = Arc<Entry>> + '_ {
        self.links.iter().map(|link| link.entry.clone())
    }

    /// Return volume-local IDs for a name already visible in this snapshot.
    pub fn file_reference(&self, path: &Path) -> Option<(u64, u64)> {
        let name = path.file_name()?;
        let parent = path.parent()?;
        self.links
            .iter()
            .find(|link| link.entry.name == name && *link.entry.parent == *parent)
            .map(|link| (link.id, link.parent_id))
    }
}

pub(crate) fn capture(
    root: &Path,
    canonical_root: &Path,
    exclusions: &Exclusions,
    coverage: &mut RootCoverage,
    batch_bytes: usize,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(u64),
) -> io::Result<ScopedSnapshot> {
    let reader = Reader::open(root)?;
    let mut namespace = Namespace::default();
    let (checkpoint, enumeration_batches) = reader.enumerate(batch_bytes, |record| {
        check_cancelled(&cancelled)?;
        namespace.insert_enumerated(record);
        Ok(())
    })?;

    let mut snapshot = ScopedSnapshot {
        checkpoint,
        enumeration_batches,
        hard_link_aliases: 0,
        links: Vec::new(),
    };
    let mut directory_buffer = vec![0_u64; 8192];
    let mut pending = vec![(
        Arc::<Path>::from(root),
        canonical_root.to_path_buf(),
        None::<ScopedLink>,
    )];
    while let Some((parent, io_path, link)) = pending.pop() {
        check_cancelled(&cancelled)?;
        let (handle, parent_id) = match open_directory(&io_path, snapshot.checkpoint.volume_serial)
        {
            Ok(directory) => directory,
            Err(error) if link.is_some() && error.kind() == io::ErrorKind::PermissionDenied => {
                coverage.skip(&parent, &error.to_string());
                continue;
            }
            Err(error) => return Err(error),
        };
        // A directory is searchable only after its own list handle was opened.
        if let Some(link) = link {
            if link.id != parent_id {
                return Err(invalid("NTFS directory changed during projection"));
            }
            snapshot.links.push(link);
            if snapshot.links.len().is_multiple_of(256) {
                progress(snapshot.links.len() as u64);
            }
        }
        list_directory(&handle, &mut directory_buffer, |name, id, attributes| {
            check_cancelled(&cancelled)?;
            let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            let path = parent.join(&name);
            if exclusions.matches(&path, is_directory) {
                coverage.skip(&path, "excluded");
                return Ok(());
            }
            if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                && (is_directory || fs::symlink_metadata(&path)?.file_type().is_symlink())
            {
                coverage.skip(&path, "reparse point");
                return Ok(());
            }
            let record = namespace.records.get(&id).ok_or_else(|| {
                invalid("visible directory entry is missing from the NTFS enumeration")
            })?;
            let primary = record.parent_id == parent_id && record.name == name;
            if is_directory != (record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0)
                || (is_directory && !primary)
            {
                return Err(invalid("NTFS namespace changed during projection"));
            }
            // The current path was resolved through a chain of verified parent
            // IDs. A file with another name or parent is a verified hard link.
            let entry = make_entry(&parent, name, is_directory);
            let link = ScopedLink {
                id,
                parent_id,
                entry,
            };
            if is_directory {
                let directory = io_path.join(&link.entry.name);
                pending.push((Arc::from(path), directory, Some(link)));
            } else {
                snapshot.hard_link_aliases += usize::from(!primary);
                snapshot.links.push(link);
                if snapshot.links.len().is_multiple_of(256) {
                    progress(snapshot.links.len() as u64);
                }
            }
            Ok(())
        })?;
    }
    coverage.entries = snapshot.links.len() as u64;
    Ok(snapshot)
}

fn check_cancelled(cancelled: impl Fn() -> bool) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "NTFS snapshot cancelled",
        ))
    } else {
        Ok(())
    }
}

fn open_directory(path: &Path, volume_serial: u32) -> io::Result<(Handle, u64)> {
    // Require the same ordinary directory access as the folder backend before
    // opening the Win32 directory handle, which requires BACKUP_SEMANTICS.
    fs::read_dir(path)?;
    let handle = Handle::open(
        path.as_os_str(),
        FILE_LIST_DIRECTORY,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The handle is live and the output is an SDK structure of the required size.
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwVolumeSerialNumber != volume_serial
        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(invalid(
            "directory crossed the NTFS volume or became a reparse point",
        ));
    }
    let id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((handle, id))
}

fn list_directory(
    handle: &Handle,
    buffer: &mut [u64],
    mut visit: impl FnMut(OsString, u64, u32) -> io::Result<()>,
) -> io::Result<()> {
    loop {
        let bytes = size_of_val(buffer);
        // The buffer is eight-byte aligned and writable for the supplied size.
        if unsafe {
            GetFileInformationByHandleEx(
                handle.0,
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                bytes as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), bytes) };
        parse_directory_batch(bytes, &mut visit)?;
    }
}

fn parse_directory_batch(
    mut record: &[u8],
    visit: &mut impl FnMut(OsString, u64, u32) -> io::Result<()>,
) -> io::Result<()> {
    const NAME: usize = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    const LENGTH: usize = offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
    const ID: usize = offset_of!(FILE_ID_BOTH_DIR_INFO, FileId);
    const ATTRIBUTES: usize = offset_of!(FILE_ID_BOTH_DIR_INFO, FileAttributes);
    while record.len() >= NAME {
        let u32_at = |offset| u32::from_le_bytes(record[offset..offset + 4].try_into().unwrap());
        let next = u32_at(0) as usize;
        let length = u32_at(LENGTH) as usize;
        if length == 0
            || !length.is_multiple_of(2)
            || length > record.len() - NAME
            || (next != 0
                && (!next.is_multiple_of(8) || next < NAME + length || next > record.len()))
        {
            return Err(invalid("invalid NTFS directory record bounds"));
        }
        let name: Vec<_> = record[NAME..NAME + length]
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect();
        if name != [46] && name != [46, 46] {
            if name.iter().any(|&unit| {
                unit < 32 || u8::try_from(unit).is_ok_and(|byte| b"<>:\"/\\|?*".contains(&byte))
            }) {
                return Err(invalid("invalid NTFS directory entry name"));
            }
            let name = OsString::from_wide(&name);
            if Path::new(&name).file_name() != Some(&name) {
                return Err(invalid("invalid NTFS directory entry name"));
            }
            let id = u64::from_le_bytes(record[ID..ID + 8].try_into().unwrap());
            visit(name, id, u32_at(ATTRIBUTES))?;
        }
        if next == 0 {
            return Ok(());
        }
        record = &record[next..];
    }
    Err(invalid("truncated NTFS directory record"))
}
