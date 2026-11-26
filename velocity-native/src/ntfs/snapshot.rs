use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs, io,
    mem::offset_of,
    os::windows::ffi::OsStringExt,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::ERROR_NO_MORE_FILES,
    Storage::FileSystem::{
        FileIdBothDirectoryInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
        FILE_LIST_DIRECTORY,
    },
    System::Ioctl::{
        USN_REASON_FILE_CREATE, USN_REASON_FILE_DELETE, USN_REASON_HARD_LINK_CHANGE,
        USN_REASON_RENAME_NEW_NAME, USN_REASON_RENAME_OLD_NAME, USN_REASON_REPARSE_POINT_CHANGE,
        USN_REASON_SECURITY_CHANGE,
    },
};

use super::{invalid, Checkpoint, Namespace, Reader};
use crate::{
    folder::{make_entry, Exclusions},
    windows::Handle,
    Entry, RootCoverage,
};

pub(crate) mod disk;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScopedLink {
    id: u64,
    parent_id: u64,
    entry: Arc<Entry>,
}

/// Only names confirmed by directory enumeration under the current token are
/// retained. The raw volume namespace never enters the published index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedSnapshot {
    pub checkpoint: Checkpoint,
    pub enumeration_start_usn: i64,
    pub enumeration_batches: usize,
    pub hard_link_aliases: usize,
    links: Arc<Vec<ScopedLink>>,
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

    pub(crate) fn same_projection(&self, other: &Self) -> bool {
        self.checkpoint.volume_identity == other.checkpoint.volume_identity
            && self.checkpoint.journal_id == other.checkpoint.journal_id
            && self.enumeration_start_usn == other.enumeration_start_usn
            && self.enumeration_batches == other.enumeration_batches
            && self.links == other.links
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
    let (mut checkpoint, enumeration_batches) = reader.enumerate(batch_bytes, |record| {
        check_cancelled(&cancelled)?;
        namespace.insert_enumerated(record);
        Ok(())
    })?;
    let enumeration_start_usn = checkpoint.next_usn;

    let changes = read_changes(&reader, &mut checkpoint, batch_bytes, &cancelled)?;
    let clean_coverage = coverage.clone();
    let mut links = project(
        root,
        canonical_root,
        exclusions,
        coverage,
        checkpoint.volume_serial,
        changes.is_empty().then_some(&namespace),
        None,
        &cancelled,
        &mut progress,
    );
    // A change after the first catch-up may invalidate the raw ID projection.
    if links.is_err()
        && !read_changes(&reader, &mut checkpoint, batch_bytes, &cancelled)?.is_empty()
    {
        *coverage = clean_coverage;
        links = project(
            root,
            canonical_root,
            exclusions,
            coverage,
            checkpoint.volume_serial,
            None,
            None,
            &cancelled,
            progress,
        );
    }
    let links = links?;
    let mut snapshot = ScopedSnapshot {
        checkpoint,
        enumeration_start_usn,
        enumeration_batches,
        hard_link_aliases: alias_count(&links),
        links,
    };
    if let Some((next, updated_coverage)) = advance(
        &reader,
        &snapshot,
        coverage,
        exclusions,
        batch_bytes,
        &cancelled,
        false,
    )? {
        *coverage = updated_coverage;
        snapshot = next;
    }
    Ok(snapshot)
}

pub(crate) fn tail(
    previous: &ScopedSnapshot,
    coverage: &RootCoverage,
    exclusions: &Exclusions,
    batch_bytes: usize,
    cancelled: impl Fn() -> bool,
) -> io::Result<Option<(ScopedSnapshot, RootCoverage)>> {
    let reader = Reader::open(&coverage.root)?;
    advance(
        &reader,
        previous,
        coverage,
        exclusions,
        batch_bytes,
        cancelled,
        false,
    )
}

