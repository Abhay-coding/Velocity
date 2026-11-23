use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs::{self, FileType, Metadata},
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use glob::{MatchOptions, Pattern};

use crate::{Entry, RootCoverage, RootState};

pub(crate) struct Exclusions(Vec<Pattern>);

impl Exclusions {
    pub(crate) fn new(patterns: &[String]) -> Result<Self, glob::PatternError> {
        patterns
            .iter()
            .map(|pattern| Pattern::new(&pattern.replace('\\', "/")))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub(crate) fn matches(&self, path: &Path, is_directory: bool) -> bool {
        let normalized = path.to_string_lossy().replace('\\', "/");
        let options = MatchOptions {
            case_sensitive: false,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        self.0.iter().any(|pattern| {
            pattern.matches_with(&normalized, options)
                || (is_directory && pattern.matches_with(&format!("{normalized}/"), options))
        })
    }

    fn excludes_root(&self, path: &Path) -> bool {
        path.ancestors()
            .any(|ancestor| self.matches(ancestor, true))
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn skip_entry(file_type: FileType, path: &Path) -> bool {
    // Windows reports junctions as symlinks. Check directory attributes too,
    // so other directory reparse points cannot lead out of scope.
    file_type.is_symlink()
        || (file_type.is_dir()
            && fs::symlink_metadata(path).map_or(true, |metadata| is_reparse_point(&metadata)))
}

pub(crate) fn make_entry(parent: &Arc<Path>, name: OsString, is_directory: bool) -> Arc<Entry> {
    let name_key = name.to_string_lossy().to_lowercase();
    let path_key = path_key(&parent.join(&name));
    Arc::new(Entry {
        parent: parent.clone(),
        name,
        name_key,
        path_key,
        is_directory,
    })
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Apply small batches of ordinary file changes. Directory changes, missed
/// events and access or reparse changes go through the full scoped walk.
pub(crate) fn reconcile_files(
    root: &Path,
    exclusions: &Exclusions,
    previous: &[Arc<Entry>],
    paths: &HashSet<PathBuf>,
    cancelled: impl Fn() -> bool,
) -> Option<Vec<Arc<Entry>>> {
    let mut replacements = HashMap::new();
    for relative in paths {
        if cancelled() {
            return None;
        }
        let path = root.join(relative);
        let parent = path.parent()?;
        for ancestor in parent
            .ancestors()
            .take_while(|ancestor| ancestor.starts_with(root))
        {
            let metadata = fs::symlink_metadata(ancestor).ok()?;
            if !metadata.is_dir()
                || is_reparse_point(&metadata)
                || exclusions.matches(ancestor, true)
            {
                return None;
            }
            fs::read_dir(ancestor).ok()?;
        }
        let replacement = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !is_reparse_point(&metadata) => {
                if exclusions.matches(&path, false) {
                    return None;
                }
                // Read the actual entry name. This preserves case-only renames
                // and avoids publishing an alias from a notification.
                let expected = path_key(&path);
                let mut matches = fs::read_dir(parent)
                    .ok()?
                    .filter_map(Result::ok)
                    .filter(|entry| path_key(&entry.path()) == expected);
                let entry = matches.next()?;
                if matches.next().is_some() {
                    return None;
                }
                Some(make_entry(&Arc::from(parent), entry.file_name(), false))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            _ => return None,
        };
        if replacements.insert(path_key(&path), replacement).is_some() {
            return None;
        }
    }
    let mut matched = HashSet::new();
    let mut updated = Vec::with_capacity(previous.len().saturating_add(replacements.len()));
    for (index, entry) in previous.iter().enumerate() {
        if index.is_multiple_of(4096) && cancelled() {
            return None;
        }
        if replacements.contains_key(&entry.path_key) {
            if entry.is_directory || !matched.insert(entry.path_key.as_str()) {
                return None;
            }
        } else {
            updated.push(entry.clone());
        }
    }
    for entry in replacements.into_values().flatten() {
        let position = updated
            .binary_search_by(|other| crate::compare_entries(other, &entry))
            .unwrap_or_else(|position| position);
        updated.insert(position, entry);
    }
    Some(updated)
}

pub(crate) fn scan(
    root: PathBuf,
    exclusions: &Exclusions,
    visited: &mut HashSet<PathBuf>,
    entries: &mut Vec<Arc<Entry>>,
    #[cfg_attr(not(windows), allow(unused_variables))] ntfs_batch_bytes: Option<usize>,
    cancelled: impl Fn() -> bool,
    mut progress: impl FnMut(u64),
) -> RootCoverage {
    let mut coverage = RootCoverage::new(root.clone(), RootState::Ready);
    let root_entries = match open_root(&root, exclusions, visited) {
        Ok(Some((canonical, entries))) => {
            coverage.canonical = Some(canonical);
            entries
        }
        Ok(None) => {
            coverage.state = RootState::Skipped;
            return coverage;
        }
        Err(error) => {
            coverage.state = if error.kind() == io::ErrorKind::PermissionDenied {
                RootState::AccessDenied
            } else {
                RootState::Unavailable
            };
            coverage.detail = Some(error.to_string());
            return coverage;
        }
    };
    #[cfg(windows)]
    {
        coverage.volume = crate::windows::volume(&root);
        if let Some(batch_bytes) = ntfs_batch_bytes.filter(|_| {
            coverage.volume.as_ref().is_some_and(|volume| {
                volume
                    .mount
                    .as_os_str()
                    .eq_ignore_ascii_case(root.as_os_str())
                    && volume.filesystem.eq_ignore_ascii_case("NTFS")
            })
        }) {
            let mut projected = coverage.clone();
            let canonical = coverage.canonical.as_deref().unwrap();
            match crate::ntfs::capture(
                &root,
                canonical,
                exclusions,
                &mut projected,
                batch_bytes,
                &cancelled,
                &mut progress,
            ) {
                Ok(snapshot) => {
                    tracing::info!(root = %root.display(), entries = projected.entries, batches = snapshot.enumeration_batches,
                        aliases = snapshot.hard_link_aliases, source = "ntfs", "Built scoped NTFS snapshot");
                    entries.extend(snapshot.entries());
                    projected.ntfs = Some(Arc::new(snapshot));
                    return projected;
                }
                Err(_) if cancelled() => return coverage,
                Err(error) => {
                    tracing::debug!(root = %root.display(), %error, "Using folder indexing for NTFS root");
                    coverage.detail = Some(error.to_string());
                    progress(0);
                }
            }
        }
    }
    let mut pending = vec![(Arc::<Path>::from(root), root_entries)];
    while let Some((parent, children)) = pending.pop() {
        for child in children {
            if cancelled() {
                return coverage;
            }
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    coverage.skip(&parent, &error.to_string());
                    continue;
                }
            };
            let path = child.path();
            let file_type = match child.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    coverage.skip(&path, &error.to_string());
                    continue;
                }
            };
            if exclusions.matches(&path, file_type.is_dir()) {
                coverage.skip(&path, "excluded");
                continue;
            }
            if skip_entry(file_type, &path) {
                coverage.skip(&path, "reparse point or unreadable directory");
                continue;
            }
            // Only publish a directory after confirming that this user can list it.
            if file_type.is_dir() {
                match fs::read_dir(&path) {
                    Ok(children) => pending.push((Arc::from(path), children)),
                    Err(error) => {
                        coverage.skip(&path, &error.to_string());
                        continue;
                    }
                }
            }
            entries.push(make_entry(&parent, child.file_name(), file_type.is_dir()));
            coverage.entries += 1;
            if coverage.entries.is_multiple_of(256) {
                progress(coverage.entries);
            }
        }
    }
    coverage
}

fn open_root(
    root: &Path,
    exclusions: &Exclusions,
    visited: &mut HashSet<PathBuf>,
) -> io::Result<Option<(PathBuf, fs::ReadDir)>> {
    if exclusions.excludes_root(root) || is_reparse_point(&fs::symlink_metadata(root)?) {
        return Ok(None);
    }
    let canonical = fs::canonicalize(root)?;
    if exclusions.excludes_root(&canonical) {
        return Ok(None);
    }
    let entries = fs::read_dir(root)?;
    if visited.iter().any(|parent| canonical.starts_with(parent)) {
        return Ok(None);
    }
    visited.insert(canonical.clone());
    Ok(Some((canonical, entries)))
}
