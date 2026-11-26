use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

use super::{
    advance, invalid, make_entry, sort_links, Checkpoint, Exclusions, Reader, RootCoverage,
    ScopedLink, ScopedSnapshot,
};
use crate::{folder, windows, RootState};

#[cfg(test)]
mod tests;

const FORMAT_VERSION: u32 = 1;
const SCOPE_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const BINARY_MAGIC: [u8; 4] = *b"VSNP";

#[derive(Default)]
struct PendingWrites {
    roots: HashMap<PathBuf, RootCoverage>,
    worker: bool,
}

pub(crate) struct Cache {
    pub(crate) directory: PathBuf,
    pending: Mutex<PendingWrites>,
    write: Mutex<()>,
}

impl Cache {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            pending: Mutex::default(),
            write: Mutex::default(),
        }
    }

    pub(crate) fn save_now(&self, roots: Vec<RootCoverage>, exclusions: &Exclusions) {
        if roots.is_empty() {
            return;
        }
        // Both the worker and an explicit rebuild take this lock before
        // removing queued entries so an older write cannot land afterward.
        let _write = self.write.lock().unwrap();
        {
            let mut pending = self.pending.lock().unwrap();
            for root in &roots {
                pending.roots.remove(&root.root);
            }
        }
        self.write_roots(roots, exclusions);
    }

    pub(crate) fn schedule(
        self: &Arc<Self>,
        roots: Vec<RootCoverage>,
        exclusions: Arc<Exclusions>,
    ) {
        if roots.is_empty() {
            return;
        }
        let mut pending = self.pending.lock().unwrap();
        pending
            .roots
            .extend(roots.into_iter().map(|root| (root.root.clone(), root)));
        if pending.worker {
            return;
        }
        pending.worker = true;
        let cache = self.clone();
        let spawned = std::thread::Builder::new()
            .name("velocity-snapshot-write".into())
            .spawn(move || {
                loop {
                    // Keep one recent projection per root while changes arrive.
                    std::thread::sleep(Duration::from_millis(750));
                    let _write = cache.write.lock().unwrap();
                    let roots = {
                        let mut pending = cache.pending.lock().unwrap();
                        if pending.roots.is_empty() {
                            pending.worker = false;
                            return;
                        }
                        std::mem::take(&mut pending.roots)
                    };
                    cache.write_roots(roots.into_values(), &exclusions);
                }
            });
        if let Err(error) = spawned {
            pending.worker = false;
            tracing::debug!(%error, "Could not start the NTFS snapshot writer");
        }
    }

    pub(crate) fn cancel_pending(&self) {
        self.pending.lock().unwrap().roots.clear();
    }

    fn write_roots(&self, roots: impl IntoIterator<Item = RootCoverage>, exclusions: &Exclusions) {
        for root in roots {
            if let Err(error) = save(&self.directory, &root, exclusions) {
                tracing::debug!(root = %root.root.display(), %error, "Could not save NTFS snapshot");
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Scope {
    version: u32,
    root: Vec<u16>,
    exclusions: Vec<String>,
    cache_exclusion: Option<String>,
}

impl Scope {
    fn new(root: &Path, exclusions: &Exclusions) -> Self {
        let (exclusions, cache_exclusion) = exclusions.scope();
        Self {
            version: SCOPE_VERSION,
            root: root.as_os_str().encode_wide().collect(),
            exclusions,
            cache_exclusion,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct FileLink {
    id: u64,
    parent_id: u64,
    parent: usize,
    name: Vec<u16>,
    directory: bool,
}

#[derive(Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    scope: Scope,
    checkpoint: Checkpoint,
    enumeration_start_usn: i64,
    enumeration_batches: usize,
    parents: Vec<Vec<u16>>,
    links: Vec<FileLink>,
}

impl SnapshotFile {
    fn new(root: &Path, exclusions: &Exclusions, snapshot: &ScopedSnapshot) -> io::Result<Self> {
        let mut parents = Vec::new();
        let mut parent_ids = HashMap::new();
        let mut links = Vec::with_capacity(snapshot.links.len());
        for link in snapshot.links.iter() {
            let parent = link
                .entry
                .parent
                .strip_prefix(root)
                .map_err(io::Error::other)?;
            let parent = *parent_ids.entry(parent).or_insert_with(|| {
                parents.push(parent.as_os_str().encode_wide().collect());
                parents.len() - 1
            });
            links.push(FileLink {
                id: link.id,
                parent_id: link.parent_id,
                parent,
                name: link.entry.name.encode_wide().collect(),
                directory: link.entry.is_directory,
            });
        }
        Ok(Self {
            version: FORMAT_VERSION,
            scope: Scope::new(root, exclusions),
            checkpoint: snapshot.checkpoint.clone(),
            enumeration_start_usn: snapshot.enumeration_start_usn,
            enumeration_batches: snapshot.enumeration_batches,
            parents,
            links,
        })
    }

    fn into_snapshot(self, root: &Path, exclusions: &Exclusions) -> io::Result<ScopedSnapshot> {
        if self.version != FORMAT_VERSION
            || self.scope != Scope::new(root, exclusions)
            || self.enumeration_start_usn < 0
            || self.enumeration_start_usn > self.checkpoint.next_usn
        {
            return Err(invalid("NTFS snapshot version or scope changed"));
        }
        let mut parents = Vec::with_capacity(self.parents.len());
        for wide in self.parents {
            let path = PathBuf::from(OsString::from_wide(&wide));
            if wide.contains(&0)
                || path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err(invalid("invalid NTFS snapshot parent path"));
            }
            parents.push(Arc::<Path>::from(root.join(path)));
        }
        let mut links = Vec::with_capacity(self.links.len());
        for link in self.links {
            let parent = parents
                .get(link.parent)
                .ok_or_else(|| invalid("unknown NTFS snapshot parent"))?;
            let name = OsString::from_wide(&link.name);
            if link.name.contains(&0) || Path::new(&name).file_name() != Some(&name) {
                return Err(invalid("invalid NTFS snapshot name"));
            }
            links.push(ScopedLink {
                id: link.id,
                parent_id: link.parent_id,
                entry: make_entry(parent, name, link.directory),
            });
        }
        sort_links(&mut links);
        Ok(ScopedSnapshot {
            checkpoint: self.checkpoint,
            enumeration_start_usn: self.enumeration_start_usn,
            enumeration_batches: self.enumeration_batches,
            hard_link_aliases: super::alias_count(&links),
            links: Arc::new(links),
        })
    }
}

pub(crate) fn restore(
    cache: &Path,
    root: &Path,
    exclusions: &Exclusions,
    batch_bytes: usize,
    visited: &HashSet<PathBuf>,
    cancelled: impl Fn() -> bool,
) -> io::Result<Option<(RootCoverage, bool)>> {
    if !root.has_root() || root.parent().is_some() {
        return Ok(None);
    }
    let reader = Reader::open(root)?;
    let identity = reader
        .volume
        .identity
        .as_deref()
        .ok_or_else(|| invalid("missing NTFS volume identity"))?;
    let file = match File::open(cache_file(cache, identity)?) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut candidates = visited.clone();
    let Some((canonical, _)) = folder::open_root(root, exclusions, &mut candidates)? else {
        return Ok(None);
    };
    let started = Instant::now();
    let saved = read_file(file)?.into_snapshot(root, exclusions)?;
    let decode_ms = started.elapsed().as_millis();
    let mut coverage = RootCoverage::new(root.to_path_buf(), RootState::Ready);
    coverage.canonical = Some(canonical);
    coverage.volume = Some(reader.volume.clone());
    let (next, mut coverage) = advance(
        &reader,
        &saved,
        &coverage,
        exclusions,
        batch_bytes,
        cancelled,
        true,
    )?
    .ok_or_else(|| invalid("could not validate saved NTFS scope"))?;
    let changed = !next.same_projection(&saved);
    tracing::info!(root = %root.display(), decode_ms, restore_ms = started.elapsed().as_millis(), "Restored NTFS snapshot");
    coverage.ntfs = Some(Arc::new(next));
    coverage.ntfs_restored = true;
    Ok(Some((coverage, changed)))
}

pub(crate) fn save(
    cache: &Path,
    coverage: &RootCoverage,
    exclusions: &Exclusions,
) -> io::Result<()> {
    let Some(snapshot) = &coverage.ntfs else {
        return Ok(());
    };
    fs::create_dir_all(cache)?;
    let target = cache_file(cache, &snapshot.checkpoint.volume_identity)?;
    let data = SnapshotFile::new(&coverage.root, exclusions, snapshot)?;
    let pending = stage_write(cache, &data)?;
    replace(&pending.0, &target)
}

fn cache_file(cache: &Path, identity: &str) -> io::Result<PathBuf> {
    let guid = identity
        .strip_prefix(r"\\?\Volume{")
        .and_then(|value| value.strip_suffix("}\\"))
        .filter(|value| {
            value.len() == 36
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        })
        .ok_or_else(|| invalid("invalid NTFS volume identity"))?;
    Ok(cache.join(format!("{}.snapshot", guid.to_ascii_lowercase())))
}

struct StagedFile(PathBuf);

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn stage_write(cache: &Path, data: &SnapshotFile) -> io::Result<StagedFile> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = cache.join(format!(
        "pending-{}-{time}-{sequence}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let pending = StagedFile(path);
    let result = write_file(&mut file, data);
    drop(file);
    result?;
    Ok(pending)
}

fn write_file(file: &mut File, data: &SnapshotFile) -> io::Result<()> {
    file.write_all(&BINARY_MAGIC)?;
    file.write_all(&[0; 4])?;
    let checksum = {
        let mut output = Checksummed {
            out: BufWriter::new(&mut *file),
            crc: crc32fast::Hasher::new(),
        };
        bincode::serde::encode_into_std_write(data, &mut output, bincode::config::standard())
            .map_err(io::Error::other)?;
        output.flush()?;
        std::mem::take(&mut output.crc).finalize()
    };
    if file.stream_position()? > MAX_FILE_BYTES {
        return Err(invalid("NTFS snapshot is too large"));
    }
    file.seek(SeekFrom::Start(BINARY_MAGIC.len() as u64))?;
    file.write_all(&checksum.to_le_bytes())?;
    file.sync_all()
}

struct Checksummed<W> {
    out: W,
    crc: crc32fast::Hasher,
}

impl<W: Write> Write for Checksummed<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.out.write(bytes)?;
        self.crc.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

fn read_file(file: File) -> io::Result<SnapshotFile> {
    if file.metadata()?.len() > MAX_FILE_BYTES {
        return Err(invalid("NTFS snapshot is too large"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid("NTFS snapshot is too large"));
    }
    let (binary, contents) = bytes
        .strip_prefix(&BINARY_MAGIC)
        .map_or((false, bytes.as_slice()), |contents| (true, contents));
    let (checksum, payload) = contents
        .split_first_chunk::<4>()
        .ok_or_else(|| invalid("truncated NTFS snapshot"))?;
    if crc32fast::hash(payload) != u32::from_le_bytes(*checksum) {
        return Err(invalid("NTFS snapshot checksum mismatch"));
    }
    if binary {
        let config = bincode::config::standard().with_limit::<{ MAX_FILE_BYTES as usize }>();
        let (value, used) =
            bincode::serde::decode_from_slice(payload, config).map_err(io::Error::other)?;
        if used != payload.len() {
            return Err(invalid("unexpected data after the NTFS snapshot"));
        }
        Ok(value)
    } else {
        // M9 used a checksum followed by a JSON payload.
        serde_json::from_slice(payload).map_err(io::Error::other)
    }
}

fn replace(pending: &Path, target: &Path) -> io::Result<()> {
    let pending = windows::wide(pending.as_os_str());
    let target = windows::wide(target.as_os_str());
    // Both names are in the same directory. The durable temporary file is
    // published in one rename, so a crash leaves the old or new complete file.
    if unsafe {
        MoveFileExW(
            pending.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
