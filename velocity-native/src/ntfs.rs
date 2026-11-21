//! Windows volume enumeration and journal access. Raw names require a
//! user-visible namespace projection before they can be served by a provider.

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{ERROR_HANDLE_EOF, GENERIC_READ},
    Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY,
    System::{
        Ioctl::{
            FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V1,
            READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0, USN_REASON_FILE_DELETE,
            USN_REASON_HARD_LINK_CHANGE, USN_REASON_RENAME_OLD_NAME, USN_REASON_SECURITY_CHANGE,
        },
        IO::DeviceIoControl,
    },
};

use crate::{
    windows::{self, Handle},
    Volume,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub id: u64,
    pub parent_id: u64,
    pub usn: i64,
    pub reason: u32,
    pub attributes: u32,
    pub name: OsString,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub volume_identity: String,
    pub volume_serial: u32,
    pub journal_id: u64,
    pub next_usn: i64,
}

pub struct Reader {
    handle: Handle,
    volume: Volume,
}

/// Candidate names from USN records. The raw journal can omit hard-link names
/// and includes entries that the search user cannot list.
#[derive(Default)]
pub struct Namespace {
    records: HashMap<u64, Record>,
    needs_reconciliation: HashSet<u64>,
}

impl Namespace {
    pub fn insert_enumerated(&mut self, record: Record) {
        self.records.insert(record.id, record);
    }

    pub fn apply(&mut self, changes: impl IntoIterator<Item = Record>) {
        for record in changes {
            let current = self.records.get(&record.id);
            if current.is_some_and(|current| current.usn > record.usn) {
                continue;
            }
            if record.reason & (USN_REASON_HARD_LINK_CHANGE | USN_REASON_SECURITY_CHANGE) != 0 {
                self.needs_reconciliation.insert(record.id);
            }
            if record.reason & USN_REASON_FILE_DELETE != 0 {
                self.records.remove(&record.id);
            } else if record.reason & USN_REASON_RENAME_OLD_NAME != 0 {
                if current.is_some_and(|current| {
                    current.parent_id == record.parent_id && current.name == record.name
                }) {
                    self.records.remove(&record.id);
                }
            } else {
                self.records.insert(record.id, record);
            }
        }
    }

    pub fn ids_needing_reconciliation(&self) -> &HashSet<u64> {
        &self.needs_reconciliation
    }

    /// Resolve a candidate through parent IDs. Missing ancestors, cycles and
    /// invalid name components are rejected before a candidate reaches callers.
    pub fn path(&self, id: u64, root_id: u64, mount: &Path) -> Option<PathBuf> {
        let mut ancestors = Vec::new();
        let mut visited = HashSet::new();
        let mut id = id;
        while id != root_id {
            let record = self.records.get(&id)?;
            let name = Path::new(&record.name);
            if !visited.insert(id)
                || name.file_name() != Some(&record.name)
                || (!ancestors.is_empty() && record.attributes & FILE_ATTRIBUTE_DIRECTORY == 0)
            {
                return None;
            }
            ancestors.push(&record.name);
            id = record.parent_id;
        }
        let mut path = mount.to_owned();
        path.extend(ancestors.into_iter().rev());
        Some(path)
    }
}

