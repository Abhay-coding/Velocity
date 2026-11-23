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
    let signals = Arc::new(Signals {
        changed: Notify::new(),
        dirty: provider.roots.iter().map(|_| Mutex::default()).collect(),
        stop: AtomicBool::new(false),
    });
    let _stop = StopOnDrop(signals.clone());
    let mut started = Vec::new();
    for (index, root) in provider.roots.iter().cloned().enumerate() {
        let (ready, signal) = oneshot::channel();
        let signals = signals.clone();
        match std::thread::Builder::new()
            .name("velocity-folder-watch".into())
            .spawn(move || watch(root, index, signals, ready))
        {
            Ok(_) => started.push(signal),
            Err(error) => warn!(%error, "Could not start a folder watcher"),
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
    loop {
        if provider.stop.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            _ = provider.stopped.notified() => break,
            _ = signals.changed.notified() => tokio::time::sleep(Duration::from_millis(80)).await,
            _ = journal_tick.tick(), if cfg!(windows) && provider.ntfs_batch_bytes.is_some() => {},
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

fn watch(root: PathBuf, index: usize, signals: Arc<Signals>, ready: oneshot::Sender<()>) {
    let mut ready = Some(ready);
    let mut reopen = false;
    while !signals.stop.load(Ordering::Acquire) {
        #[cfg(windows)]
        let result = {
            let mut registered = false;
            crate::windows::watch(&root, &signals.stop, |notice| {
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
