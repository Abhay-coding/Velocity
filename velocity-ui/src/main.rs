use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, info};

use velocity_core::{
    config::Config,
    error::{VelocityError, VelocityResult},
    index_provider::{IndexProvider, SearchQuery, SearchResult, SearchResults},
};
use velocity_everything::EverythingCliAdapter;

slint::include_modules!();

fn format_size(bytes: u64) -> String {
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
    use chrono::{DateTime, Local, TimeZone};
    DateTime::from_timestamp(unix_secs as i64, 0)
        .map(|dt| dt.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

// Convert velocity_core::SearchResult to Slint-generated SearchResultItem
fn to_slint_result(r: SearchResult) -> SearchResultItem {
    let size_str = if r.is_directory {
        String::new()
    } else {
        format_size(r.size)
    };
    let modified_str = if r.modified > 0 {
        format_timestamp(r.modified)
    } else {
        String::new()
    };
    SearchResultItem {
        name: r.name.into(),
        path: r.path.to_string_lossy().to_string().into(),
        size: size_str.into(),
        modified: modified_str.into(),
        is_directory: r.is_directory,
    }
}

#[derive(Debug)]
enum UiMessage {
    SearchResults(SearchResults),
    SearchError(String),
    Stats(velocity_core::index_provider::IndexStats),
    Status(String),
    ShowWindow,
    HideWindow,
    BackendError(String),
}

#[tokio::main]
async fn main() -> VelocityResult<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("velocity=debug,info")
        .init();

    info!("Starting Velocity Search...");

    // Load configuration
    let config = Config::load()?;
    info!("Configuration loaded");

    // Initialize index provider (Everything CLI adapter)
    let provider = EverythingCliAdapter::new(None)
        .with_max_results(config.ui.max_results)
        .with_timeout(Duration::from_secs(10));

    // Try to initialize provider - don't crash if it fails
    let provider = Arc::new(provider);
    let provider_initialized = match provider.initialize().await {
        Ok(()) => {
            info!("Index provider initialized");
            // Get initial stats
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

    // Create Slint UI
    let ui = SearchBar::new()?;

    // Set initial UI state
    ui.set_placeholder("Search files...".into());
    ui.set_hotkey_hint(config.hotkey.hotkey_hint().into());

    // Channel for UI updates from async tasks
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiMessage>(100);
    let ui_weak = ui.as_weak();

    // Spawn UI message handler
    let ui_handle = ui_weak.clone();
    tokio::spawn(async move {
        while let Some(msg) = ui_rx.recv().await {
            if let Some(ui) = ui_handle.upgrade() {
                match msg {
                    UiMessage::SearchResults(results) => {
                        let items: Vec<SearchResultItem> = results.results.into_iter().map(to_slint_result).collect();
                        ui.set_results_count(items.len() as i32);
                        ui.set_searching(false);
                        ui.set_status_text(format!("{} results in {}ms", items.len(), results.query_time_ms).into());

                        // Update results model
                        let model = ModelRc::new(VecModel::from(items));
                        ui.set_search_results(model);
                    }
                    UiMessage::SearchError(err) => {
                        ui.set_searching(false);
                        ui.set_status_text(format!("Error: {}", err).into());
                        error!("Search error: {}", err);
                    }
                    UiMessage::Stats(stats) => {
                        ui.set_status_text(format!("{} files indexed", stats.total_files).into());
                    }
                    UiMessage::Status(text) => {
                        ui.set_status_text(text.into());
                    }
                    UiMessage::ShowWindow => {
                        ui.show();
                    }
                    UiMessage::HideWindow => {
                        ui.hide();
                    }
                    UiMessage::BackendError(err) => {
                        ui.set_status_text(format!("Backend error: {}", err).into());
                        ui.set_searching(false);
                        // Show error in results area as well
                        let empty_model: Vec<SearchResultItem> = Vec::new();
                        let model = ModelRc::new(VecModel::from(empty_model));
                        ui.set_search_results(model);
                        ui.set_results_count(0);
                    }
                }
            }
        }
    });

    // If backend failed to initialize, show error in UI
    if !provider_initialized {
        let _ = ui_tx.send(UiMessage::BackendError(
            "Everything search backend not found. Please install Everything from voidtools.com".into()
        )).await;
        // Still show the window so user can see the error
        let _ = ui_tx.send(UiMessage::ShowWindow).await;
    }

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
                        // Alt+Space pressed
                        let _ = ui_tx_hotkey.send(UiMessage::ShowWindow);
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

    // Set up search callback
    let ui_tx_clone = ui_tx.clone();
    let provider_clone = provider.clone();
    let config_clone = config.clone();

    ui.on_search_requested(move |query_text| {
        let query = query_text.to_string();
        if query.trim().is_empty() {
            return;
        }

        let provider = provider_clone.clone();
        let ui_tx = ui_tx_clone.clone();
        let debounce_ms = config_clone.ui.hover_delay_ms; // reuse for search debounce

        // Debounce search
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(debounce_ms)).await;

            let search_query = SearchQuery {
                text: query.clone(),
                max_results: 100,
                offset: 0,
            };

            ui_tx.send(UiMessage::Status("Searching...".into())).await.ok();
            ui_tx.send(UiMessage::ShowWindow).await.ok();

            match provider.search(search_query).await {
                Ok(results) => {
                    ui_tx.send(UiMessage::SearchResults(results)).await.ok();
                }
                Err(e) => {
                    ui_tx.send(UiMessage::SearchError(e.to_string())).await.ok();
                }
            }
        });
    });

    // Set up result selection
    let ui_tx_select = ui_tx.clone();
    ui.on_result_selected(move |path_text| {
        let path = path_text.to_string();
        info!("Opening: {}", path);

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

    // Set up hotkey (placeholder - would use global hotkey crate in production)
    let ui_tx_hotkey = ui_tx.clone();
    ui.on_hotkey_pressed(move || {
        ui_tx_hotkey.try_send(UiMessage::ShowWindow).ok();
    });

    // Set up escape
    let ui_tx_escape = ui_tx.clone();
    ui.on_escape_pressed(move || {
        ui_tx_escape.try_send(UiMessage::HideWindow).ok();
    });

    // Set up preview
    let provider_preview = provider.clone();
    let ui_tx_preview = ui_tx.clone();
    ui.on_preview_requested(move |path_text| {
        let path = path_text.to_string();
        let provider = provider_preview.clone();
        let ui_tx = ui_tx_preview.clone();

        tokio::spawn(async move {
            // For now, just show basic file info as preview
            if let Ok(metadata) = std::fs::metadata(&path) {
                let preview = format!(
                    "Path: {}\nSize: {}\nModified: {}\nType: {}",
                    path,
                    format_size(metadata.len()),
                    format_timestamp(
                        metadata.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    ),
                    if metadata.is_dir() { "Directory" } else { "File" }
                );
                ui_tx.send(UiMessage::Status(preview)).await.ok();
            }
        });
    });

    // Run the UI event loop
    info!("Starting UI event loop...");
    ui.run()?;

    // Shutdown
    provider.shutdown().await?;
    info!("Velocity Search stopped");

    Ok(())
}