fn advance(
    reader: &Reader,
    previous: &ScopedSnapshot,
    coverage: &RootCoverage,
    exclusions: &Exclusions,
    batch_bytes: usize,
    cancelled: impl Fn() -> bool,
    validate_scope: bool,
) -> io::Result<Option<(ScopedSnapshot, RootCoverage)>> {
    let mut next = previous.clone();
    let mut coverage = coverage.clone();
    let mut journal_time = Duration::ZERO;
    let mut read = |checkpoint: &mut Checkpoint| {
        let started = Instant::now();
        let changes = read_changes(reader, checkpoint, batch_bytes, &cancelled);
        journal_time += started.elapsed();
        changes
    };
    let mut changes = read(&mut next.checkpoint)?;
    if next.checkpoint == previous.checkpoint && !validate_scope {
        return Ok(None);
    }
    // A new process must verify its current visibility even if the journal
    // was quiet. For example, group membership can change outside this volume.
    changes.scope |= validate_scope;
    for _ in 0..4 {
        if changes.is_empty() {
            break;
        }
        let (root, canonical) = (
            coverage.root.clone(),
            coverage
                .canonical
                .clone()
                .ok_or_else(|| invalid("missing NTFS root path"))?,
        );
        let (links, aliases) =
            match relist_files(&next, &coverage, exclusions, &changes, &cancelled) {
                Ok(Some(links)) => (Arc::new(links), next.hard_link_aliases),
                Err(error) if cancelled() => return Err(error),
                _ => {
                    coverage.skipped = 0;
                    coverage.examples.clear();
                    let links = project(
                        &root,
                        &canonical,
                        exclusions,
                        &mut coverage,
                        next.checkpoint.volume_serial,
                        None,
                        Some(&next.links),
                        &cancelled,
                        |_| {},
                    )?;
                    let aliases = alias_count(&links);
                    (links, aliases)
                }
            };
        coverage.entries = links.len() as u64;
        next.hard_link_aliases = aliases;
        next.links = links;
        changes = read(&mut next.checkpoint)?;
    }
    if !changes.is_empty() {
        return Err(invalid(
            "NTFS namespace kept changing during journal replay",
        ));
    }
    if validate_scope {
        tracing::info!(root = %coverage.root.display(), journal_ms = journal_time.as_millis(), "Read journal during snapshot restore");
    }
    Ok(Some((next, coverage)))
}

#[derive(Default)]
struct Changes {
    scope: bool,
    parents: HashSet<u64>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        !self.scope && self.parents.is_empty()
    }
}

fn read_changes(
    reader: &Reader,
    checkpoint: &mut Checkpoint,
    batch_bytes: usize,
    cancelled: impl Fn() -> bool,
) -> io::Result<Changes> {
    let end = reader.checkpoint()?.next_usn;
    let mut changes = Changes::default();
    loop {
        check_cancelled(&cancelled)?;
        let (next, records) = reader.read(checkpoint, batch_bytes)?;
        for record in records {
            let name_changed = record.reason
                & (USN_REASON_FILE_CREATE
                    | USN_REASON_FILE_DELETE
                    | USN_REASON_RENAME_OLD_NAME
                    | USN_REASON_RENAME_NEW_NAME)
                != 0;
            if record.reason
                & (USN_REASON_HARD_LINK_CHANGE
                    | USN_REASON_SECURITY_CHANGE
                    | USN_REASON_REPARSE_POINT_CHANGE)
                != 0
                || (name_changed && record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0)
            {
                changes.scope = true;
            } else if name_changed && !changes.scope {
                changes.parents.insert(record.parent_id);
                changes.scope = changes.parents.len() > 32;
            }
        }
        let reached_end = next.next_usn >= end || next == *checkpoint;
        *checkpoint = next;
        if reached_end {
            return Ok(changes);
        }
    }
}

