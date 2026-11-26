mod folder;
mod monitor;
#[cfg(windows)]
pub mod ntfs;
mod query;
#[cfg(windows)]
mod windows;

use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use tracing::{debug, info};
use velocity_core::{
    index_provider::{
        async_trait, IndexProvider, IndexStats, IndexingProgress, ProviderCapabilities,
        SearchQuery, SearchResult, SearchResults,
    },
    VelocityError, VelocityResult,
};

use folder::Exclusions;
use monitor::Pending;
use query::Term;

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    parent: Arc<Path>,
    name: OsString,
    name_key: String,
    path_key: String,
    is_directory: bool,
}

impl Entry {
    fn result(&self) -> SearchResult {
        let path = self.parent.join(&self.name);
        let extension = (!self.is_directory)
            .then(|| Path::new(&self.name).extension())
            .flatten()
            .map(|ext| ext.to_string_lossy().to_lowercase());
        SearchResult {
            name: self.name.to_string_lossy().into_owned(),
            path,
            size: None,
            modified: None,
            extension,
            is_directory: self.is_directory,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootState {
    Scanning,
    Ready,
    AccessDenied,
    Unavailable,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootCoverage {
    pub root: PathBuf,
    pub state: RootState,
    pub entries: u64,
    pub skipped: u64,
    pub examples: Vec<(PathBuf, String)>,
    pub detail: Option<String>,
    pub volume: Option<Volume>,
    #[cfg(windows)]
    pub ntfs: Option<Arc<ntfs::ScopedSnapshot>>,
    #[cfg(windows)]
    pub ntfs_restored: bool,
    canonical: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub mount: PathBuf,
    pub filesystem: String,
    pub identity: Option<String>,
    pub serial: u32,
}

impl RootCoverage {
    fn new(root: PathBuf, state: RootState) -> Self {
        Self {
            root,
            state,
            entries: 0,
            skipped: 0,
            examples: Vec::new(),
            detail: None,
            volume: None,
            #[cfg(windows)]
            ntfs: None,
            #[cfg(windows)]
            ntfs_restored: false,
            canonical: None,
        }
    }

    fn skip(&mut self, path: &Path, reason: &str) {
        self.skipped += 1;
        if self.examples.len() < 8 {
            self.examples.push((path.to_path_buf(), reason.into()));
        }
        debug!(path = %path.display(), reason, "Skipped index location");
    }

    fn same_coverage(&self, other: &Self) -> bool {
        #[cfg(windows)]
        if let (Some(current), Some(previous)) = (&self.ntfs, &other.ntfs) {
            if !current.same_projection(previous) {
                return false;
            }
            // The checkpoint can advance even when the searchable projection
            // is unchanged. Avoid refreshing the UI for that alone.
            return Self {
                ntfs: other.ntfs.clone(),
                ..self.clone()
            } == *other;
        }
        self == other
    }
}

#[derive(Clone, Debug, Default)]
pub struct NativeStatus {
    pub revision: u64,
    pub scanning: bool,
    pub paused: bool,
    pub roots: Vec<RootCoverage>,
}

#[derive(Default)]
struct State {
    status: NativeStatus,
    entries: Arc<Vec<Arc<Entry>>>,
    by_root: Vec<Arc<Vec<Arc<Entry>>>>,
    published_roots: Vec<RootCoverage>,
    last_updated: Option<u64>,
    searchable: bool,
}

pub struct FolderProvider {
    roots: Arc<Vec<PathBuf>>,
    exclusions: Arc<Exclusions>,
    state: Arc<RwLock<State>>,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    pause_epoch: Arc<AtomicU64>,
    rebuild: Arc<tokio::sync::Mutex<()>>,
    stopped: tokio::sync::Notify,
    ntfs_batch_bytes: Option<usize>,
    #[cfg(windows)]
    snapshot_cache: Option<PathBuf>,
}

impl FolderProvider {
    pub fn new(mut roots: Vec<PathBuf>, exclusions: &[String]) -> VelocityResult<Self> {
        if roots.iter().any(|path| !path.is_absolute()) {
            return Err(VelocityError::Config(
                "Search roots must be absolute paths".into(),
            ));
        }
        roots.sort_unstable_by(|a, b| {
            a.components()
                .count()
                .cmp(&b.components().count())
                .then_with(|| a.cmp(b))
        });
        roots.dedup();
        let exclusions =
            Exclusions::new(exclusions).map_err(|err| VelocityError::Config(err.to_string()))?;
        let coverage: Vec<_> = roots
            .iter()
            .map(|root| RootCoverage::new(root.clone(), RootState::Scanning))
            .collect();
        let state = State {
            status: NativeStatus {
                roots: coverage.clone(),
                ..NativeStatus::default()
            },
            published_roots: coverage,
            by_root: vec![Arc::default(); roots.len()],
            ..State::default()
        };
        Ok(Self {
            roots: Arc::new(roots),
            exclusions: Arc::new(exclusions),
            state: Arc::new(RwLock::new(state)),
            stop: Arc::default(),
            paused: Arc::default(),
            pause_epoch: Arc::default(),
            rebuild: Arc::default(),
            stopped: tokio::sync::Notify::new(),
            ntfs_batch_bytes: None,
            #[cfg(windows)]
            snapshot_cache: None,
        })
    }

    /// Request NTFS snapshots for configured volume roots. Folder indexing
    /// remains available when the current Windows token cannot open a volume.
    #[cfg(windows)]
    pub fn with_ntfs_snapshots(mut self, batch_bytes: usize) -> Self {
        self.ntfs_batch_bytes = Some(batch_bytes);
        self
    }

    /// Save and restore NTFS snapshots in a private per-user directory.
    #[cfg(windows)]
    pub fn with_snapshot_cache(mut self, directory: PathBuf) -> Self {
        if directory.is_absolute() && directory.parent().is_some() {
            Arc::make_mut(&mut self.exclusions).exclude_cache(&directory);
            self.snapshot_cache = Some(directory);
        }
        self
    }

    pub fn status(&self) -> NativeStatus {
        self.state.read().unwrap().status.clone()
    }

    pub fn start_monitor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(monitor::monitor(self.clone()))
    }

    async fn rebuild_roots(
        &self,
        dirty: Option<Vec<Pending>>,
        #[cfg_attr(not(windows), allow(unused_variables))] startup: bool,
    ) -> VelocityResult<()> {
        let guard = self.rebuild.clone().lock_owned().await;
        if self.stop.load(Ordering::Acquire) || self.paused.load(Ordering::Acquire) {
            return Ok(());
        }
        let (roots, exclusions, state, stop, epoch) = (
            self.roots.clone(),
            self.exclusions.clone(),
            self.state.clone(),
            self.stop.clone(),
            self.pause_epoch.clone(),
        );
        let starting_epoch = epoch.load(Ordering::Acquire);
        let ntfs_batch_bytes = self.ntfs_batch_bytes;
        #[cfg(windows)]
        let snapshot_cache = self.snapshot_cache.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let cancelled =
                || stop.load(Ordering::Acquire) || epoch.load(Ordering::Acquire) != starting_epoch;
            let started = Instant::now();
            let (mut coverage, mut by_root) = {
                let state = state.read().unwrap();
                (state.published_roots.clone(), state.by_root.clone())
            };
            let mut visited = HashSet::new();
            let mut changed = false;
            #[cfg(windows)]
            let mut failed_journal_roots = Vec::<&Path>::new();
            #[cfg(windows)]
            let mut save = HashSet::new();
            #[cfg(windows)]
            let mut restored_indices = HashSet::new();
            #[cfg(windows)]
            if let (true, Some(cache), Some(batch_bytes)) = (startup, snapshot_cache.as_deref(), ntfs_batch_bytes) {
                let mut restored = HashSet::new();
                if coverage.iter().all(|root| root.state == RootState::Scanning) {
                    for (index, root) in roots.iter().enumerate() {
                        if cancelled() { break; }
                        match ntfs::disk::restore(cache, root, &exclusions, batch_bytes, &restored, cancelled) {
                            Ok(Some((projected, needs_save))) => {
                                let mut found: Vec<_> = projected.ntfs.as_ref().unwrap().entries().collect();
                                found.sort_unstable_by(compare_entries);
                                by_root[index] = Arc::new(found);
                                restored.extend(projected.canonical.iter().cloned());
                                coverage[index] = projected;
                                restored_indices.insert(index);
                                if needs_save { save.insert(index); }
                            }
                            Ok(None) => {}
                            Err(error) => debug!(root = %root.display(), %error, "Could not restore NTFS snapshot"),
                        }
                    }
                }
                if !restored.is_empty() && !cancelled() {
                    // A valid volume is searchable before we enumerate any
                    // other volume whose snapshot could not be restored.
                    publish(&state, coverage.clone(), by_root.clone(), true, true, cancelled, started);
                }
            }
            for (index, root) in roots.iter().enumerate() {
                if cancelled() {
                    break;
                }
                #[cfg(windows)]
                if restored_indices.contains(&index) {
                    visited.extend(coverage[index].canonical.iter().cloned());
                    continue;
                }
                if let Some(dirty) = &dirty {
                    // Rescan descendants of changed roots in case a formerly
                    // redundant root now provides independent coverage.
                    let affected = roots.iter().zip(dirty).any(|(other, dirty)| {
                        #[cfg(windows)]
                        if matches!(dirty, Pending::Journal) {
                            return root == other;
                        }
                        !matches!(dirty, Pending::Clean) && (root.starts_with(other) || other.starts_with(root))
                    });
                    #[cfg(windows)]
                    let affected = affected || failed_journal_roots.iter().any(|parent| root.starts_with(parent));
                    if !affected {
                        if let Some(path) = &coverage[index].canonical {
                            visited.insert(path.clone());
                        }
                        continue;
                    }
                    #[cfg(windows)]
                    if let (Some(batch_bytes), Some(snapshot)) = (ntfs_batch_bytes, &coverage[index].ntfs) {
                        match ntfs::tail(snapshot, &coverage[index], &exclusions, batch_bytes, cancelled) {
                            Ok(updated) => {
                                if let Some((next, mut projected)) = updated {
                                    if !next.same_projection(snapshot) {
                                        save.insert(index);
                                        let mut found: Vec<_> = next.entries().collect();
                                        found.sort_unstable_by(compare_entries);
                                        if *by_root[index] != found {
                                            by_root[index] = Arc::new(found);
                                            changed = true;
                                        }
                                    }
                                    projected.ntfs = Some(Arc::new(next));
                                    coverage[index] = projected;
                                }
                                if let Some(path) = &coverage[index].canonical {
                                    visited.insert(path.clone());
                                }
                                continue;
                            }
                            Err(_) if cancelled() => break,
                            Err(error) => {
                                failed_journal_roots.push(root);
                                debug!(root = %root.display(), %error, "Rebuilding after NTFS journal read failed");
                            }
                        }
                    }
                    if let Pending::Paths(paths) = &dirty[index] {
                        #[cfg(windows)]
                        let folder_source = coverage[index].ntfs.is_none();
                        #[cfg(not(windows))]
                        let folder_source = true;
                        let independent = roots.len() == 1 || roots.iter().enumerate().all(|(other_index, other)| {
                            index == other_index || (!root.starts_with(other) && !other.starts_with(root)
                                && coverage[other_index].state != RootState::Skipped)
                        });
                        if folder_source && independent && coverage[index].state == RootState::Ready && coverage[index].skipped == 0 {
                            if let Some(found) = folder::reconcile_files(root, &exclusions, &by_root[index], paths, cancelled) {
                                if let Some(path) = &coverage[index].canonical { visited.insert(path.clone()); }
                                coverage[index].entries = found.len() as u64;
                                if *by_root[index] != found {
                                    debug!(root = %root.display(), paths = paths.len(), "Updated changed files in the folder snapshot");
                                    by_root[index] = Arc::new(found);
                                    changed = true;
                                }
                                continue;
                            }
                        }
                    }
                }
                {
                    let mut state = state.write().unwrap();
                    state.status.scanning = true;
                    state.status.roots[index] = RootCoverage::new(root.clone(), RootState::Scanning);
                }
                let mut found = Vec::new();
                coverage[index] = folder::scan(
                    root.clone(),
                    &exclusions,
                    &mut visited,
                    &mut found,
                    ntfs_batch_bytes,
                    cancelled,
                    |count| {
                        state.write().unwrap().status.roots[index].entries = count;
                    },
                );
                if cancelled() {
                    break;
                }
                #[cfg(windows)]
                if coverage[index].ntfs.is_some() { save.insert(index); }
                found.sort_unstable_by(compare_entries);
                if *by_root[index] != found {
                    by_root[index] = Arc::new(found);
                    changed = true;
                }
            }
            #[cfg(windows)]
            let cache_writes = snapshot_cache.as_deref().map(|cache| {
                (cache, save.into_iter().map(|index| coverage[index].clone()).collect::<Vec<_>>())
            });
            publish(&state, coverage, by_root, changed, dirty.is_none(), cancelled, started);
            #[cfg(windows)]
            if let Some((cache, roots)) = cache_writes {
                for root in roots {
                    if cancelled() { break; }
                    if let Err(error) = ntfs::disk::save(cache, &root, &exclusions) {
                        debug!(root = %root.root.display(), %error, "Could not save NTFS snapshot");
                    }
                }
            }
        })
        .await
        .map_err(|err| VelocityError::IndexProvider(err.to_string()))
    }
}

fn publish(
    state: &RwLock<State>,
    coverage: Vec<RootCoverage>,
    by_root: Vec<Arc<Vec<Arc<Entry>>>>,
    changed: bool,
    force_revision: bool,
    cancelled: impl Fn() -> bool,
    started: Instant,
) {
    let merged = (changed && !cancelled()).then(|| {
        if let [entries] = by_root.as_slice() {
            return entries.clone();
        }
        let mut entries: Vec<_> = by_root
            .iter()
            .flat_map(|entries| entries.iter().cloned())
            .collect();
        entries.sort_unstable_by(compare_entries);
        entries.dedup();
        Arc::new(entries)
    });
    let mut state = state.write().unwrap();
    let scanning = coverage
        .iter()
        .any(|root| root.state == RootState::Scanning);
    state.status.scanning = scanning && !cancelled();
    if cancelled() {
        state.status.roots = state.published_roots.clone();
        return;
    }
    let coverage_changed = state
        .published_roots
        .iter()
        .zip(&coverage)
        .any(|(previous, next)| !previous.same_coverage(next));
    state.status.roots = coverage.clone();
    state.searchable = coverage.iter().any(|root| root.state == RootState::Ready);
    state.published_roots = coverage;
    let previous_snapshots = merged.map(|entries| {
        (
            std::mem::replace(&mut state.by_root, by_root),
            std::mem::replace(&mut state.entries, entries),
        )
    });
    if force_revision || changed || coverage_changed {
        info!(
            entries = state.entries.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "Published search snapshot"
        );
        state.last_updated = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|value| value.as_secs());
        state.status.revision = state.status.revision.wrapping_add(1);
    }
    drop(state);
    drop(previous_snapshots);
}

