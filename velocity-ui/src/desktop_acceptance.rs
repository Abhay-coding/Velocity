use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use slint::{
    platform::{Key, WindowEvent},
    ComponentHandle as _, Model as _, Weak,
};
use tokio::{
    runtime::Runtime,
    sync::oneshot,
    task::JoinHandle,
    time::{sleep, timeout},
};
use velocity_core::{
    index_provider::SearchQuery, IndexProvider as _, VelocityError, VelocityResult,
};
use velocity_native::FolderProvider;

use crate::SearchBar;

const PREFIX: &str = "report";
const SUFFIX: &str = "-needle";
const EXPECTED: &str = "report-needle.txt";

#[derive(Default)]
struct Observation {
    started: Option<Instant>,
    first_frame: Option<(Duration, bool, bool)>,
    painted_generation: Option<i32>,
    stale_frames: usize,
}

pub(crate) fn start_ntfs(
    ui: &SearchBar,
    runtime: &Runtime,
) -> Option<JoinHandle<VelocityResult<()>>> {
    let root = PathBuf::from(std::env::var_os("VELOCITY_NTFS_DESKTOP_FIXTURE")?);
    let weak = ui.as_weak();
    Some(runtime.spawn(async move {
        let result = verify_ntfs(&weak, &root).await;
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
        result
    }))
}

async fn verify_ntfs(ui: &Weak<SearchBar>, root: &Path) -> VelocityResult<()> {
    let before = root.join("before");
    let after = root.join("after");
    let old = before.join("desktop-journal-old.txt");
    timeout(Duration::from_secs(20), async {
        while !on_ui(ui, |ui| ui.get_coverage().contains("NTFS snapshot")).await? {
            sleep(Duration::from_millis(20)).await;
        }
        on_ui(ui, |ui| {
            ui.invoke_focus_search();
            for ch in "desktop-journal".chars() {
                key(&ui, ch);
            }
        })
        .await
    })
    .await
    .map_err(|_| failure("The desktop did not publish its NTFS snapshot"))??;
    let revision = wait_for_row(ui, old, None).await?;
    fs::rename(before, &after).map_err(|error| failure(&error.to_string()))?;
    let old_name = after.join("desktop-journal-old.txt");
    let revision = wait_for_row(ui, old_name.clone(), Some(revision)).await?;
    let new_name = after.join("desktop-journal-new.txt");
    fs::rename(old_name, &new_name).map_err(|error| failure(&error.to_string()))?;
    wait_for_row(ui, new_name, Some(revision)).await?;
    println!("NTFS desktop query refreshed after both directory and file renames");
    Ok(())
}