fn relist_files(
    snapshot: &ScopedSnapshot,
    coverage: &RootCoverage,
    exclusions: &Exclusions,
    changes: &Changes,
    cancelled: impl Fn() -> bool,
) -> io::Result<Option<Vec<ScopedLink>>> {
    if changes.scope || coverage.skipped != coverage.examples.len() as u64 {
        return Ok(None);
    }
    let serial = snapshot.checkpoint.volume_serial;
    let root = &coverage.root;
    let (_, root_id) = open_directory(root, serial)?;
    let mut paths = Vec::with_capacity(changes.parents.len());
    for &id in &changes.parents {
        let path = if id == root_id {
            root.to_path_buf()
        } else if let Some(link) = snapshot
            .links
            .iter()
            .find(|link| link.id == id && link.entry.is_directory)
        {
            link.entry.parent.join(&link.entry.name)
        } else {
            return Ok(None);
        };
        paths.push((id, Arc::<Path>::from(path)));
    }

    let mut links = Vec::with_capacity(snapshot.links.len());
    let mut buffer = vec![0_u64; 8192];
    for (expected_id, path) in paths {
        check_cancelled(&cancelled)?;
        // A nearby ACL or reparse change can precede its journal record. Check
        // every ancestor instead of relying on the old projection for access.
        for ancestor in path
            .ancestors()
            .skip(1)
            .take_while(|ancestor| *ancestor != root)
        {
            open_directory(ancestor, serial)?;
        }
        let (handle, parent_id) = open_directory(&path, serial)?;
        if parent_id != expected_id {
            return Ok(None);
        }
        // Confirm previously skipped siblings using the parent listing so
        // protected directories require no metadata access of their own.
        let mut skipped: HashSet<_> = coverage
            .examples
            .iter()
            .filter_map(|(entry, _)| {
                (entry.parent() == Some(&path))
                    .then(|| entry.file_name())
                    .flatten()
            })
            .collect();
        list_directory(&handle, &mut buffer, |name, id, attributes| {
            check_cancelled(&cancelled)?;
            let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
            if skipped.remove(name.as_os_str()) && !is_directory {
                return Err(invalid("skipped entry needs full scope reconciliation"));
            }
            if !is_directory {
                if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || exclusions.matches(&path.join(&name), false)
                {
                    return Err(invalid("directory needs full scope reconciliation"));
                }
                links.push(ScopedLink {
                    id,
                    parent_id,
                    entry: make_entry(&path, name, false),
                });
            }
            Ok(())
        })?;
        if !skipped.is_empty() {
            return Ok(None);
        }
    }
    sort_links(&mut links);
    let mut replacements = links.into_iter().peekable();
    let mut merged = Vec::with_capacity(snapshot.links.len());
    for current in snapshot
        .links
        .iter()
        .filter(|link| link.entry.is_directory || !changes.parents.contains(&link.parent_id))
    {
        while replacements.peek().is_some_and(|replacement| {
            crate::compare_entries(&replacement.entry, &current.entry).is_le()
        }) {
            merged.push(replacements.next().unwrap());
        }
        merged.push(current.clone());
    }
    merged.extend(replacements);
    Ok(Some(merged))
}

fn sort_links(links: &mut [ScopedLink]) {
    links.sort_unstable_by(|a, b| crate::compare_entries(&a.entry, &b.entry));
}

fn alias_count(links: &[ScopedLink]) -> usize {
    let mut files = HashSet::new();
    links
        .iter()
        .filter(|link| !link.entry.is_directory && !files.insert(link.id))
        .count()
}

