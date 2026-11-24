#[cfg(windows)]
use std::io;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::sync::{oneshot, Notify};
use tracing::{debug, warn};

use crate::{FolderProvider, IndexProvider as _};

#[derive(Default)]
pub(crate) enum Pending {
    #[default]
    Clean,
    Paths(HashSet<PathBuf>),
    Scan,
    #[cfg(windows)]
    Journal,
}

#[cfg(windows)]
pub(crate) enum Notice {
    Registered,
    Paths(HashSet<PathBuf>),
    Scan(&'static str),
}

struct Signals {
    changed: Notify,
    dirty: Vec<Mutex<Pending>>,
    stop: AtomicBool,
}

impl Signals {
    fn flag(&self, index: usize, root: &Path, reason: &str) {
        debug!(root = %root.display(), reason, "Folder needs reconciliation");
        *self.dirty[index].lock().unwrap() = Pending::Scan;
        self.changed.notify_one();
    }

    #[cfg(windows)]
    fn paths(&self, index: usize, paths: HashSet<PathBuf>) {
        let mut pending = self.dirty[index].lock().unwrap();
        match &mut *pending {
            Pending::Clean => *pending = Pending::Paths(paths),
            Pending::Paths(previous) => previous.extend(paths),
            Pending::Scan | Pending::Journal => return,
        }
        if matches!(&*pending, Pending::Paths(paths) if paths.len() > 32) {
            *pending = Pending::Scan;
        }
        self.changed.notify_one();
    }
}

struct StopOnDrop(Arc<Signals>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
    }
}

pub(crate) async fn monitor(provider: Arc<FolderProvider>) {
    monitor_with(
        provider,
        #[cfg(windows)]
        Arc::new(crate::windows::watch),
    )
    .await;
}

#[cfg(windows)]
type Watch = dyn Fn(&Path, &AtomicBool, &mut dyn FnMut(Notice)) -> io::Result<()> + Send + Sync;

