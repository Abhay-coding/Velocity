use std::{io, ptr::null_mut, sync::mpsc, thread};

use windows_sys::Win32::{
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{
            RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT,
            MOD_WIN, VK_ESCAPE, VK_F1, VK_F12, VK_RETURN, VK_SPACE, VK_TAB,
        },
        WindowsAndMessaging::{GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY, WM_QUIT},
    },
};

const HOTKEY_ID: i32 = 1;

pub(crate) struct Registration {
    thread_id: u32,
    worker: Option<thread::JoinHandle<()>>,
}

impl Registration {
    #[cfg(feature = "desktop-acceptance")]
    pub(crate) fn post_test_event(&self) -> impl FnOnce() -> io::Result<()> + Send + 'static {
        let thread_id = self.thread_id;
        move || {
            if unsafe { PostThreadMessageW(thread_id, WM_HOTKEY, HOTKEY_ID as usize, 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    pub(crate) fn register(
        shortcut: &str,
        activate: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        let (modifiers, key) = parse(shortcut)?;
        let (send, receive) = mpsc::sync_channel(0);
        let worker = thread::Builder::new()
            .name("velocity-hotkey".into())
            .spawn(move || {
                // A null HWND registers on this worker's Windows message queue.
                if unsafe { RegisterHotKey(null_mut(), HOTKEY_ID, modifiers | MOD_NOREPEAT, key) }
                    == 0
                {
                    let _ = send.send(Err(io::Error::last_os_error()));
                    return;
                }
                let thread_id = unsafe { GetCurrentThreadId() };
                if send.send(Ok(thread_id)).is_ok() {
                    let mut message = MSG::default();
                    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
                        if message.message == WM_HOTKEY && message.wParam == HOTKEY_ID as usize {
                            tracing::debug!("Windows delivered the registered shortcut");
                            activate();
                        }
                    }
                }
                unsafe { UnregisterHotKey(null_mut(), HOTKEY_ID) };
            })?;
        let thread_id = receive.recv().map_err(io::Error::other)??;
        Ok(Self {
            thread_id,
            worker: Some(worker),
        })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // The registered thread owns the message queue and wakes on WM_QUIT.
        if unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } != 0 {
            let _ = self.worker.take().unwrap().join();
        }
    }
}

pub(crate) fn parse(shortcut: &str) -> io::Result<(u32, u32)> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Unsupported shortcut {shortcut}"),
        )
    };
    let (modifiers, key) = shortcut.rsplit_once('+').ok_or_else(invalid)?;
    let mut flags = 0;
    for part in modifiers.split('+') {
        let flag = match part.trim().to_ascii_lowercase().as_str() {
            "alt" => MOD_ALT,
            "ctrl" | "control" => MOD_CONTROL,
            "shift" => MOD_SHIFT,
            "win" | "meta" | "super" => MOD_WIN,
            _ => return Err(invalid()),
        };
        if flags & flag != 0 {
            return Err(invalid());
        }
        flags |= flag;
    }
    let key = key.trim().to_ascii_uppercase();
    let vk = match key.as_str() {
        "SPACE" => VK_SPACE,
        "TAB" => VK_TAB,
        "ENTER" | "RETURN" => VK_RETURN,
        "ESC" | "ESCAPE" => VK_ESCAPE,
        other if matches!(other.as_bytes(), [b'A'..=b'Z' | b'0'..=b'9']) => {
            u16::from(other.as_bytes()[0])
        }
        other => {
            let number = other
                .strip_prefix('F')
                .and_then(|suffix| suffix.parse::<u16>().ok())
                .filter(|number| (1..=24).contains(number))
                .ok_or_else(invalid)?;
            let vk = VK_F1 + number - 1;
            if vk == VK_F12 {
                return Err(invalid());
            }
            vk
        }
    };
    Ok((flags, u32::from(vk)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_and_custom_windows_shortcuts() {
        assert_eq!(parse("Alt+Space").unwrap(), (MOD_ALT, VK_SPACE.into()));
        assert_eq!(
            parse("ctrl + Shift + f9").unwrap(),
            (MOD_CONTROL | MOD_SHIFT, (VK_F1 + 8).into())
        );
        for shortcut in ["Space", "Ctrl+Ctrl+F9", "Alt+F12", "Win+Fn", "Alt+"] {
            assert!(parse(shortcut).is_err(), "{shortcut}");
        }
    }
}
