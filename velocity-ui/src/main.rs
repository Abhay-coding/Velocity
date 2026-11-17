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
    SearchFinished(u64, Result<(Vec<SearchResultItem>, String), String>),
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

/// Open a path with the Windows shell without interpreting it as a command line.
fn open_file(path: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::ffi::OsStr;
        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(hwnd: isize, operation: *const u16, file: *const u16,
                parameters: *const u16, directory: *const u16, show: i32) -> isize;
        }
        let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
        let result = unsafe { ShellExecuteW(0, std::ptr::null(), wide.as_ptr(),
            std::ptr::null(), std::ptr::null(), 1) };
        if result <= 32 { error!("Failed to open file (shell error {})", result); }
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

    info!("Starting Velocity Search...");

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

    let config = Config::load()?;
    let max_results = config.ui.max_results;
    let provider = Arc::new(EverythingCliAdapter::new(None)
        .with_max_results(max_results)
        .with_timeout(Duration::from_secs(10)));

    // Create Slint UI on the main thread
    let ui = SearchBar::new()?;

    // Set initial UI state
    ui.set_placeholder("Search files...".into());
    ui.set_hotkey_hint(config.hotkey.hotkey_hint().into());
    ui.set_searching(false);
    ui.set_results_count(0);
    ui.set_status_text("Connecting to Everything...".into());

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
    let command_search_id = search_id.clone();
    runtime_handle_tray.spawn(async move {
        while let Some(cmd) = ui_rx.recv().await {
            let ui_weak = ui_handle_for_commands.clone();
            let command_search_id = command_search_id.clone();
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
                        UiCommand::SearchFinished(id, outcome) => {
                            if command_search_id.load(Ordering::SeqCst) != id { return; }
                            ui.set_searching(false);
                            match outcome {
                                Ok((items, status)) => {
                                    ui.set_results_count(items.len() as i32);
                                    ui.set_search_results(ModelRc::new(VecModel::from(items)));
                                    ui.set_status_text(status.into());
                                }
                                Err(message) => ui.set_status_text(format!("Error: {}", message).into()),
                            }
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

    let init_provider = provider.clone();
    let init_ui = ui.as_weak();
    let init_search_id = search_id.clone();
    runtime_handle.spawn(async move {
        let result = init_provider.initialize().await;
        let _ = invoke_from_event_loop(move || {
            if init_search_id.load(Ordering::SeqCst) != 0 { return; }
            if let Some(ui) = init_ui.upgrade() {
                match result {
                    Ok(()) => ui.set_status_text("Ready".into()),
                    Err(e) => ui.set_status_text(format!("Everything unavailable: {}", e).into()),
                }
            }
        });
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

        if trimmed_query.chars().count() < 2 {
            search_id.fetch_add(1, Ordering::SeqCst);
            let _ = ui_tx.blocking_send(UiCommand::ClearResults);
            let _ = ui_tx.blocking_send(UiCommand::ResetSelection);
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
                max_results,
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

                    let _ = ui_tx.send(UiCommand::SearchFinished(this_search_id,
                        Ok((items, format!("{} results in {}ms", results_count, query_time_ms))))).await;
                }
                Err(e) => {
                    error!("Search #{} error: {}", this_search_id, e);
                    let _ = ui_tx.send(UiCommand::SearchFinished(this_search_id, Err(e.to_string()))).await;
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
    let pause_item = MenuItem::new("Pause Indexing", false, None);
    tray_menu.append(&pause_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // "Exclude List" menu item
    let exclude_item = MenuItem::new("Exclude List...", false, None);
    tray_menu.append(&exclude_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // "Drives to Index" menu item
    let drives_item = MenuItem::new("Drives to Index...", false, None);
    tray_menu.append(&drives_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Separator
    tray_menu.append(&PredefinedMenuItem::separator()).map_err(|e| VelocityError::Config(format!("Failed to add separator: {}", e)))?;

    // "Auto-start" menu item (checkbox)
    let autostart_item = MenuItem::new("Auto-start", false, None);
    tray_menu.append(&autostart_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Separator
    tray_menu.append(&PredefinedMenuItem::separator()).map_err(|e| VelocityError::Config(format!("Failed to add separator: {}", e)))?;

    // "Check for Updates" menu item
    let updates_item = MenuItem::new("Check for Updates", false, None);
    tray_menu.append(&updates_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // "About" menu item
    let about_item = MenuItem::new("About", false, None);
    tray_menu.append(&about_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Separator
    tray_menu.append(&PredefinedMenuItem::separator()).map_err(|e| VelocityError::Config(format!("Failed to add separator: {}", e)))?;

    // "Quit" menu item
    let quit_item = MenuItem::new("Quit", true, None);
    tray_menu.append(&quit_item).map_err(|e| VelocityError::Config(format!("Failed to add menu item: {}", e)))?;

    // Create tray icon
    let mut pixels = vec![0u8; 32 * 32 * 4];
    for pixel in pixels.chunks_exact_mut(4) { pixel.copy_from_slice(&[0, 120, 212, 255]); }
    let icon = tray_icon::Icon::from_rgba(pixels, 32, 32)
        .map_err(|e| VelocityError::Config(format!("Failed to create tray icon: {}", e)))?;
    let _tray_icon = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Velocity Search")
        .with_menu(Box::new(tray_menu))
        .build()
        .map_err(|e| VelocityError::Config(format!("Failed to create tray icon: {}", e)))?;

    // Set up tray icon event handler
    let tray_event_receiver = TrayIconEvent::receiver();
    let ui_tx_tray = ui_tx.clone();

    // Spawn task to handle tray events
    std::thread::spawn(move || {
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

    // Menu events are received on a dedicated thread because recv is blocking.
    let menu_event_receiver = MenuEvent::receiver();
    let ui_tx_menu = ui_tx.clone();
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();
    std::thread::spawn(move || {
        while let Ok(event) = menu_event_receiver.recv() {
            if event.id == show_id {
                let _ = ui_tx_menu.blocking_send(UiCommand::ShowWindow);
            } else if event.id == quit_id {
                let _ = invoke_from_event_loop(|| { let _ = slint::quit_event_loop(); });
                break;
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