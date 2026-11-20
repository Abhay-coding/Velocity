use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::task::JoinHandle;
use tracing::{error, info};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIconBuilder, TrayIconEvent,
};
use velocity_core::{
    config::Config,
    error::{VelocityError, VelocityResult},
    index_provider::{IndexProvider, SearchQuery, SearchResults},
};
use velocity_everything::EverythingCliAdapter;

slint::include_modules!();

#[derive(Default)]
struct SearchState {
    pending: Option<JoinHandle<()>>,
    page: Vec<SearchResultItem>,
    total: Option<usize>,
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
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn show_page(ui: &SearchBar, state: &SearchState) {
    let filter = ui.get_result_filter();
    let shown: Vec<_> = state
        .page
        .iter()
        .filter(|row| match filter {
            1 => !row.is_directory,
            2 => row.is_directory,
            _ => true,
        })
        .cloned()
        .collect();
    let summary = match (filter, state.total) {
        (0, Some(total)) if total > shown.len() => format!("{} of {total} results", shown.len()),
        (0, _) => format!("{} results shown", shown.len()),
        _ => format!("{} of {} displayed results", shown.len(), state.page.len()),
    };
    ui.set_result_summary(summary.into());
    ui.set_search_results(ModelRc::new(VecModel::from(shown)));
}

fn receive_page(ui: &SearchBar, state: &mut SearchState, results: VelocityResult<SearchResults>) {
    state.pending = None;
    ui.set_searching(false);
    match results {
        Ok(results) => {
            state.total = results.total_count;
            state.page = results
                .results
                .into_iter()
                .map(|result| SearchResultItem {
                    name: result.name.into(),
                    path: result.path.to_string_lossy().into_owned().into(),
                    size: if result.is_directory {
                        "".into()
                    } else {
                        format_bytes(result.size).into()
                    },
                    modified: if result.modified == 0 {
                        "".into()
                    } else {
                        format_timestamp(result.modified).into()
                    },
                    is_directory: result.is_directory,
                })
                .collect();
            show_page(ui, state);
        }
        Err(err) => {
            error!("Search failed: {err}");
            ui.set_failed(true);
            ui.set_status_text("Everything search failed".into());
        }
    }
}

fn open_path(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }
    if !path.is_absolute() {
        return Err("Result has no absolute path".into());
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let status = unsafe {
        ShellExecuteW(
            0,
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if status > 32 {
        Ok(())
    } else {
        Err(format!("Could not open item (Windows error {status})"))
    }
}

fn main() -> VelocityResult<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "velocity=debug,info".into()),
        )
        .init();

    let config = Config::load()?;
    let max_results = config.ui.max_results;
    let provider = Arc::new(EverythingCliAdapter::new(
        std::env::var_os("VELOCITY_ES_PATH").map(PathBuf::from),
    ));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ui = SearchBar::new()?;
    ui.set_dark(config.ui.theme != "light");
    ui.set_hotkey_hint("Alt+Space".into());
    ui.set_status_text("Connecting to Everything…".into());
    let state = Arc::new(Mutex::new(SearchState::default()));

