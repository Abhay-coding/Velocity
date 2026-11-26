use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, UNIX_EPOCH},
};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    ClickType, TrayIconBuilder, TrayIconEvent,
};
use velocity_core::{
    config::{Config, SearchBackend},
    error::{VelocityError, VelocityResult},
    index_provider::{IndexProvider, SearchQuery, SearchResults},
};
#[cfg(feature = "everything")]
use velocity_everything::EverythingCliAdapter;
use velocity_native::{FolderProvider, NativeStatus, RootState};

mod hotkey;

#[cfg(feature = "desktop-acceptance")]
mod desktop_acceptance;

slint::include_modules!();

#[derive(Default)]
struct SearchState {
    pending: Option<JoinHandle<()>>,
    page: Vec<SearchResultItem>,
    paths: Vec<PathBuf>,
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
        (0, _) if shown.len() == 1 => "1 result".into(),
        (0, _) => format!("{} results", shown.len()),
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
            state.paths = results.results.iter().map(|row| row.path.clone()).collect();
            state.page = results
                .results
                .into_iter()
                .enumerate()
                .map(|(id, result)| SearchResultItem {
                    id: id as i32,
                    name: result.name.into(),
                    path: result.path.to_string_lossy().into_owned().into(),
                    location: result
                        .path
                        .parent()
                        .unwrap_or(&result.path)
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                    size: if result.is_directory {
                        "".into()
                    } else {
                        result
                            .size
                            .map(format_bytes)
                            .unwrap_or_else(|| "Unknown size".into())
                            .into()
                    },
                    modified: result
                        .modified
                        .map(format_timestamp)
                        .unwrap_or_else(|| "Unknown".into())
                        .into(),
                    is_directory: result.is_directory,
                })
                .collect();
            show_page(ui, state);
        }
        Err(err) => {
            error!("Search failed: {err}");
            ui.set_failed(true);
            let invalid_query = matches!(err, VelocityError::InvalidQuery(_));
            ui.set_search_retry(
                !invalid_query && (!ui.get_native_backend() || ui.get_index_needs_retry()),
            );
            ui.set_status_text(
                if invalid_query {
                    err.to_string()
                } else {
                    format!("{} · {err}", ui.get_backend_name())
                }
                .into(),
            );
        }
    }
}

