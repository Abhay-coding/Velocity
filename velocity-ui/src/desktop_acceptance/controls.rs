use std::{
    fs, io,
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::{Arc, Mutex},
    time::Duration,
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{
    platform::{Key, WindowEvent},
    ComponentHandle as _, Model as _, Weak,
};
use tokio::{
    runtime::Runtime,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tray_icon::menu::{ContextMenu, Menu};
use velocity_core::VelocityResult;
use velocity_native::FolderProvider;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, MOD_ALT,
            MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
        },
        WindowsAndMessaging::{
            EnumThreadWindows, GetClassNameW, GetMenuItemID, PostMessageW, WM_CLOSE, WM_COMMAND,
        },
    },
};

use super::{failure, key, on_ui};
use crate::SearchBar;

static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub(crate) fn record_open(path: &Path) {
    if std::env::var_os("VELOCITY_CONTROLS_ACCEPTANCE").is_some() {
        OPENED.lock().unwrap().push(path.to_path_buf());
    }
}

pub(crate) fn start_controls(
    ui: &SearchBar,
    runtime: &Runtime,
    native: Option<&Arc<FolderProvider>>,
    menu: &Menu,
    hotkey: Option<&crate::hotkey::Registration>,
) -> VelocityResult<Option<JoinHandle<VelocityResult<()>>>> {
    let Some(stage) = std::env::var_os("VELOCITY_CONTROLS_ACCEPTANCE") else {
        return Ok(None);
    };
    let native = native
        .cloned()
        .ok_or_else(|| failure("Desktop controls need the native provider"))?;
    let tray = TrayCommands::new(menu)?;
    let trigger = hotkey
        .ok_or_else(|| failure("Windows did not register the acceptance shortcut"))?
        .post_test_event();
    let weak = ui.as_weak();
    let stage = PathBuf::from(stage);
    Ok(Some(runtime.spawn(async move {
        let result = verify(&weak, &native, &stage, tray, trigger).await;
        if result.is_err() {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        }
        result
    })))
}