#[allow(clippy::too_many_arguments)]
fn project(
    root: &Path,
    canonical_root: &Path,
    exclusions: &Exclusions,
    coverage: &mut RootCoverage,
    volume_serial: u32,
    namespace: Option<&Namespace>,
    previous: Option<&Arc<Vec<ScopedLink>>>,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(u64),
) -> io::Result<Arc<Vec<ScopedLink>>> {
    let mut links = Vec::new();
    let mut all_reused = true;
    let mut previous_rows: HashMap<_, _> = previous
        .into_iter()
        .flat_map(|links| links.iter())
        .map(|link| ((link.id, link.parent_id), &link.entry))
        .collect();
    let mut directory_buffer = vec![0_u64; 8192];
    let mut pending = vec![(
        Arc::<Path>::from(root),
        canonical_root.to_path_buf(),
        None::<ScopedLink>,
    )];
    while let Some((parent, io_path, link)) = pending.pop() {
        check_cancelled(&cancelled)?;
        let (handle, parent_id) = match open_directory(&io_path, volume_serial) {
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
            links.push(link);
            if links.len().is_multiple_of(256) {
                progress(links.len() as u64);
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
            if let Some(namespace) = namespace {
                let record = namespace.records.get(&id).ok_or_else(|| {
                    invalid("visible directory entry is missing from the NTFS enumeration")
                })?;
                let primary = record.parent_id == parent_id && record.name == name;
                if is_directory != (record.attributes & FILE_ATTRIBUTE_DIRECTORY != 0)
                    || (is_directory && !primary)
                {
                    return Err(invalid("NTFS namespace changed during projection"));
                }
            }
            // The current path was resolved through a chain of verified parent
            // IDs. A file with another name or parent is a verified hard link.
            let entry = previous_rows
                .remove(&(id, parent_id))
                .filter(|entry| {
                    entry.parent == parent
                        && entry.name == name
                        && entry.is_directory == is_directory
                })
                .cloned()
                .unwrap_or_else(|| {
                    all_reused = false;
                    make_entry(&parent, name, is_directory)
                });
            let link = ScopedLink {
                id,
                parent_id,
                entry,
            };
            if is_directory {
                let directory = io_path.join(&link.entry.name);
                pending.push((Arc::from(path), directory, Some(link)));
            } else {
                links.push(link);
                if links.len().is_multiple_of(256) {
                    progress(links.len() as u64);
                }
            }
            Ok(())
        })?;
    }
    coverage.entries = links.len() as u64;
    if let Some(previous) = previous {
        if all_reused && previous_rows.is_empty() && links.len() == previous.len() {
            return Ok(previous.clone());
        }
    }
    sort_links(&mut links);
    Ok(Arc::new(links))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RootState;

    #[test]
    #[ignore = "uses a disposable NTFS volume with a journal"]
    fn catches_changes_during_projection_and_replays_idempotently() {
        let root = std::env::var_os("VELOCITY_NTFS_VOLUME_A")
            .map(std::path::PathBuf::from)
            .unwrap();
        let canonical = fs::canonicalize(&root).unwrap();
        let batch = root.join("fixture/batch");
        for index in 0..260 {
            fs::write(batch.join(format!("catch-up-padding-{index}.txt")), "").unwrap();
        }
        let before = root.join("fixture/alpha/catch-up-before.txt");
        let after = root.join("fixture/alpha/catch-up-after.txt");
        fs::write(&before, "").unwrap();
        let exclusions = Exclusions::new(&[
            "**/System Volume Information/**".into(),
            "**/$RECYCLE.BIN/**".into(),
        ])
        .unwrap();
        let mut coverage = RootCoverage::new(root.clone(), RootState::Ready);
        coverage.canonical = Some(canonical.clone());
        let mut renamed = false;
        let initial = capture(
            &root,
            &canonical,
            &exclusions,
            &mut coverage,
            1024,
            || false,
            |_| {
                if !renamed {
                    fs::rename(&before, &after).unwrap();
                    renamed = true;
                }
            },
        )
        .unwrap();
        assert!(renamed);
        assert!(initial.file_reference(&before).is_none());
        assert!(initial.file_reference(&after).is_some());

        fs::rename(&after, &before).unwrap();
        let first = tail(&initial, &coverage, &exclusions, 1024, || false)
            .unwrap()
            .unwrap()
            .0;
        let second = tail(&initial, &coverage, &exclusions, 1024, || false)
            .unwrap()
            .unwrap()
            .0;
        assert!(first.same_projection(&second));
        assert_eq!(first.checkpoint, second.checkpoint);
        assert!(first.file_reference(&before).is_some());
        assert!(first.file_reference(&after).is_none());
        println!("startup journal catch-up and duplicate replay passed");
    }
}