async fn wait_for_row(
    ui: &Weak<SearchBar>,
    expected: PathBuf,
    previous: Option<i32>,
) -> VelocityResult<i32> {
    let expected = Arc::new(expected);
    timeout(Duration::from_secs(20), async {
        loop {
            let expected = expected.clone();
            let generation = on_ui(ui, move |ui| {
                let rows = ui.get_search_results();
                let generation = ui.get_search_generation();
                (!ui.get_searching()
                    && previous != Some(generation)
                    && rows.row_count() == 1
                    && rows
                        .row_data(0)
                        .is_some_and(|row| Path::new(row.path.as_str()) == expected.as_path()))
                .then_some(generation)
            })
            .await?;
            if let Some(generation) = generation {
                return Ok(generation);
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| {
        failure(&format!(
            "The active query did not show {}",
            expected.display()
        ))
    })?
}

pub(crate) struct Run {
    native: Arc<FolderProvider>,
    trials: usize,
    observation: Mutex<Observation>,
}

impl Run {
    pub(crate) fn from_env(
        native: Option<&Arc<FolderProvider>>,
    ) -> VelocityResult<Option<Arc<Self>>> {
        let Some(trials) = std::env::var_os("VELOCITY_TYPING_ACCEPTANCE") else {
            return Ok(None);
        };
        let trials = trials
            .to_str()
            .and_then(|value| value.parse().ok())
            .filter(|count| (1..=100).contains(count))
            .ok_or_else(|| {
                VelocityError::Config(
                    "Set VELOCITY_TYPING_ACCEPTANCE to a trial count from 1 to 100".into(),
                )
            })?;
        let native = native.cloned().ok_or_else(|| {
            VelocityError::Config("Typing acceptance requires native folder search".into())
        })?;
        Ok(Some(Arc::new(Self {
            native,
            trials,
            observation: Mutex::default(),
        })))
    }

    pub(crate) fn after_rendering(&self, ui: &SearchBar) {
        let mut observed = self.observation.lock().unwrap();
        let Some(started) = observed.started else {
            return;
        };
        let model = ui.get_search_results();
        if model.row_count() == 0 {
            return;
        }
        let row = model.row_data(0).unwrap();
        if model.row_count() != 1 || row.name != EXPECTED {
            observed.stale_frames += 1;
        } else {
            observed.painted_generation = Some(ui.get_search_generation());
            if observed.first_frame.is_none() {
                observed.first_frame = Some((
                    started.elapsed(),
                    self.native.status().scanning,
                    row.size == "Unknown size" && row.modified == "Unknown",
                ));
            }
        }
    }

    pub(crate) fn start(
        self: Arc<Self>,
        ui: &SearchBar,
        runtime: &Runtime,
    ) -> JoinHandle<VelocityResult<()>> {
        let weak = ui.as_weak();
        runtime.spawn(async move {
            let result = self.verify(&weak).await;
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
            result
        })
    }

    async fn verify(self: Arc<Self>, ui: &Weak<SearchBar>) -> VelocityResult<()> {
        // Wait for the desktop to observe the initial snapshot as well. Only the
        // subsequent rebuild should account for the automatic refresh we test.
        timeout(Duration::from_secs(20), async {
            while !on_ui(ui, |ui| ui.get_coverage().contains(" · ready (")).await? {
                sleep(Duration::from_millis(15)).await;
            }
            Ok::<_, VelocityError>(())
        })
        .await
        .map_err(|_| failure("The desktop did not report a ready search root"))??;
        let broad = self
            .native
            .search(SearchQuery {
                text: PREFIX.into(),
                max_results: 0,
                offset: 0,
            })
            .await?;
        if broad.total_count.unwrap_or_default() <= 100 {
            return Err(failure(
                "The fixture must contain more than 100 report files",
            ));
        }
        let mut latencies = Vec::with_capacity(self.trials);
        for trial in 1..=self.trials {
            let native = self.native.clone();
            let rebuild = tokio::spawn(async move { native.reindex().await });
            wait_for("the rebuild to start", || self.native.status().scanning).await?;
            on_ui(ui, move |ui| {
                ui.invoke_focus_search();
                if trial > 1 {
                    for _ in 0..PREFIX.len() + SUFFIX.len() {
                        key(&ui, Key::Backspace);
                    }
                }
                for ch in PREFIX.chars() {
                    key(&ui, ch);
                }
            })
            .await?;
            sleep(Duration::from_millis(35)).await;
            if !self.native.status().scanning {
                return Err(failure(
                    "Rebuild finished before the narrow edit. Increase the fixture size",
                ));
            }
            let run = self.clone();
            let generation = on_ui(ui, move |ui| {
                run.observation.lock().unwrap().started = Some(Instant::now());
                for ch in SUFFIX.chars() {
                    key(&ui, ch);
                }
                ui.get_search_generation()
            })
            .await?;
            wait_for("the narrow page to paint", || {
                self.observation.lock().unwrap().first_frame.is_some()
            })
            .await?;
            rebuild.await.map_err(|err| failure(&err.to_string()))??;
            // Keep observing through the automatic query refresh after publication.
            wait_for("the post-rebuild page to paint", || {
                self.observation
                    .lock()
                    .unwrap()
                    .painted_generation
                    .is_some_and(|painted| painted != generation)
            })
            .await?;
            let observed = std::mem::take(&mut *self.observation.lock().unwrap());
            let (elapsed, during_rebuild, unknown_metadata) = observed.first_frame.unwrap();
            if observed.stale_frames != 0 || !unknown_metadata {
                return Err(failure(
                    "An obsolete result painted or metadata was loaded eagerly",
                ));
            }
            if !during_rebuild {
                return Err(failure(
                    "The rebuild finished before the names painted. Increase the fixture size",
                ));
            }
            latencies.push(elapsed.as_micros());
            println!(
                "trial={trial} first_paint_us={} rebuilding=true stale_frames=0 metadata=unknown",
                elapsed.as_micros()
            );
        }
        latencies.sort_unstable();
        println!(
            "typing_during_rebuild trials={} p50_us={} p95_us={} stale_frames=0",
            self.trials,
            latencies[self.trials.div_ceil(2) - 1],
            latencies[(self.trials * 95).div_ceil(100) - 1]
        );
        Ok(())
    }
}

fn key(ui: &SearchBar, text: impl Into<slint::SharedString>) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.into() });
}

async fn on_ui<T: Send + 'static>(
    ui: &Weak<SearchBar>,
    action: impl FnOnce(SearchBar) -> T + Send + 'static,
) -> VelocityResult<T> {
    let (send, receive) = oneshot::channel();
    ui.upgrade_in_event_loop(move |ui| {
        let _ = send.send(action(ui));
    })
    .map_err(|err| failure(&err.to_string()))?;
    timeout(Duration::from_secs(5), receive)
        .await
        .map_err(|_| failure("The UI thread did not respond"))?
        .map_err(|err| failure(&err.to_string()))
}

async fn wait_for(what: &str, condition: impl Fn() -> bool) -> VelocityResult<()> {
    timeout(Duration::from_secs(20), async {
        while !condition() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| failure(&format!("Timed out waiting for {what}")))
}

fn failure(message: &str) -> VelocityError {
    VelocityError::IndexProvider(message.into())
}
