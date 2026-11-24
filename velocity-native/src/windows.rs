use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
    ptr::{null, null_mut},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, GetVolumeInformationW,
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, ReadDirectoryChangesW,
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_SECURITY,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    },
    System::{
        Threading::{CreateEventW, ResetEvent, WaitForSingleObject},
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

use crate::{monitor::Notice, Volume};

pub(crate) fn wide(path: &OsStr) -> Vec<u16> {
    path.encode_wide().chain(Some(0)).collect()
}

pub(crate) struct Handle(pub(crate) HANDLE);

impl Handle {
    pub(crate) fn open(path: &OsStr, access: u32, flags: u32) -> io::Result<Self> {
        let path = wide(path);
        // The UTF-16 path and all optional parameters are valid for this call.
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                flags,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // This wrapper exclusively owns the handle.
        unsafe { CloseHandle(self.0) };
    }
}

pub(crate) fn volume(path: &Path) -> Option<Volume> {
    let path = wide(path.as_os_str());
    let mut mount = vec![0; 32_768];
    let mut filesystem = [0; 256];
    let mut identity = [0; 64];
    let mut serial = 0;
    // Each output buffer is writable for the advertised number of UTF-16 code units.
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), mount.as_mut_ptr(), mount.len() as u32) == 0 {
            return None;
        }
        if GetVolumeInformationW(
            mount.as_ptr(),
            null_mut(),
            0,
            &mut serial,
            null_mut(),
            null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        ) == 0
        {
            return None;
        }
        let has_identity = GetVolumeNameForVolumeMountPointW(
            mount.as_ptr(),
            identity.as_mut_ptr(),
            identity.len() as u32,
        ) != 0;
        Some(Volume {
            mount: PathBuf::from(decode(&mount)),
            filesystem: decode(&filesystem),
            identity: has_identity.then(|| decode(&identity)),
            serial,
        })
    }
}

fn decode(wide: &[u16]) -> String {
    String::from_utf16_lossy(
        &wide[..wide
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(wide.len())],
    )
}

pub(crate) fn watch(
    root: &Path,
    stop: &AtomicBool,
    signal: &mut dyn FnMut(Notice),
) -> io::Result<()> {
    watch_with_buffer(root, stop, &mut [0u32; 8192], signal)
}

fn directory_id(handle: &Handle) -> io::Result<(u32, u32, u32)> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle.0, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        info.dwVolumeSerialNumber,
        info.nFileIndexHigh,
        info.nFileIndexLow,
    ))
}

fn open_directory(root: &Path) -> io::Result<Handle> {
    use std::os::windows::fs::MetadataExt;
    let root_metadata = std::fs::symlink_metadata(root)?;
    if !root_metadata.is_dir() || root_metadata.file_attributes() & 0x400 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "watch root must be a regular directory",
        ));
    }
    let absolute = std::fs::canonicalize(root)?;
    Handle::open(
        absolute.as_os_str(),
        FILE_LIST_DIRECTORY,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OVERLAPPED,
    )
}