    let init_provider = provider.clone();
    let weak = ui.as_weak();
    runtime.spawn(async move {
        let result = init_provider.initialize().await;
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if ui.get_status_text() != "Connecting to Everything…" {
                return;
            }
            match result {
                Ok(()) => ui.set_status_text("Everything connected".into()),
                Err(err) => {
                    error!("Everything initialization failed: {err}");
                    ui.set_failed(true);
                    ui.set_status_text("Everything unavailable".into());
                }
            }
        });
    });

    let callback_state = state.clone();
    let weak = ui.as_weak();
    let handle = runtime.handle().clone();
    ui.on_search_requested(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let mut state = callback_state.lock().unwrap();
        if let Some(job) = state.pending.take() {
            job.abort();
        }
        state.page.clear();
        state.total = None;
        ui.set_search_results(ModelRc::default());
        ui.set_result_summary("".into());
        ui.set_failed(false);
        let text = text.trim();
        if text.chars().count() < 2 {
            ui.set_status_text(
                if text.is_empty() {
                    ""
                } else {
                    "Type at least two characters"
                }
                .into(),
            );
            ui.set_searching(false);
            return;
        }
        ui.set_searching(true);
        ui.set_status_text("Searching…".into());
        let query = SearchQuery {
            text: text.to_owned(),
            max_results,
            offset: 0,
        };
        let provider = provider.clone();
        let weak = weak.clone();

        // A result can already be queued when its task is aborted. Validate its
        // generation on the event thread before publishing the result.
        let generation = ui.get_search_generation().wrapping_add(1);
        ui.set_search_generation(generation);
        let result_state = callback_state.clone();
        state.pending = Some(handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let outcome = provider.search(query).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                if ui.get_search_generation() != generation || !ui.get_searching() {
                    return;
                }
                ui.set_status_text("".into());
                receive_page(&ui, &mut result_state.lock().unwrap(), outcome);
            });
        }));
    });

    let weak = ui.as_weak();
    ui.on_filter_requested(move |filter| {
        if let Some(ui) = weak.upgrade() {
            ui.set_result_filter(filter);
            if !ui.get_searching() && !ui.get_result_summary().is_empty() {
                show_page(&ui, &state.lock().unwrap());
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_result_opened(move |path| {
        if let (Some(ui), Err(err)) = (weak.upgrade(), open_path(Path::new(path.as_str()))) {
            ui.set_status_text(err.into());
        }
    });
    let weak = ui.as_weak();
    ui.on_result_opened_folder(move |path| {
        let path = Path::new(path.as_str());
        let result = open_path(path.parent().unwrap_or(path));
        if let (Some(ui), Err(err)) = (weak.upgrade(), result) {
            ui.set_status_text(err.into());
        }
    });
    let weak = ui.as_weak();
    ui.on_escape_pressed(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let menu = Menu::new();
    let show = MenuItem::new("Show search", true, None);
    let theme = MenuItem::new("Toggle light / dark", true, None);
    let quit = MenuItem::new("Quit Velocity Search", true, None);
    for item in [&show, &theme, &quit] {
        menu.append(item)
            .map_err(|err| VelocityError::Config(err.to_string()))?;
    }
    let mut pixels = vec![0; 32 * 32 * 4];
    for (i, pixel) in pixels.chunks_exact_mut(4).enumerate() {
        let (x, y) = (i % 32, i / 32);
        if (3..29).contains(&x) && (3..29).contains(&y) {
            let v = (9..24).contains(&y) && (x.abs_diff(16) as i32 - (23 - y as i32) / 2).abs() < 2;
            pixel.copy_from_slice(if v {
                &[17, 25, 29, 255]
            } else {
                &[119, 228, 206, 255]
            });
        }
    }
    let icon = tray_icon::Icon::from_rgba(pixels, 32, 32)
        .map_err(|err| VelocityError::Config(err.to_string()))?;
    let _tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Velocity Search")
        .with_menu(Box::new(menu))
        .build()
        .map_err(|err| VelocityError::Config(err.to_string()))?;

    let weak = ui.as_weak();
    let (show_id, theme_id, quit_id) = (show.id().clone(), theme.id().clone(), quit.id().clone());
    std::thread::spawn(move || {
        while let Ok(event) = MenuEvent::receiver().recv() {
            if event.id == quit_id {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
                break;
            }
            if event.id == show_id {
                let _ = weak.upgrade_in_event_loop(show_window);
            } else if event.id == theme_id {
                let _ = weak.upgrade_in_event_loop(|ui| ui.set_dark(!ui.get_dark()));
            }
        }
    });
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        while let Ok(event) = TrayIconEvent::receiver().recv() {
            if format!("{event:?}").contains("DoubleClick") {
                let _ = weak.upgrade_in_event_loop(show_window);
            }
        }
    });
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        use rdev::{listen, EventType, Key};
        let (mut alt, mut space) = (false, false);
        if let Err(err) = listen(move |event| match event.event_type {
            EventType::KeyPress(Key::Alt) => alt = true,
            EventType::KeyRelease(Key::Alt) => alt = false,
            EventType::KeyPress(Key::Space) => {
                if alt && !space {
                    let _ = weak.upgrade_in_event_loop(show_window);
                }
                space = true;
            }
            EventType::KeyRelease(Key::Space) => space = false,
            _ => {}
        }) {
            error!("Global hotkey unavailable: {err:?}");
        }
    });

    info!("Starting Velocity Search");
    ui.show()?;
    slint::run_event_loop_until_quit()?;
    runtime.shutdown_background();
    Ok(())
}

fn show_window(ui: SearchBar) {
    if let Err(err) = ui.show() {
        error!("Could not show window: {err}");
    }
    ui.invoke_focus_search();
}