fn coverage_text(status: &NativeStatus) -> String {
    if status.roots.is_empty() {
        return "Native folder search · Set search.roots in config.toml".into();
    }
    let mut text = format!("Native search · {} indexed", status.indexed_entries);
    if status.paused {
        text.push_str(if status.searchable {
            " · Paused · Showing last index"
        } else {
            " · Paused"
        });
    } else if status.scanning && status.searchable {
        text.push_str(" · Updating · Showing last index");
    }
    for root in &status.roots {
        let label = match root.state {
            RootState::Scanning => format!("scanning ({} entries)", root.entries),
            RootState::Ready => format!("ready ({} entries)", root.entries),
            RootState::AccessDenied => "access denied".into(),
            RootState::Unavailable => "unavailable".into(),
            RootState::Skipped => "excluded, nested or a reparse point".into(),
        };
        text.push_str(&format!("\n{} · {label}", root.root.display()));
        if root.ntfs.is_some() {
            text.push_str(" · NTFS snapshot");
        } else if root.state == RootState::Ready && root.detail.is_some() {
            text.push_str(" · Folder fallback");
        } else if root.state == RootState::Ready {
            text.push_str(" · Folder index");
        } else if let Some(detail) = &root.detail {
            text.push_str(&format!(" · {detail}"));
        }
        if root.skipped > 0 {
            text.push_str(&format!(" · {} skipped", root.skipped));
            if let Some((path, reason)) = root.examples.first() {
                text.push_str(&format!(" · {} ({reason})", path.display()));
            }
        }
    }
    text
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
    let (provider, native): (Arc<dyn IndexProvider>, _) = match config.search.backend {
        SearchBackend::Everything => {
            #[cfg(feature = "everything")]
            {
                (
                    Arc::new(EverythingCliAdapter::new(
                        std::env::var_os("VELOCITY_ES_PATH").map(PathBuf::from),
                    )),
                    None,
                )
            }
            #[cfg(not(feature = "everything"))]
            return Err(VelocityError::Config(
                "Everything comparison requires a build with --features everything".into(),
            ));
        }
        SearchBackend::Native => {
            let mut roots = config.search.roots;
            if config.drives.enabled {
                roots.extend(
                    config
                        .drives
                        .drives
                        .iter()
                        .map(|drive| PathBuf::from(format!("{drive}\\"))),
                );
            }
            let native = FolderProvider::new(
                roots,
                if config.excludes.enabled {
                    &config.excludes.patterns
                } else {
                    &[]
                },
            )?;
            let native = Arc::new(if config.search.ntfs_snapshots {
                let native = native.with_ntfs_snapshots(velocity_native::ntfs::DEFAULT_BATCH_BYTES);
                if let Some(local_data) = std::env::var_os("LOCALAPPDATA") {
                    native.with_snapshot_cache(
                        PathBuf::from(local_data).join("Velocity").join("index"),
                    )
                } else {
                    native
                }
            } else {
                native
            });
            (native.clone(), Some(native))
        }
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let ui = SearchBar::new()?;
    let backend = provider.capabilities().name;
    ui.set_dark(config.ui.theme != "light");
    ui.set_hotkey_hint(config.hotkey.hotkey_hint().into());
    ui.set_backend_name(backend.clone().into());
    ui.set_native_backend(native.is_some());
    ui.set_coverage(format!("{backend} · Starting").into());
    let state = Arc::new(Mutex::new(SearchState::default()));

    let monitor = if let Some(native) = &native {
        let _runtime = runtime.enter();
        Some(native.start_monitor())
    } else {
        let init_provider = provider.clone();
        let weak = ui.as_weak();
        runtime.spawn(async move {
            let result = init_provider.initialize().await;
            let _ = weak.upgrade_in_event_loop(move |ui| match result {
                Ok(()) => ui.set_coverage("Everything CLI · Connected".into()),
                Err(err) => {
                    error!("Search initialization failed: {err}");
                    ui.set_failed(true);
                    ui.set_search_retry(true);
                    ui.set_coverage("Everything CLI · Unavailable".into());
                    ui.set_status_text(format!("Everything CLI · {err}").into());
                }
            });
        });
        None
    };

    let callback_state = state.clone();
    let weak = ui.as_weak();
    let handle = runtime.handle().clone();
    let search_provider = provider.clone();
    let first_paint = Arc::new(Mutex::new(None::<(i32, Instant)>));
    let render_time = first_paint.clone();
    #[cfg(feature = "desktop-acceptance")]
    let acceptance = desktop_acceptance::Run::from_env(native.as_ref())?;
    #[cfg(feature = "desktop-acceptance")]
    let (paint_observer, paint_window) = (acceptance.clone(), ui.as_weak());
    let rendering_notifier = ui.window().set_rendering_notifier(move |state, _| {
        if matches!(state, slint::RenderingState::AfterRendering) {
            if let Some((generation, edited)) = render_time.lock().unwrap().take() {
                debug!(
                    generation,
                    elapsed_us = edited.elapsed().as_micros(),
                    "First search page painted"
                );
            }
            #[cfg(feature = "desktop-acceptance")]
            if let (Some(observer), Some(ui)) = (&paint_observer, paint_window.upgrade()) {
                observer.after_rendering(&ui);
            }
        }
    });
    let paint_supported = rendering_notifier.is_ok();
    if let Err(err) = rendering_notifier {
        #[cfg(feature = "desktop-acceptance")]
        if acceptance.is_some() {
            return Err(VelocityError::Config(format!(
                "Desktop acceptance requires a renderer with paint notifications: {err:?}"
            )));
        }
        debug!(?err, "First-paint timing unavailable with this renderer");
    }
    let refresh_paint = first_paint.clone();
    ui.on_search_requested(move |text| {
        let Some(ui) = weak.upgrade() else { return };
        let edited = Instant::now();
        let generation = ui.get_search_generation().wrapping_add(1);
        ui.set_search_generation(generation);
        *first_paint.lock().unwrap() = None;
        let mut state = callback_state.lock().unwrap();
        if let Some(job) = state.pending.take() {
            job.abort();
        }
        state.page.clear();
        state.paths.clear();
        state.total = None;
        ui.set_search_results(ModelRc::default());
        ui.set_result_summary("".into());
        ui.set_failed(false);
        ui.set_search_retry(false);
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
        let provider = search_provider.clone();
        let weak = weak.clone();

        let result_state = callback_state.clone();
        let first_paint = first_paint.clone();
        state.pending = Some(handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let outcome = provider.search(query).await;
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                // Aborting the task cannot retract a completion already queued on the UI thread.
                if ui.get_search_generation() != generation || !ui.get_searching() {
                    return;
                }
                ui.set_status_text("".into());
                receive_page(&ui, &mut result_state.lock().unwrap(), outcome);
                if ui.get_search_results().row_count() > 0 {
                    if paint_supported && ui.window().is_visible() {
                        *first_paint.lock().unwrap() = Some((generation, edited));
                    }
                    debug!(
                        generation,
                        elapsed_us = edited.elapsed().as_micros(),
                        "First search page delivered to UI"
                    );
                }
            });
        }));
    });

    let _coverage_timer = native.as_ref().map(|native| {
        let timer = slint::Timer::default();
        let weak = ui.as_weak();
        let native = native.clone();
        let revision = std::cell::Cell::new(0);
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(200),
            move || {
                let Some(ui) = weak.upgrade() else { return };
                let status = native.status();
                let coverage = coverage_text(&status);
                if ui.get_coverage() != coverage.as_str() {
                    ui.set_coverage(coverage.into());
                }
                if ui.get_index_paused() != status.paused {
                    ui.set_index_paused(status.paused);
                    ui.set_pause_command_pending(false);
                }
                ui.set_index_scanning(status.scanning);
                ui.set_index_needs_retry(status.roots.iter().any(|root| {
                    matches!(root.state, RootState::AccessDenied | RootState::Unavailable)
                }));
                // Give the user's query a chance to paint before refreshing it for a new snapshot.
                let awaiting_paint = ui.window().is_visible()
                    && refresh_paint
                        .lock()
                        .unwrap()
                        .is_some_and(|(_, edited)| edited.elapsed() < Duration::from_secs(1));
                if revision.get() != status.revision && !ui.get_searching() && !awaiting_paint {
                    revision.set(status.revision);
                    ui.invoke_refresh_search();
                }
            },
        );
        timer
    });
    let weak = ui.as_weak();
    let handle = runtime.handle().clone();
    let retry_provider = provider.clone();
    let retry_native = native.clone();
    ui.on_retry_requested(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_index_paused() || ui.get_index_scanning() || ui.get_index_command_pending() {
            return;
        }
        let retry = ui.get_index_needs_retry();
        let (provider, native, weak) = (retry_provider.clone(), retry_native.clone(), weak.clone());
        ui.set_index_command_pending(true);
        ui.set_index_message("".into());
        handle.spawn(async move {
            let result = match &native {
                Some(native) if retry => native.retry_unavailable().await,
                Some(_) => provider.reindex().await,
                None => provider.initialize().await,
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_index_command_pending(false);
                match result {
                    Err(error) => {
                        error!(%error, "Index action failed");
                        ui.set_index_message(format!("{} · {error}", ui.get_backend_name()).into());
                    }
                    Ok(()) if native.is_none() => {
                        ui.set_coverage("Everything CLI · Connected".into());
                        ui.set_failed(false);
                        ui.set_search_retry(false);
                        ui.set_status_text("".into());
                        ui.invoke_refresh_search();
                    }
                    Ok(()) => {}
                }
            });
        });
    });
    let weak = ui.as_weak();
    let handle = runtime.handle().clone();
    let pause_provider = provider.clone();
    ui.on_pause_requested(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_pause_command_pending() {
            return;
        }
        let was_paused = ui.get_index_paused();
        let (provider, weak) = (pause_provider.clone(), weak.clone());
        ui.set_pause_command_pending(true);
        ui.set_index_message("".into());
        handle.spawn(async move {
            let result = if was_paused {
                provider.resume_indexing().await
            } else {
                provider.pause_indexing().await
            };
            if let Err(err) = result {
                error!(%err, "Could not change indexing state");
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_pause_command_pending(false);
                    ui.set_index_message(format!("{} · {err}", ui.get_backend_name()).into());
                });
            }
        });
    });

    let weak = ui.as_weak();
    let filter_state = state.clone();
    ui.on_filter_requested(move |filter| {
        if let Some(ui) = weak.upgrade() {
            ui.set_result_filter(filter);
            if !ui.get_searching() && !ui.get_result_summary().is_empty() {
                show_page(&ui, &filter_state.lock().unwrap());
            }
        }
    });
    for parent in [false, true] {
        let weak = ui.as_weak();
        let state = state.clone();
        let open = move |id: i32| {
            let path = state.lock().unwrap().paths.get(id as usize).cloned();
            if let (Some(ui), Some(path)) = (weak.upgrade(), path) {
                let target = if parent {
                    path.parent().unwrap_or(&path)
                } else {
                    &path
                };
                match open_path(target) {
                    Ok(()) => {
                        debug!(path = %target.display(), "Opened search result");
                        #[cfg(feature = "desktop-acceptance")]
                        desktop_acceptance::record_open(target);
                    }
                    Err(err) => ui.set_status_text(err.into()),
                }
            }
        };
        if parent {
            ui.on_result_opened_folder(open);
        } else {
            ui.on_result_opened(open);
        }
    }
    let weak = ui.as_weak();
    let handle = runtime.handle().clone();
    ui.on_details_requested(move |id| {
        let id = id as usize;
        let Some(ui) = weak.upgrade() else { return };
        let Some(path) = state.lock().unwrap().paths.get(id).cloned() else {
            return;
        };
        let generation = ui.get_search_generation();
        let (weak, state) = (weak.clone(), state.clone());
        handle.spawn(async move {
            let metadata = tokio::fs::metadata(&path).await;
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if generation != ui.get_search_generation() {
                    return;
                }
                let mut state = state.lock().unwrap();
                let Some(row) = state.page.get_mut(id) else {
                    return;
                };
                match metadata {
                    Ok(metadata) => {
                        if !row.is_directory {
                            row.size = format_bytes(metadata.len()).into();
                        }
                        if let Some(modified) = metadata
                            .modified()
                            .ok()
                            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        {
                            row.modified = format_timestamp(modified.as_secs()).into();
                        }
                        use slint::Model;
                        let model = ui.get_search_results();
                        if let Some(position) = model.iter().position(|shown| shown.id == row.id) {
                            model.set_row_data(position, row.clone());
                        }
                    }
                    Err(err) => {
                        ui.set_status_text(format!("File details unavailable: {err}").into())
                    }
                }
            });
        });
    });
    let weak = ui.as_weak();
    ui.on_escape_pressed(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });
    ui.window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);

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
                &[20, 30, 64, 255]
            } else {
                &[168, 189, 255, 255]
            });
        }
    }
    let icon = tray_icon::Icon::from_rgba(pixels, 32, 32)
        .map_err(|err| VelocityError::Config(err.to_string()))?;
    let _tray = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Velocity Search")
        .with_menu(Box::new(menu.clone()))
        .build()
        .map_err(|err| VelocityError::Config(err.to_string()))?;

    let weak = ui.as_weak();
    let (show_id, theme_id, quit_id) = (show.id().clone(), theme.id().clone(), quit.id().clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if event.id == quit_id {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        } else if event.id == show_id {
            let _ = weak.upgrade_in_event_loop(show_window);
        } else if event.id == theme_id {
            let _ = weak.upgrade_in_event_loop(|ui| ui.set_dark(!ui.get_dark()));
        }
    }));
    let weak = ui.as_weak();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if matches!(event.click_type, ClickType::Left | ClickType::Double) {
            let _ = weak.upgrade_in_event_loop(show_window);
        }
    }));
    let weak = ui.as_weak();
    let hotkey = hotkey::Registration::register(config.hotkey.hotkey_hint(), move || {
        let _ = weak.upgrade_in_event_loop(show_window);
    });
    if let Err(error) = &hotkey {
        error!(%error, "Global hotkey unavailable");
        ui.set_hotkey_hint("Shortcut unavailable".into());
    }

    info!("Starting Velocity Search");
    ui.show()?;
    #[cfg(feature = "desktop-acceptance")]
    let acceptance_job = acceptance
        .map(|run| run.start(&ui, &runtime))
        .or_else(|| desktop_acceptance::start_ntfs(&ui, &runtime));
    #[cfg(feature = "desktop-acceptance")]
    let acceptance_job = match acceptance_job {
        Some(job) => Some(job),
        None => desktop_acceptance::start_controls(
            &ui,
            &runtime,
            native.as_ref(),
            &menu,
            hotkey.as_ref().ok(),
        )?,
    };
    slint::run_event_loop_until_quit()?;
    runtime.block_on(provider.shutdown())?;
    drop(hotkey);
    if let Some(mut monitor) = monitor {
        runtime.block_on(async {
            if tokio::time::timeout(Duration::from_millis(750), &mut monitor)
                .await
                .is_err()
            {
                monitor.abort();
            }
        });
    }
    #[cfg(feature = "desktop-acceptance")]
    if let Some(job) = acceptance_job {
        runtime
            .block_on(job)
            .map_err(|err| VelocityError::IndexProvider(err.to_string()))??;
    }
    runtime.shutdown_background();
    Ok(())
}

fn show_window(ui: SearchBar) {
    if let Err(err) = ui.show() {
        error!("Could not show window: {err}");
    }
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    if let Ok(handle) = ui.window().window_handle().window_handle() {
        if let RawWindowHandle::Win32(handle) = handle.as_raw() {
            let hwnd = handle.hwnd.get() as _;
            // Windows applies its normal foreground policy to this request.
            unsafe {
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
            }
        }
    }
    ui.invoke_focus_search();
}