async fn monitor_with(provider: Arc<FolderProvider>, #[cfg(windows)] watcher: Arc<Watch>) {
    let signals = Arc::new(Signals {
        changed: Notify::new(),
        dirty: provider.roots.iter().map(|_| Mutex::default()).collect(),
        stop: AtomicBool::new(false),
    });
    let _stop = StopOnDrop(signals.clone());
    let mut started = Vec::new();
    let mut polling = Vec::new();
    for (index, root) in provider.roots.iter().cloned().enumerate() {
        let (ready, signal) = oneshot::channel();
        let signals = signals.clone();
        #[cfg(windows)]
        let watcher = watcher.clone();
        match std::thread::Builder::new()
            .name("velocity-folder-watch".into())
            .spawn(move || {
                watch(
                    root,
                    index,
                    signals,
                    ready,
                    #[cfg(windows)]
                    watcher.as_ref(),
                )
            }) {
            Ok(_) => started.push(signal),
            Err(error) => {
                warn!(%error, "Could not start a folder watcher. Polling this root");
                polling.push(index);
            }
        }
    }
    // Start the watches before scanning. Changes that arrive during the walk
    // remain dirty and trigger another reconciliation after publication.
    for signal in started {
        let _ = signal.await;
    }
    if let Err(error) = provider.initialize().await {
        warn!(%error, "Could not initialize the folder index");
    }
    let mut journal_tick = tokio::time::interval(Duration::from_millis(250));
    journal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut fallback_tick = tokio::time::interval(Duration::from_secs(2));
    fallback_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if provider.stop.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            _ = provider.stopped.notified() => break,
            _ = signals.changed.notified() => tokio::time::sleep(Duration::from_millis(80)).await,
            _ = journal_tick.tick(), if cfg!(windows) && provider.ntfs_batch_bytes.is_some() => {},
            _ = fallback_tick.tick(), if !polling.is_empty() => {
                for &index in &polling {
                    *signals.dirty[index].lock().unwrap() = Pending::Scan;
                }
            },
        }
        let dirty: Vec<_> = signals
            .dirty
            .iter()
            .map(|dirty| std::mem::take(&mut *dirty.lock().unwrap()))
            .collect();
        #[cfg(windows)]
        let mut dirty = dirty;
        #[cfg(windows)]
        if provider.ntfs_batch_bytes.is_some() {
            for (pending, root) in dirty.iter_mut().zip(provider.status().roots) {
                if root.ntfs.is_some() && matches!(pending, Pending::Clean) {
                    *pending = Pending::Journal;
                }
            }
        }
        if dirty.iter().any(|dirty| !matches!(dirty, Pending::Clean)) {
            if let Err(error) = provider.rebuild_roots(Some(dirty)).await {
                warn!(%error, "Could not reconcile changed folders");
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{RootState, SearchQuery};
    use std::{
        collections::BTreeSet,
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn overflow_during_creates_moves_and_deletes_converges_to_manifest() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("velocity-overflow-{}-{stamp}", std::process::id()));
        let old = root.join("original-directory");
        fs::create_dir_all(&old).unwrap();
        let keep = "long-descendant-filename.txt";
        fs::write(old.join(keep), "original").unwrap();
        let obsolete = root.join("obsolete-file-before-burst.txt");
        fs::write(&obsolete, "delete me").unwrap();

        let provider = Arc::new(FolderProvider::new(vec![root.clone()], &[]).unwrap());
        let overflow = Arc::new(AtomicBool::new(false));
        let observed = overflow.clone();
        let monitor = tokio::spawn(monitor_with(
            provider.clone(),
            Arc::new(move |root, stop, signal| {
                crate::windows::watch_with_buffer(root, stop, &mut [0; 4], &mut |notice| {
                    if matches!(notice, Notice::Scan("notification buffer overflow")) {
                        observed.store(true, Ordering::Release);
                    }
                    signal(notice);
                })
            }),
        ));
        let compare = async {
            while provider.status().roots[0].state != RootState::Ready {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let new = root.join("renamed-directory-after-burst");
            let incoming = root.join("new-file-after-overflow.txt");
            for index in 0..32 {
                fs::write(
                    root.join(format!("file-created-during-overflow-{index:02}.txt")),
                    "burst",
                )
                .unwrap();
            }
            fs::rename(&old, &new).unwrap();
            fs::remove_file(&obsolete).unwrap();
            fs::rename(root.join("file-created-during-overflow-00.txt"), &incoming).unwrap();
            fs::remove_file(root.join("file-created-during-overflow-01.txt")).unwrap();
            let mut manifest = BTreeSet::from([
                (new.clone(), true),
                (new.join(keep), false),
                (incoming, false),
            ]);
            manifest.extend((2..32).map(|index| {
                (
                    root.join(format!("file-created-during-overflow-{index:02}.txt")),
                    false,
                )
            }));
            loop {
                let page = provider
                    .search(SearchQuery {
                        text: String::new(),
                        offset: 0,
                        max_results: 100,
                    })
                    .await
                    .unwrap();
                let actual = page
                    .results
                    .into_iter()
                    .map(|row| (row.path, row.is_directory))
                    .collect();
                if overflow.load(Ordering::Acquire) && manifest == actual {
                    assert_eq!(page.total_count, Some(manifest.len()));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let result = tokio::time::timeout(Duration::from_secs(10), compare).await;
        provider.shutdown().await.unwrap();
        monitor.await.unwrap();
        fs::remove_dir_all(root).unwrap();
        result.expect("the real notification overflow must trigger a matching snapshot");
    }
}

fn watch(
    root: PathBuf,
    index: usize,
    signals: Arc<Signals>,
    ready: oneshot::Sender<()>,
    #[cfg(windows)] watcher: &Watch,
) {
    let mut ready = Some(ready);
    let mut reopen = false;
    while !signals.stop.load(Ordering::Acquire) {
        #[cfg(windows)]
        let result = {
            let mut registered = false;
            watcher(&root, &signals.stop, &mut |notice| {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
                if !registered && reopen {
                    signals.flag(index, &root, "watch reconnected");
                }
                registered = true;
                match notice {
                    Notice::Registered => {}
                    Notice::Paths(paths) => signals.paths(index, paths),
                    Notice::Scan(reason) => signals.flag(index, &root, reason),
                }
            })
        };
        #[cfg(not(windows))]
        let result = Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native folder notifications require Windows",
        ));
        if signals.stop.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = result {
            if let Some(ready) = ready.take() {
                let _ = ready.send(());
            }
            if !reopen {
                debug!(root = %root.display(), %error, "Folder watch unavailable. Polling for reconciliation");
            }
            signals.flag(index, &root, "watch unavailable");
        }
        reopen = true;
        // Do not spin when a share is offline or notifications are unsupported.
        std::thread::sleep(Duration::from_secs(2));
    }
}
