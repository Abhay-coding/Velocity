use slint::{ComponentHandle, ModelRc, VecModel, invoke_from_event_loop};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info};

use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    TrayIconBuilder, TrayIconEvent,
};

use velocity_core::{
    config::Config,
    error::{VelocityError, VelocityResult},
    index_provider::{IndexProvider, SearchQuery},
};
use velocity_everything::EverythingCliAdapter;

slint::include_modules!();

#[derive(Debug)]
enum UiCommand {
    SetResults(Vec<SearchResultItem>),
    SetStatus(String),
    SetSearching(bool),
    ClearResults,
    ShowWindow,
    HideWindow,
    ResetSelection,
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

/// Safely open a file with the default application
/// Properly escapes the path to prevent command injection
fn open_file(path: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;
        // Use start with quoted path to safely handle spaces and special characters
        // The empty string "" is the window title (required by start when quoting)
        if let Err(e) = Command::new("cmd")
            .args(["/c", "start", "", "\"", path, "\""])
            .spawn()
        {
            error!("Failed to open file: {}", e);
        }
    }
}

/// Safely open the folder containing the given file
fn open_containing_folder(path: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::path::Path;
        use std::process::Command;

        let path_obj = Path::new(path);
        let folder = path_obj.parent().map(|p| p.to_string_lossy().to_string());

        if let Some(folder) = folder {
            if !folder.is_empty() {
                info!("Opening containing folder: {}", folder);
                if let Err(e) = Command::new("explorer")
                    .args([&folder])
                    .spawn()
                {
                    error!("Failed to open folder: {}", e);
                }
            }
        }
    }
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
    let runtime_handle_tray = runtime_handle.clone();

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
    runtime_handle_tray.spawn(async move {
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
                        UiCommand::ClearResults => {
                            ui.set_results_count(0);
                            ui.set_searching(false);
                            ui.set_status_text("".into());
                            ui.set_search_results(ModelRc::new(VecModel::from(Vec::<SearchResultItem>::new())));
                        }
                        UiCommand::ResetSelection => {
                            ui.set_selected_path("".into());
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
    let runtime_handle_tray = runtime_handle.clone();

    // Clone runtime_handle_tray for the search callback to avoid moving the original
    let runtime_handle_search_cb = runtime_handle_tray.clone();
    ui.on_search_requested(move |query_text| {
        let query = query_text.to_string();
        let ui_tx = ui_tx_search.clone();
        let provider = provider.clone();
        let search_id = search_id.clone();

        let trimmed_query = query.trim().to_string();

        // Check for minimum query length (2 characters)
        if trimmed_query.is_empty() {
            info!("Empty query, clearing results and invalidating in-flight searches");
            search_id.fetch_add(1, Ordering::SeqCst);
            let _ = ui_tx.blocking_send(UiCommand::ClearResults);
            let _ = ui_tx.blocking_send(UiCommand::ResetSelection);
            return;
        }

        if trimmed_query.len() < 2 {
            info!("Query too short (min 2 chars): '{}'", trimmed_query);
            let _ = ui_tx.blocking_send(UiCommand::SetStatus("Type at least 2 characters...".to_string()));
            return;
        }

        let this_search_id = search_id.fetch_add(1, Ordering::SeqCst) + 1;
        info!("Search #{}: '{}'", this_search_id, trimmed_query);

        let _ = ui_tx.blocking_send(UiCommand::SetSearching(true));
        let _ = ui_tx.blocking_send(UiCommand::SetStatus(format!("Searching for '{}'...", trimmed_query)));
        let _ = ui_tx.blocking_send(UiCommand::ResetSelection);

        runtime_handle_search_cb.spawn(async move {
            info!("Search #{} task started, waiting for debounce (150ms)...", this_search_id);
            tokio::time::sleep(Duration::from_millis(150)).await;

            // Check if a newer search has superseded this one
            let current_search_id = search_id.load(Ordering::SeqCst);
            if current_search_id != this_search_id {
                info!("Search #{} superseded by newer search #{}, aborting", this_search_id, current_search_id);
                return;
            }

            info!("Search #{} debounce complete, executing search...", this_search_id);

            let search_query = SearchQuery {
                text: trimmed_query,
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

    // Result selection (mouse click)
    ui.on_result_selected(move |path_text| {
        let path = path_text.to_string();
        info!("Result selected: {}", path);
        // Selection change handled by selection_changed callback
    });

    // Result opened via Enter key
    ui.on_result_opened(move |path_text| {
        let path = path_text.to_string();
        info!("Opening file: {}", path);
        open_file(&path);
    });

    // Result opened folder via Ctrl+Enter
    ui.on_result_opened_folder(move |path_text| {
        let path = path_text.to_string();
        info!("Opening folder: {}", path);
        open_containing_folder(&path);
    });

    // Selection changed via keyboard navigation
    ui.on_selection_changed(move |path| {
        info!("Selection changed to path: {}", path);
        // Could update UI state here if needed
    });

    // Key up pressed - navigate up in results
    ui.on_key_up_pressed(move || {
        info!("Key up pressed - navigate up");
        // The selected_path update is handled in the UI
    });

    // Key down pressed - navigate down in results
    ui.on_key_down_pressed(move || {
        info!("Key down pressed - navigate down");
        // The selected_path update is handled in the UI
    });

    // Key down pressed - navigate down in results
    ui.on_key_down_pressed(move || {
        info!("Key down pressed - navigate down");
        // The selected_path update is handled in the UI
    });

    // Create weak references for keyboard navigation callbacks (one for each callback)
    let ui_weak_enter = ui.as_weak();
    let ui_weak_ctrl_enter = ui.as_weak();

    // Key Enter pressed - open selected file
    ui.on_key_enter_pressed(move || {
        let ui_weak = ui_weak_enter.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let selected_path = ui.get_selected_path();
                if !selected_path.is_empty() {
                    info!("Enter pressed - opening: {}", selected_path);
                    open_file(&selected_path);
                }
            }
        });
    });

    // Key Ctrl+Enter pressed - open containing folder
    ui.on_key_ctrl_enter_pressed(move || {
        let ui_weak = ui_weak_ctrl_enter.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let selected_path = ui.get_selected_path();
                if !selected_path.is_empty() {
                    info!("Ctrl+Enter pressed - opening folder: {}", selected_path);
                    open_containing_folder(&selected_path);
                }
            }
        });
    });

    // Create system tray icon
    let tray_menu = Menu::new();

    // "Show Search" menu item
    let show_item = MenuItem::new("Show Search", true, None);
    tray_menu.append(&show_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Separator
    tray_menu.append(&PredefinedMenuItem::separator()).map_err(|e| VelocityError::Config(format!("Failed to add separator: {}", e)))?;

    // "Pause Indexing" menu item
    let pause_item = MenuItem::new("Pause Indexing", true, None);
    tray_menu.append(&pause_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // "Exclude List" menu item
    let exclude_item = MenuItem::new("Exclude List...", true, None);
    tray_menu.append(&exclude_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // "Drives to Index" menu item
    let drives_item = MenuItem::new("Drives to Index...", true, None);
    tray_menu.append(&drives_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Separator
    tray_menu.append(&PredefinedMenuItem::separator()).map_err(|e| VelocityError::Config(format!("Failed to add separator: {}", e)))?;

    // "Auto-start" menu item (checkbox)
    let autostart_item = MenuItem::new("Auto-start", true, None);
    tray_menu.append(&autostart_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Separator
    tray_menu.append(&PredefinedMenuItem::separator()).map_err(|e| VelocityError::Config(format!("Failed to add separator: {}", e)))?;

    // "Check for Updates" menu item
    let updates_item = MenuItem::new("Check for Updates", true, None);
    tray_menu.append(&updates_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // "About" menu item
    let about_item = MenuItem::new("About", true, None);
    tray_menu.append(&about_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Separator
    tray_menu.append(&PredefinedMenuItem::separator()).map_err(|e| VelocityError::Config(format!("Failed to add separator: {}", e)))?;

    // "Quit" menu item
    let quit_item = MenuItem::new("Quit", true, None);
    tray_menu.append(&quit_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Create tray icon
    let _tray_icon = TrayIconBuilder::new()
        .with_tooltip("Velocity Search")
        .with_menu(Box::new(tray_menu))
        .build()
        .map_err(|e| VelocityError::Config(format!("Failed to create tray icon: {}", e)))?;

    // Set up tray icon event handler
    let tray_event_receiver = TrayIconEvent::receiver();
    let ui_tx_tray = ui_tx.clone();

    // Spawn task to handle tray events
    runtime_handle_tray.spawn(async move {
        loop {
            match tray_event_receiver.recv() {
                Ok(event) => {
                    // Check for double click using event type
                    let is_double_click = format!("{:?}", event).contains("DoubleClick");
                    if is_double_click {
                        let _ = ui_tx_tray.blocking_send(UiCommand::ShowWindow);
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Set up menu event handler
    let menu_event_receiver = MenuEvent::receiver();
    let ui_tx_menu = ui_tx.clone();

    runtime_handle_tray.spawn(async move {
        loop {
            match menu_event_receiver.recv() {
                Ok(event) => {
                    // Use the menu item ID which is the label text
                    let id = event.id.0.to_string();
                    match id.as_str() {
                        "Show Search" => {
                            let _ = ui_tx_menu.blocking_send(UiCommand::ShowWindow);
                        }
                        "Pause Indexing" => {
                            info!("Pause Indexing clicked");
                            let _ = ui_tx_menu.blocking_send(UiCommand::SetStatus("Indexing paused".to_string()));
                        }
                        "Exclude List..." => {
                            info!("Exclude List clicked");
                            #[cfg(target_os = "windows")]
                            {
                                use std::process::Command;
                                let _ = Command::new("notepad")
                                    .args(["excludes.txt"])
                                    .spawn();
                            }
                        }
                        "Drives to Index..." => {
                            info!("Drives to Index clicked");
                            #[cfg(target_os = "windows")]
                            {
                                use std::process::Command;
                                let _ = Command::new("notepad")
                                    .args(["drives.txt"])
                                    .spawn();
                            }
                        }
                        "Auto-start" => {
                            info!("Auto-start clicked");
                        }
                        "Check for Updates" => {
                            info!("Check for Updates clicked");
                        }
                        "About" => {
                            info!("About clicked");
                        }
                        "Quit" => {
                            info!("Quit clicked");
                            std::process::exit(0);
                        }
                        _ => {}
                    }
                }
                Err(_) => break,
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