fn compare_entries(a: &Arc<Entry>, b: &Arc<Entry>) -> std::cmp::Ordering {
    a.name_key
        .cmp(&b.name_key)
        .then_with(|| a.path_key.cmp(&b.path_key))
        .then_with(|| a.parent.cmp(&b.parent))
        .then_with(|| a.name.cmp(&b.name))
}

#[async_trait]
impl IndexProvider for FolderProvider {
    async fn initialize(&self) -> VelocityResult<()> {
        self.rebuild_roots(None, true).await
    }

    async fn search(&self, query: SearchQuery) -> VelocityResult<SearchResults> {
        let started = Instant::now();
        let terms = Term::parse(&query.text)?;
        let entries = {
            let state = self.state.read().unwrap();
            if !state.searchable {
                return Err(VelocityError::IndexProvider(if state.status.scanning {
                    "Scanning folders. Results will appear when a snapshot is ready.".into()
                } else {
                    "No readable search root is available.".into()
                }));
            }
            Arc::clone(&state.entries)
        };
        let mut total_count = 0;
        let mut page = Vec::new();
        for chunk in entries.chunks(4096) {
            for entry in chunk {
                if !terms
                    .iter()
                    .all(|term| term.matches(&entry.name_key, &entry.path_key, entry.is_directory))
                {
                    continue;
                }
                if total_count >= query.offset && page.len() < query.max_results {
                    page.push(entry.result());
                }
                total_count += 1;
            }
            // Cooperate with task cancellation without blocking other async work.
            tokio::task::yield_now().await;
        }
        debug!(
            matched = total_count,
            page = page.len(),
            elapsed_us = started.elapsed().as_micros(),
            filesystem_reads = 0,
            "Native first page ready"
        );
        Ok(SearchResults {
            partial: query.offset.saturating_add(page.len()) < total_count,
            results: page,
            total_count: Some(total_count),
            query_time_ms: started.elapsed().as_millis() as u64,
        })
    }