pub(crate) fn watch_with_buffer(
    root: &Path,
    stop: &AtomicBool,
    buffer: &mut [u32],
    signal: &mut dyn FnMut(Notice),
) -> io::Result<()> {
    let directory = open_directory(root)?;
    let identity = directory_id(&directory)?;
    let mut last_checked = Instant::now();
    // The thread holds the event, buffer and OVERLAPPED at fixed addresses until
    // the request completes, including after cancellation.
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if event.is_null() {
        return Err(io::Error::last_os_error());
    }
    let event = Handle(event);
    let filters = FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_SECURITY
        | FILE_NOTIFY_CHANGE_ATTRIBUTES;
    while !stop.load(Ordering::Acquire) {
        let mut request = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        let queued = unsafe {
            ResetEvent(event.0);
            ReadDirectoryChangesW(
                directory.0,
                buffer.as_mut_ptr().cast(),
                size_of_val(buffer) as u32,
                1,
                filters,
                null_mut(),
                &mut request,
                None,
            )
        };
        if queued == 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error().map(|code| code as u32) {
                Some(ERROR_IO_PENDING) => {}
                Some(ERROR_NOTIFY_ENUM_DIR) => {
                    signal(Notice::Scan("notification buffer overflow"));
                    continue;
                }
                _ => return Err(error),
            }
        }
        signal(Notice::Registered);
        let mut transferred = 0;
        loop {
            let waited = unsafe { WaitForSingleObject(event.0, 200) };
            let stopped = stop.load(Ordering::Acquire);
            let status = if stopped {
                Ok(())
            } else if ![WAIT_OBJECT_0, WAIT_TIMEOUT].contains(&waited) {
                Err(io::Error::last_os_error())
            } else if last_checked.elapsed() >= Duration::from_secs(1) {
                last_checked = Instant::now();
                open_directory(root)
                    .and_then(|current| directory_id(&current))
                    .and_then(|current| {
                        if current == identity {
                            Ok(())
                        } else {
                            Err(io::Error::new(
                                io::ErrorKind::NotFound,
                                "watch root identity changed",
                            ))
                        }
                    })
            } else {
                Ok(())
            };
            if stopped || status.is_err() {
                unsafe {
                    CancelIoEx(directory.0, &request);
                    GetOverlappedResult(directory.0, &request, &mut transferred, 1);
                }
                return status;
            }
            if waited == WAIT_OBJECT_0 {
                break;
            }
        }
        if unsafe { GetOverlappedResult(directory.0, &request, &mut transferred, 0) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                return Err(error);
            }
            signal(Notice::Scan("notification buffer overflow"));
            continue;
        }
        signal(if transferred == 0 {
            Notice::Scan("notification buffer overflow")
        } else {
            // Only the completed portion of the aligned buffer contains records.
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), size_of_val(buffer)) };
            bytes
                .get(..transferred as usize)
                .and_then(notification_paths)
                .map_or(
                    Notice::Scan("directory attributes or permissions changed"),
                    Notice::Paths,
                )
        });
    }
    Ok(())
}

fn notification_paths(mut records: &[u8]) -> Option<HashSet<PathBuf>> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ACTION_ADDED, FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_NEW_NAME,
        FILE_ACTION_RENAMED_OLD_NAME,
    };
    let mut paths = HashSet::new();
    loop {
        let (header, data) = records.split_first_chunk::<12>()?;
        let next = u32::from_le_bytes(header[..4].try_into().ok()?) as usize;
        let action = u32::from_le_bytes(header[4..8].try_into().ok()?);
        let length = u32::from_le_bytes(header[8..12].try_into().ok()?) as usize;
        if length == 0
            || !length.is_multiple_of(2)
            || !matches!(
                action,
                FILE_ACTION_ADDED
                    | FILE_ACTION_REMOVED
                    | FILE_ACTION_RENAMED_OLD_NAME
                    | FILE_ACTION_RENAMED_NEW_NAME
            )
        {
            return None;
        }
        let units: Vec<_> = data
            .get(..length)?
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect();
        let path = PathBuf::from(OsString::from_wide(&units));
        if units.contains(&0)
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return None;
        }
        paths.insert(path);
        if next == 0 {
            return Some(paths);
        }
        if next < 12 + length {
            return None;
        }
        records = records.get(next..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{mpsc, Arc},
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn oversized_notification_reports_overflow() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("velocity-overflow-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let (send, receive) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread_root = root.clone();
        let watcher = thread::spawn(move || {
            // Four DWORDs cannot hold a notification for the longer file name.
            watch_with_buffer(&thread_root, &thread_stop, &mut [0; 4], &mut |reason| {
                let _ = send.send(reason);
            })
        });
        let ready = receive.recv_timeout(Duration::from_secs(3));
        let created = fs::write(
            root.join("a-file-name-that-overflows-the-test-buffer.txt"),
            b"change",
        );
        let observed = receive.recv_timeout(Duration::from_secs(3));
        stop.store(true, Ordering::Release);
        let result = watcher.join().unwrap();
        fs::remove_dir_all(&root).unwrap();
        result.unwrap();
        assert!(matches!(ready.unwrap(), Notice::Registered));
        created.unwrap();
        assert!(matches!(
            observed.unwrap(),
            Notice::Scan("notification buffer overflow")
        ));
    }
}