impl Reader {
    /// Open a local NTFS volume by GUID. The caller's existing token determines
    /// access. The reader neither creates nor modifies the USN journal.
    pub fn open(mount: &Path) -> io::Result<Self> {
        let volume = windows::volume(mount)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "volume is unavailable"))?;
        if volume.mount != mount || !volume.filesystem.eq_ignore_ascii_case("NTFS") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supply the root of a local NTFS volume",
            ));
        }
        let identity = volume
            .identity
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "no stable volume GUID"))?;
        let handle = Handle::open(OsStr::new(identity.trim_end_matches('\\')), GENERIC_READ, 0)?;
        Ok(Self { handle, volume })
    }

    pub fn checkpoint(&self) -> io::Result<Checkpoint> {
        let journal = self.journal()?;
        Ok(Checkpoint {
            volume_identity: self.volume.identity.clone().unwrap(),
            volume_serial: self.volume.serial,
            journal_id: journal.UsnJournalID,
            next_usn: journal.NextUsn,
        })
    }

    pub fn enumerate(
        &self,
        buffer_bytes: usize,
        mut accept: impl FnMut(Record) -> io::Result<()>,
    ) -> io::Result<(Checkpoint, usize)> {
        let checkpoint = self.checkpoint()?;
        let mut input = MFT_ENUM_DATA_V1 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: i64::MAX,
            MinMajorVersion: 2,
            MaxMajorVersion: 2,
        };
        let mut batches = 0;
        let mut output = batch_buffer(buffer_bytes)?;
        loop {
            let bytes = match self.batch(FSCTL_ENUM_USN_DATA, &input, &mut output) {
                Ok(bytes) => bytes,
                Err(err) if err.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) => break,
                Err(err) => return Err(err),
            };
            let (cursor, records) = parse_batch(bytes)?;
            if cursor <= input.StartFileReferenceNumber {
                return Err(invalid("NTFS enumeration cursor did not advance"));
            }
            for record in records {
                accept(record)?;
            }
            input.StartFileReferenceNumber = cursor;
            batches += 1;
        }
        Ok((checkpoint, batches))
    }

    /// Read the next available batch after validating volume, journal, and USN
    /// boundaries. The consumer publishes its updated projection before saving
    /// the returned checkpoint.
    pub fn read(
        &self,
        checkpoint: &Checkpoint,
        buffer_bytes: usize,
    ) -> io::Result<(Checkpoint, Vec<Record>)> {
        let journal = self.journal()?;
        let minimum = journal.FirstUsn.max(journal.LowestValidUsn);
        if !valid_checkpoint(
            checkpoint,
            &self.volume,
            journal.UsnJournalID,
            minimum,
            journal.NextUsn,
        ) {
            return Err(invalid("saved NTFS journal checkpoint requires a rebuild"));
        }
        let input = READ_USN_JOURNAL_DATA_V1 {
            StartUsn: checkpoint.next_usn,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 0,
            UsnJournalID: checkpoint.journal_id,
            MinMajorVersion: 2,
            MaxMajorVersion: 2,
        };
        let mut output = batch_buffer(buffer_bytes)?;
        let (cursor, records) =
            parse_batch(self.batch(FSCTL_READ_USN_JOURNAL, &input, &mut output)?)?;
        let next_usn = i64::try_from(cursor).map_err(|_| invalid("invalid NTFS journal cursor"))?;
        if next_usn < checkpoint.next_usn
            || (next_usn == checkpoint.next_usn && !records.is_empty())
        {
            return Err(invalid("NTFS journal cursor did not advance"));
        }
        Ok((
            Checkpoint {
                next_usn,
                ..checkpoint.clone()
            },
            records,
        ))
    }

    fn journal(&self) -> io::Result<USN_JOURNAL_DATA_V0> {
        let mut journal = USN_JOURNAL_DATA_V0::default();
        let mut bytes = 0;
        // The output is an SDK structure sized for the V0 ABI.
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0,
                FSCTL_QUERY_USN_JOURNAL,
                null(),
                0,
                (&mut journal as *mut USN_JOURNAL_DATA_V0).cast(),
                size_of_val(&journal) as u32,
                &mut bytes,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if (bytes as usize) < size_of_val(&journal) {
            return Err(invalid("truncated NTFS journal state"));
        }
        Ok(journal)
    }

    fn batch<'b, T>(&self, code: u32, input: &T, output: &'b mut [u64]) -> io::Result<&'b [u8]> {
        let mut bytes = 0;
        // The caller supplies the matching SDK input structure and an aligned
        // output buffer. The synchronous call does not retain either pointer.
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0,
                code,
                (input as *const T).cast(),
                size_of_val(input) as u32,
                output.as_mut_ptr().cast(),
                size_of_val(output) as u32,
                &mut bytes,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if bytes as usize > size_of_val(output) {
            return Err(invalid("invalid NTFS batch length"));
        }
        Ok(unsafe { std::slice::from_raw_parts(output.as_ptr().cast(), bytes as usize) })
    }
}

