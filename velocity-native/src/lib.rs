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

#[derive(PartialEq, Eq)]
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
        })
    }

    pub fn status(&self) -> NativeStatus {
        self.state.read().unwrap().status.clone()
    }

    pub fn start_monitor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(monitor::monitor(self.clone()))
    }

    async fn rebuild_roots(&self, dirty: Option<Vec<Pending>>) -> VelocityResult<()> {
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
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let cancelled =
                || stop.load(Ordering::Acquire) || epoch.load(Ordering::Acquire) != starting_epoch;
            let started = Instant::now();
            let (mut coverage, mut by_root) = {
                let mut state = state.write().unwrap();
                state.status.scanning = true;
                (state.published_roots.clone(), state.by_root.clone())
            };
            let mut visited = HashSet::new();
            let mut changed = false;
            for (index, root) in roots.iter().enumerate() {
                if cancelled() {
                    break;
                }
                if let Some(dirty) = &dirty {
                    // Rescan descendants of changed roots in case a formerly
                    // redundant root now provides independent coverage.
                    let affected = roots.iter().zip(dirty).any(|(other, dirty)| {
                        !matches!(dirty, Pending::Clean) && (root.starts_with(other) || other.starts_with(root))
                    });
                    if !affected {
                        if let Some(path) = &coverage[index].canonical {
                            visited.insert(path.clone());
                        }
                        continue;
                    }
                    if let Pending::Paths(paths) = &dirty[index] {
                        let independent = roots.len() == 1 || roots.iter().enumerate().all(|(other_index, other)| {
                            index == other_index || (!root.starts_with(other) && !other.starts_with(root)
                                && coverage[other_index].state != RootState::Skipped)
                        });
                        if independent && coverage[index].state == RootState::Ready && coverage[index].skipped == 0 {
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
                state.write().unwrap().status.roots[index] =
                    RootCoverage::new(root.clone(), RootState::Scanning);
                let mut found = Vec::new();
                coverage[index] = folder::scan(
                    root.clone(),
                    &exclusions,
                    &mut visited,
                    &mut found,
                    cancelled,
                    |count| {
                        state.write().unwrap().status.roots[index].entries = count;
                    },
                );
                if cancelled() {
                    break;
                }
                found.sort_unstable_by(compare_entries);
                if *by_root[index] != found {
                    by_root[index] = Arc::new(found);
                    changed = true;
                }
            }
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
            state.status.scanning = false;
            if cancelled() {
                state.status.roots = state.published_roots.clone();
                return;
            }
            let coverage_changed = state.published_roots != coverage;
            state.status.roots = coverage.clone();
            state.searchable = coverage.iter().any(|root| root.state == RootState::Ready);
            state.published_roots = coverage;
            let previous_snapshots = merged.map(|entries| {
                (
                    std::mem::replace(&mut state.by_root, by_root),
                    std::mem::replace(&mut state.entries, entries),
                )
            });
            if dirty.is_none() || changed || coverage_changed {
                info!(
                    source = "folder",
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
        })
        .await
        .map_err(|err| VelocityError::IndexProvider(err.to_string()))
    }
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
        self.reindex().await
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
        self.rebuild_roots(None).await
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