    async fn stats(&self) -> VelocityResult<IndexStats> {
        let state = self.state.read().unwrap();
        let current = state
            .status
            .roots
            .iter()
            .find(|root| root.state == RootState::Scanning);
        Ok(IndexStats {
            total_files: state
                .entries
                .iter()
                .filter(|entry| !entry.is_directory)
                .count() as u64,
            total_size: 0,
            index_size_bytes: 0,
            last_updated: state.last_updated,
            drives_indexed: state
                .status
                .roots
                .iter()
                .filter(|root| root.state == RootState::Ready)
                .map(|root| root.root.display().to_string())
                .collect(),
            is_indexing: state.status.scanning,
            indexing_progress: current.map(|root| IndexingProgress {
                current_drive: root.root.display().to_string(),
                files_processed: root.entries,
                estimated_total: 0,
                current_path: None,
            }),
        })
    }

    async fn reindex(&self) -> VelocityResult<()> {
        self.rebuild_roots(None, false).await
    }

    async fn pause_indexing(&self) -> VelocityResult<()> {
        self.paused.store(true, Ordering::Release);
        self.pause_epoch.fetch_add(1, Ordering::AcqRel);
        self.state.write().unwrap().status.paused = true;
        Ok(())
    }

    async fn resume_indexing(&self) -> VelocityResult<()> {
        self.paused.store(false, Ordering::Release);
        self.state.write().unwrap().status.paused = false;
        self.reindex().await
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            name: "Native folder search".into(),
            supports_realtime: true,
            supports_operators: true,
            supports_content_search: false,
            supports_preview: false,
            requires_admin: false,
            version: env!("CARGO_PKG_VERSION").into(),
        }
    }

    async fn shutdown(&self) -> VelocityResult<()> {
        self.stop.store(true, Ordering::Release);
        self.stopped.notify_one();
        Ok(())
    }
}