fn batch_buffer(bytes: usize) -> io::Result<Vec<u64>> {
    if !(1024..=16 * 1024 * 1024).contains(&bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NTFS batch size must be 1 KiB to 16 MiB",
        ));
    }
    Ok(vec![0; bytes.div_ceil(size_of::<u64>())])
}

fn valid_checkpoint(
    checkpoint: &Checkpoint,
    volume: &Volume,
    journal_id: u64,
    minimum: i64,
    maximum: i64,
) -> bool {
    volume.identity.as_ref() == Some(&checkpoint.volume_identity)
        && volume.serial == checkpoint.volume_serial
        && journal_id == checkpoint.journal_id
        && (minimum..=maximum).contains(&checkpoint.next_usn)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn parse_batch(bytes: &[u8]) -> io::Result<(u64, Vec<Record>)> {
    let (cursor, mut records) = bytes
        .split_first_chunk::<8>()
        .ok_or_else(|| invalid("missing NTFS batch cursor"))?;
    let mut output = Vec::new();
    while !records.is_empty() {
        let (header, _) = records
            .split_first_chunk::<60>()
            .ok_or_else(|| invalid("truncated USN record"))?;
        let len = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let version = u16::from_le_bytes(header[4..6].try_into().unwrap());
        let name_len = u16::from_le_bytes(header[56..58].try_into().unwrap()) as usize;
        let name_offset = u16::from_le_bytes(header[58..60].try_into().unwrap()) as usize;
        if version != 2
            || len < header.len()
            || !len.is_multiple_of(8)
            || name_len == 0
            || !name_len.is_multiple_of(2)
            || !name_offset.is_multiple_of(2)
            || name_offset < header.len()
            || name_offset + name_len > len
            || len > records.len()
        {
            return Err(invalid("invalid USN record version, length or name bounds"));
        }
        let units: Vec<_> = records[name_offset..name_offset + name_len]
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect();
        if units.iter().any(|&unit| {
            unit < 32 || u8::try_from(unit).is_ok_and(|byte| b"<>:\"/\\|?*".contains(&byte))
        }) {
            return Err(invalid("invalid NTFS entry name"));
        }
        output.push(Record {
            id: u64::from_le_bytes(header[8..16].try_into().unwrap()),
            parent_id: u64::from_le_bytes(header[16..24].try_into().unwrap()),
            usn: i64::from_le_bytes(header[24..32].try_into().unwrap()),
            reason: u32::from_le_bytes(header[40..44].try_into().unwrap()),
            attributes: u32::from_le_bytes(header[52..56].try_into().unwrap()),
            name: OsString::from_wide(&units),
        });
        records = &records[len..];
    }
    Ok((u64::from_le_bytes(*cursor), output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    #[test]
    fn batch_preserves_reference_sequence_bits_and_utf16() {
        let id = 0x1020_0000_0000_014a_u64;
        let parent = 0x3344_0000_0000_0005_u64;
        let utf16 = [0x4e88, 0x7b97, 0xd800];
        let mut bytes = vec![0; 8 + 72];
        bytes[0..8].copy_from_slice(&42_u64.to_le_bytes());
        bytes[8..12].copy_from_slice(&72_u32.to_le_bytes());
        bytes[12..14].copy_from_slice(&2_u16.to_le_bytes());
        bytes[16..24].copy_from_slice(&id.to_le_bytes());
        bytes[24..32].copy_from_slice(&parent.to_le_bytes());
        bytes[64..66].copy_from_slice(&6_u16.to_le_bytes());
        bytes[66..68].copy_from_slice(&60_u16.to_le_bytes());
        for (index, unit) in utf16.iter().enumerate() {
            bytes[68 + index * 2..70 + index * 2].copy_from_slice(&u16::to_le_bytes(*unit));
        }
        let (cursor, records) = parse_batch(&bytes).unwrap();
        assert_eq!(cursor, 42);
        assert_eq!(records[0].id, id);
        assert_eq!(records[0].parent_id, parent);
        assert_eq!(records[0].name.encode_wide().collect::<Vec<_>>(), utf16);
        for length in 9..bytes.len() {
            assert!(parse_batch(&bytes[..length]).is_err());
        }
        bytes[66..68].copy_from_slice(&70_u16.to_le_bytes());
        assert!(parse_batch(&bytes).is_err());
    }

    #[test]
    fn checkpoint_rejects_replaced_volumes_journals_and_expired_usns() {
        let volume = Volume {
            mount: "V:\\".into(),
            filesystem: "NTFS".into(),
            identity: Some("volume-guid".into()),
            serial: 7,
        };
        let mut saved = Checkpoint {
            volume_identity: "volume-guid".into(),
            volume_serial: 7,
            journal_id: 11,
            next_usn: 25,
        };
        assert!(valid_checkpoint(&saved, &volume, 11, 20, 30));
        assert!(!valid_checkpoint(&saved, &volume, 11, 26, 30));
        assert!(!valid_checkpoint(&saved, &volume, 11, 20, 24));
        assert!(!valid_checkpoint(&saved, &volume, 12, 20, 30));
        saved.volume_identity = "replacement-volume-guid".into();
        assert!(!valid_checkpoint(&saved, &volume, 11, 20, 30));
    }

    #[test]
    fn replay_moves_descendants_and_replaces_old_sequence_id() {
        use windows_sys::Win32::System::Ioctl::{
            USN_REASON_FILE_CREATE, USN_REASON_RENAME_NEW_NAME,
        };

        let record = |id, parent_id, usn, reason, attributes, name: &str| Record {
            id,
            parent_id,
            usn,
            reason,
            attributes,
            name: name.into(),
        };
        let root = 5;
        let directory = 11;
        let file = 0x1000_0000_0000_0012;
        let reused_file = 0x2000_0000_0000_0012;
        let mount = Path::new("V:\\");
        let mut ns = Namespace::default();
        let dir = FILE_ATTRIBUTE_DIRECTORY;
        ns.insert_enumerated(record(directory, root, 10, 0, dir, "old"));
        ns.insert_enumerated(record(file, directory, 11, 0, 0, "予算.txt"));
        assert_eq!(ns.path(file, root, mount), Some(mount.join("old/予算.txt")));

        let rename = [
            record(directory, root, 12, USN_REASON_RENAME_OLD_NAME, dir, "old"),
            record(directory, root, 13, USN_REASON_RENAME_NEW_NAME, dir, "new"),
        ];
        ns.apply(rename.clone());
        ns.apply(rename);
        assert_eq!(ns.path(file, root, mount), Some(mount.join("new/予算.txt")));

        let replace = [
            record(file, directory, 14, USN_REASON_FILE_DELETE, 0, "予算.txt"),
            record(
                reused_file,
                directory,
                15,
                USN_REASON_FILE_CREATE,
                0,
                "予算.txt",
            ),
            record(
                reused_file,
                directory,
                16,
                USN_REASON_HARD_LINK_CHANGE,
                0,
                "予算.txt",
            ),
            record(directory, root, 17, USN_REASON_SECURITY_CHANGE, dir, "new"),
        ];
        ns.apply(replace.clone());
        ns.apply(replace);
        assert!(ns.path(file, root, mount).is_none());
        assert_eq!(
            ns.path(reused_file, root, mount),
            Some(mount.join("new/予算.txt"))
        );
        assert_eq!(
            ns.ids_needing_reconciliation(),
            &HashSet::from([directory, reused_file])
        );
    }

    #[test]
    #[ignore = "requires an elevated disposable NTFS volume with a change journal"]
    fn enumerate_disposable_volume_in_multiple_batches() {
        let volume = std::env::var_os("VELOCITY_DISPOSABLE_NTFS_VOLUME")
            .expect("set a disposable NTFS volume root");
        let reader = Reader::open(Path::new(&volume)).unwrap();
        let mut count = 0;
        let (checkpoint, batches) = reader
            .enumerate(1024, |_| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert!(batches > 1 && count > 1);
        let (updated, _) = reader.read(&checkpoint, 4096).unwrap();
        assert!(updated.next_usn >= checkpoint.next_usn);
    }
}
