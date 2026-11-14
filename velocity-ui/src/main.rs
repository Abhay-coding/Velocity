use slint::{ComponentHandle, ModelRc, VecModel, invoke_from_event_loop};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use velocity_core::{
    config::Config,
    error::VelocityResult,
    index_provider::{IndexProvider, SearchQuery},
};
use velocity_everything::EverythingCliAdapter;

slint::include_modules!();

#[derive(Debug)]
enum UiCommand {
    SetResults(Vec<SearchResultItem>),
    SetStatus(String),
    SetSearching(bool),
    SetResultsCount(i32),
    ClearResults,
    ShowWindow,
    HideWindow,
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

fn format_timestamp(unix_secs: u64) -> String {
    use chrono::{DateTime, Local};
    DateTime::from_timestamp(unix_secs as i64, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn main() -> VelocityResult<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("velocity=debug,info")
        .init();

    info!("Starting Velocity Search (Dynamic Search Test)...");

    // Create a tokio runtime in a separate thread for async operations
    // Use a oneshot channel to pass the runtime handle
    let (runtime_tx, runtime_rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");

        // Send the handle so we can spawn tasks from the main thread
        let handle = rt.handle().clone();
        let _ = runtime_tx.send(handle);

        // Signal that runtime is ready
        let _ = ready_tx.send(());

        // Run the runtime forever (this blocks the thread)
        rt.block_on(async {
            // Keep alive forever
            std::future::pending::<()>().await;
        });
    });

    // Wait for runtime to be ready
    ready_rx.recv().expect("Runtime thread failed to start");
    let runtime_handle = runtime_rx.recv().expect("Failed to receive runtime handle");

    // Run async initialization on the tokio runtime
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    runtime_handle.spawn(async move {
        // Load configuration
        let config = match Config::load() {
            Ok(c) => {
                info!("Configuration loaded");
                c
            }
            Err(e) => {
                error!("Failed to load config: {}", e);
                let _ = init_tx.send(Err(e));
                return;
            }
        };

        // Initialize index provider (Everything CLI adapter)
        let provider = EverythingCliAdapter::new(None)
            .with_max_results(config.ui.max_results)
            .with_timeout(Duration::from_secs(10));

        let provider = Arc::new(provider);
        let provider_initialized = match provider.initialize().await {
            Ok(()) => {
                info!("Index provider initialized");
                if let Ok(stats) = provider.stats().await {
                    info!("Index stats: {} files", stats.total_files);
                }
                true
            }
            Err(e) => {
                error!("Failed to initialize index provider: {}", e);
                false
            }
        };

        let _ = init_tx.send(Ok((config, provider, provider_initialized)));
    });

    // Wait for initialization to complete
    let (config, provider, _provider_initialized) = init_rx.recv().expect("Init channel closed")?;

    // Create Slint UI on the main thread
    let ui = SearchBar::new()?;

    // Set initial UI state
    ui.set_placeholder("Search files...".into());
    ui.set_hotkey_hint(config.hotkey.hotkey_hint().into());
    ui.set_searching(false);
    ui.set_results_count(0);
    ui.set_status_text("Ready".into());

    // IMMEDIATELY SHOW WINDOW
    ui.show()?;
    info!("Window shown immediately");

    // Channel for UI updates from background tasks
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiCommand>(100);

    // Atomic counter for search ID to prevent race conditions
    let search_id = Arc::new(AtomicU64::new(0));

    // Spawn UI command handler on the main thread (via event loop)
    let ui_handle = ui.as_weak();
    let ui_handle_for_commands = ui_handle.clone();
    runtime_handle.spawn(async move {
        while let Some(cmd) = ui_rx.recv().await {
            let ui_weak = ui_handle_for_commands.clone();
            let _ = invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    match cmd {
                        UiCommand::SetResults(items) => {
                            ui.set_results_count(items.len() as i32);
                            ui.set_searching(false);
                            let model = ModelRc::new(VecModel::from(items));
                            ui.set_search_results(model);
                        }
                        UiCommand::SetStatus(text) => {
                            ui.set_status_text(text.into());
                        }
                        UiCommand::SetSearching(searching) => {
                            ui.set_searching(searching);
                        }
                        UiCommand::SetResultsCount(count) => {
                            ui.set_results_count(count);
                        }
                        UiCommand::ClearResults => {
                            ui.set_results_count(0);
                            ui.set_searching(false);
                            ui.set_status_text("".into());
                            ui.set_search_results(ModelRc::new(VecModel::from(Vec::<SearchResultItem>::new())));
                        }
                        UiCommand::ShowWindow => {
                            let _ = ui.show();
                        }
                        UiCommand::HideWindow => {
                            let _ = ui.hide();
                        }
                    }
                }
            });
        }
    });

    // INJECT TEST RESULTS FROM RUST ON STARTUP - verify Rust-to-Slint binding
    let ui_tx_clone = ui_tx.clone();
    runtime_handle.spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;

        let test_items = vec![
            SearchResultItem {
                name: "Rust-Generated: Assignment-8.pdf".into(),
                path: "C:\\Documents\\Assignment-8.pdf".into(),
                size: "2.5 MB".into(),
                modified: "2024-01-13 09:15".into(),
                is_directory: false,
            },
            SearchResultItem {
                name: "Rust-Generated: Resume.docx".into(),
                path: "C:\\Users\\Resume.docx".into(),
                size: "1.2 MB".into(),
                modified: "2024-01-10 14:30".into(),
                is_directory: false,
            },
            SearchResultItem {
                name: "Rust-Generated: Project Notes.txt".into(),
                path: "C:\\Projects\\Notes.txt".into(),
                size: "15 KB".into(),
                modified: "2024-01-12 10:45".into(),
                is_directory: false,
            },
            SearchResultItem {
                name: "Rust-Generated: Documents".into(),
                path: "C:\\Documents".into(),
                size: "".into(),
                modified: "2024-01-01 09:00".into(),
                is_directory: true,
            },
            SearchResultItem {
                name: "Rust-Generated: Downloads".into(),
                path: "C:\\Users\\Downloads".into(),
                size: "".into(),
                modified: "2024-01-05 16:20".into(),
                is_directory: true,
            },
        ];

        let _ = ui_tx_clone.send(UiCommand::SetResults(test_items)).await;
        let _ = ui_tx_clone.send(UiCommand::SetStatus("Rust-generated results injected!".to_string())).await;
    });

    // Set up global hotkey (Alt+Space) using rdev
    let ui_tx_hotkey = ui_tx.clone();
    std::thread::spawn(move || {
        use rdev::{listen, EventType, Key};
        let mut alt_pressed = false;
        if let Err(e) = listen(move |event| {
            match event.event_type {
                EventType::KeyPress(key) => {
                    if key == Key::Alt {
                        alt_pressed = true;
                    } else if key == Key::Space && alt_pressed {
                        let _ = ui_tx_hotkey.blocking_send(UiCommand::ShowWindow);
                    }
                }
                EventType::KeyRelease(key) => {
                    if key == Key::Alt {
                        alt_pressed = false;
                    }
                }
                _ => {}
            }
        }) {
            error!("Failed to listen for global hotkeys: {:?}", e);
        }
    });

    // Search callback - REAL SEARCH PIPELINE
    let provider = provider.clone();
    let ui_tx_search = ui_tx.clone();
    let search_id = search_id.clone();

    ui.on_search_requested(move |query_text| {
        let query = query_text.to_string();
        let ui_tx = ui_tx_search.clone();
        let provider = provider.clone();
        let search_id = search_id.clone();

        if query.trim().is_empty() {
            info!("Empty query, clearing results");
            let _ = ui_tx.blocking_send(UiCommand::ClearResults);
            return;
        }

        let this_search_id = search_id.fetch_add(1, Ordering::SeqCst) + 1;
        info!("Search #{}: '{}'", this_search_id, query);

        let _ = ui_tx.blocking_send(UiCommand::SetSearching(true));
        let _ = ui_tx.blocking_send(UiCommand::SetStatus(format!("Searching for '{}'...", query)));

        runtime_handle.spawn(async move {
            info!("Search #{} task started, waiting for debounce (300ms)...", this_search_id);
            tokio::time::sleep(Duration::from_millis(300)).await;

            // Check if a newer search has superseded this one
            let current_search_id = search_id.load(Ordering::SeqCst);
            if current_search_id != this_search_id {
                info!("Search #{} superseded by newer search #{}, aborting", this_search_id, current_search_id);
                return;
            }

            info!("Search #{} debounce complete, executing search...", this_search_id);

            let search_query = SearchQuery {
                text: query.clone(),
                max_results: 100,
                offset: 0,
            };

            info!("Search #{} executing provider.search...", this_search_id);
            match provider.search(search_query).await {
                Ok(results) => {
                    // Check again if this search is still the latest
                    let current_search_id = search_id.load(Ordering::SeqCst);
                    if current_search_id != this_search_id {
                        info!("Search #{} superseded after completion, discarding results", this_search_id);
                        return;
                    }

                    let results_count = results.results.len();
                    let query_time_ms = results.query_time_ms;

                    let items: Vec<SearchResultItem> = results.results.into_iter().map(|r| SearchResultItem {
                        name: r.name.into(),
                        path: r.path.to_string_lossy().to_string().into(),
                        size: if r.is_directory { String::new().into() } else { format_bytes(r.size).into() },
                        modified: if r.modified > 0 {
                            format_timestamp(r.modified).into()
                        } else {
                            String::new().into()
                        },
                        is_directory: r.is_directory,
                    }).collect();

                    let _ = ui_tx.send(UiCommand::SetResults(items)).await;
                    let _ = ui_tx.send(UiCommand::SetStatus(format!("{} results in {}ms", results_count, query_time_ms))).await;
                }
                Err(e) => {
                    error!("Search #{} error: {}", this_search_id, e);
                    let _ = ui_tx.send(UiCommand::SetSearching(false)).await;
                    let _ = ui_tx.send(UiCommand::SetStatus(format!("Error: {}", e))).await;
                }
            }
        });
    });

    // Set up hotkey callback
    let ui_tx_hotkey_cb = ui_tx.clone();
    ui.on_hotkey_pressed(move || {
        info!("Hotkey pressed callback fired");
        let _ = ui_tx_hotkey_cb.blocking_send(UiCommand::ShowWindow);
    });

    // Set up escape
    let ui_tx_escape = ui_tx.clone();
    ui.on_escape_pressed(move || {
        let _ = ui_tx_escape.blocking_send(UiCommand::HideWindow);
    });

    // Result selection
    ui.on_result_selected(move |path_text| {
        let path = path_text.to_string();
        info!("Result selected: {}", path);

        // Try to open with default application
        #[cfg(target_os = "windows")]
        {
            use std::process::Command;
            if let Err(e) = Command::new("cmd")
                .args(["/c", "start", "", &path])
                .spawn()
            {
                error!("Failed to open file: {}", e);
            }
        }
    });

    // Run the UI event loop on the main thread
    info!("Starting UI event loop...");
    ui.run()?;

    // Shutdown
    info!("Velocity Search stopped");
    Ok(())
}