async fn verify(
    ui: &Weak<SearchBar>,
    native: &FolderProvider,
    stage: &Path,
    tray: TrayCommands,
    trigger: impl FnOnce() -> io::Result<()>,
) -> VelocityResult<()> {
    ui_wait(ui, "a ready folder and an unavailable root", |ui| {
        ui.get_index_needs_retry()
            && ui.get_coverage().contains("Folder index")
            && ui.get_coverage().contains("unavailable")
    })
    .await?;
    let missing = stage.join("missing");
    fs::create_dir(&missing)?;
    fs::write(missing.join("recovered-root.txt"), "Retry fixture")?;
    on_ui(ui, |ui| ui.invoke_retry_requested()).await?;
    ui_wait(ui, "the root to recover through Retry", |ui| {
        !ui.get_index_needs_retry() && !ui.get_index_command_pending()
    })
    .await?;
    search(ui, "recovered-root").await?;
    ui_wait(ui, "recovered root results", |ui| {
        !ui.get_searching() && ui.get_search_results().row_count() == 1
    })
    .await?;
    println!("Retry recovered an unavailable root");

    search(ui, "report-同名").await?;
    ui_wait(ui, "the two Unicode rows", |ui| {
        !ui.get_searching() && ui.get_search_results().row_count() == 2
    })
    .await?;
    let first = stage.join("healthy/alpha/report-同名.txt");
    let second = stage.join("healthy/beta/report-同名.txt");
    expect_selection(ui, &first).await?;
    press(ui, Key::DownArrow).await?;
    expect_selection(ui, &second).await?;
    press(ui, Key::UpArrow).await?;
    expect_selection(ui, &first).await?;
    press(ui, Key::DownArrow).await?;
    press(ui, Key::Return).await?;
    expect_opened(&second).await?;
    on_ui(ui, |ui| {
        key(&ui, Key::Control);
        key(&ui, Key::Return);
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
    })
    .await?;
    expect_opened(second.parent().unwrap()).await?;

    search(ui, "report").await?;
    ui_wait(ui, "the broad result page", |ui| {
        !ui.get_searching() && ui.get_search_results().row_count() >= 10
    })
    .await?;
    let initial = on_ui(ui, |ui| ui.get_detail().id).await?;
    press(ui, Key::PageDown).await?;
    if on_ui(ui, |ui| ui.get_detail().id).await? <= initial {
        return Err(failure("Page Down did not advance the selection"));
    }
    press(ui, Key::PageUp).await?;
    if on_ui(ui, |ui| ui.get_detail().id).await? != initial {
        return Err(failure("Page Up did not restore the starting selection"));
    }
    on_ui(ui, |ui| ui.invoke_filter_requested(1)).await?;
    ui_wait(ui, "the Files page filter", |ui| {
        let rows = ui.get_search_results();
        rows.row_count() > 0
            && rows.iter().all(|row| !row.is_directory)
            && ui.get_result_summary().contains("displayed results")
    })
    .await?;
    on_ui(ui, |ui| ui.invoke_filter_requested(2)).await?;
    ui_wait(ui, "the Folders page filter", |ui| {
        let rows = ui.get_search_results();
        rows.row_count() == 1 && rows.iter().all(|row| row.is_directory)
    })
    .await?;
    press(ui, Key::Return).await?;
    expect_opened(&stage.join("healthy/report-folder")).await?;
    on_ui(ui, |ui| ui.invoke_filter_requested(0)).await?;
    search(ui, "report").await?;
    sleep(Duration::from_millis(35)).await;
    on_ui(ui, |ui| {
        for ch in "-needle".chars() {
            key(&ui, ch);
        }
    })
    .await?;
    ui_wait(ui, "only the latest rapid query", |ui| {
        let rows = ui.get_search_results();
        !ui.get_searching()
            && rows.row_count() == 1
            && rows
                .row_data(0)
                .is_some_and(|row| row.name == "report-needle.txt")
    })
    .await?;
    sleep(Duration::from_millis(200)).await;
    expect_selection(ui, &stage.join("healthy/report-needle.txt")).await?;
    println!(
        "Unicode selection, keyboard navigation, page filters, shell opens and rapid typing passed"
    );

    on_ui(ui, |ui| ui.invoke_pause_requested()).await?;
    ui_wait(ui, "the paused index", |ui| {
        ui.get_index_paused() && ui.get_coverage().contains("Showing last index")
    })
    .await?;
    fs::rename(
        stage.join("healthy/report-needle.txt"),
        stage.join("healthy/report-after-pause.txt"),
    )?;
    search(ui, "report-needle").await?;
    ui_wait(ui, "the published result while paused", |ui| {
        !ui.get_searching() && ui.get_search_results().row_count() == 1
    })
    .await?;
    on_ui(ui, |ui| ui.invoke_pause_requested()).await?;
    ui_wait(ui, "the active query to refresh after Resume", |ui| {
        !ui.get_index_paused()
            && !ui.get_index_scanning()
            && !ui.get_searching()
            && ui.get_result_summary() == "0 results"
    })
    .await?;
    search(ui, "report-after-pause").await?;
    ui_wait(ui, "the renamed file after Resume", |ui| {
        !ui.get_searching() && ui.get_result_summary() == "1 result"
    })
    .await?;
    let revision = native.status().revision;
    on_ui(ui, |ui| ui.invoke_retry_requested()).await?;
    timeout(Duration::from_secs(10), async {
        while native.status().revision == revision {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| failure("Reindex did not publish a new revision"))?;
    println!("Pause preserved the prior index, Resume caught up, and Reindex published");

    press(ui, Key::Escape).await?;
    ui_wait(ui, "Escape hiding the window", |ui| {
        !ui.window().is_visible()
    })
    .await?;
    let shortcut = on_ui(ui, |ui| ui.get_hotkey_hint().to_string()).await?;
    if std::env::var_os("VELOCITY_CONTROLS_PHYSICAL_HOTKEY").is_some() {
        trigger_hotkey(&shortcut)?;
    } else {
        trigger()?;
    }
    ui_wait(ui, "the registered Windows hotkey showing search", |ui| {
        ui.window().is_visible()
    })
    .await?;
    on_ui(ui, |ui| {
        let handle = ui.window().window_handle();
        let Ok(handle) = handle.window_handle() else {
            return false;
        };
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return false;
        };
        unsafe { PostMessageW(handle.hwnd.get() as HWND, WM_CLOSE, 0, 0) != 0 }
    })
    .await?
    .then_some(())
    .ok_or_else(|| failure("Could not send a window close request"))?;
    ui_wait(ui, "the close button hiding the window", |ui| {
        !ui.window().is_visible()
    })
    .await?;
    tray.post(0)?;
    ui_wait(ui, "Show search from the tray", |ui| {
        ui.window().is_visible()
    })
    .await?;
    let dark = on_ui(ui, |ui| ui.get_dark()).await?;
    tray.post(1)?;
    ui_wait(ui, "the tray theme command", move |ui| {
        ui.get_dark() != dark
    })
    .await?;
    println!("Escape, window close, {shortcut}, and the Windows tray menu passed");

    ui_wait(ui, "a settled index before the shutdown trial", |ui| {
        !ui.get_index_scanning() && !ui.get_index_command_pending()
    })
    .await?;
    fs::rename(
        stage.join("reservoir"),
        stage.join("healthy/rebuild-corpus"),
    )?;
    on_ui(ui, |ui| ui.invoke_retry_requested()).await?;
    timeout(Duration::from_secs(10), async {
        while !native.status().scanning {
            sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .map_err(|_| failure("The rebuild did not start before the quit command"))?;
    println!("Quitting from the tray while the provider is indexing");
    tray.post(2)?;
    Ok(())
}

async fn ui_wait(
    ui: &Weak<SearchBar>,
    what: &str,
    condition: impl Fn(&SearchBar) -> bool + Send + Sync + 'static,
) -> VelocityResult<()> {
    let condition = Arc::new(condition);
    timeout(Duration::from_secs(8), async {
        loop {
            let condition = condition.clone();
            if on_ui(ui, move |ui| condition(&ui)).await? {
                return Ok(());
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| failure(&format!("Timed out waiting for {what}")))?
}

async fn search(ui: &Weak<SearchBar>, text: &'static str) -> VelocityResult<()> {
    on_ui(ui, move |ui| {
        ui.invoke_focus_search();
        key(&ui, Key::Control);
        key(&ui, "a");
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
        for ch in text.chars() {
            key(&ui, ch);
        }
    })
    .await
}

async fn press(ui: &Weak<SearchBar>, value: impl Into<slint::SharedString>) -> VelocityResult<()> {
    let value = value.into();
    on_ui(ui, move |ui| {
        ui.invoke_focus_search();
        key(&ui, value);
    })
    .await
}

async fn expect_selection(ui: &Weak<SearchBar>, path: &Path) -> VelocityResult<()> {
    let expected = path.to_path_buf();
    let shown = on_ui(ui, |ui| ui.get_detail().path.to_string()).await?;
    if Path::new(&shown) != expected {
        return Err(failure(&format!(
            "Expected selected path {}, got {shown}",
            expected.display()
        )));
    }
    Ok(())
}

async fn expect_opened(path: &Path) -> VelocityResult<()> {
    timeout(Duration::from_secs(5), async {
        while OPENED
            .lock()
            .unwrap()
            .last()
            .is_none_or(|opened| opened != path)
        {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| failure(&format!("Windows Shell did not open {}", path.display())))
}

#[derive(Clone, Copy)]
struct TrayCommands {
    window: usize,
    commands: [u32; 3],
}

impl TrayCommands {
    fn new(menu: &Menu) -> VelocityResult<Self> {
        let mut window = null_mut::<std::ffi::c_void>();
        unsafe extern "system" fn find_tray(hwnd: HWND, context: LPARAM) -> i32 {
            let mut class = [0u16; 64];
            let length = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
            if length > 0 && String::from_utf16_lossy(&class[..length as usize]) == "tray_icon_app"
            {
                unsafe { *(context as *mut HWND) = hwnd };
                return 0;
            }
            1
        }
        // The tray crate creates its menu owner on the application's UI thread.
        unsafe {
            EnumThreadWindows(
                GetCurrentThreadId(),
                Some(find_tray),
                (&mut window as *mut HWND) as LPARAM,
            )
        };
        let commands = std::array::from_fn(|index| unsafe {
            GetMenuItemID(menu.hpopupmenu() as _, index as i32)
        });
        if window.is_null() || commands.contains(&u32::MAX) {
            return Err(failure("The Windows tray menu was not created"));
        }
        Ok(Self {
            window: window as usize,
            commands,
        })
    }

    fn post(self, index: usize) -> io::Result<()> {
        // Feed the same menu notification Windows sends to this tray owner.
        if unsafe {
            PostMessageW(
                self.window as HWND,
                WM_COMMAND,
                self.commands[index] as usize,
                0,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn trigger_hotkey(shortcut: &str) -> io::Result<()> {
    let (modifiers, key) = crate::hotkey::parse(shortcut)?;
    let keys: Vec<_> = [
        (MOD_ALT, VK_MENU),
        (MOD_CONTROL, VK_CONTROL),
        (MOD_SHIFT, VK_SHIFT),
        (MOD_WIN, VK_LWIN),
    ]
    .into_iter()
    .filter_map(|(flag, vk)| (modifiers & flag != 0).then_some(vk))
    .chain([key as u16])
    .collect();
    let input = |vk, flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                dwFlags: flags,
                ..Default::default()
            },
        },
    };
    let inputs: Vec<_> = keys
        .iter()
        .map(|&vk| input(vk, 0))
        .chain(keys.iter().rev().map(|&vk| input(vk, KEYEVENTF_KEYUP)))
        .collect();
    if unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    } != inputs.len() as u32
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
