//! One SysX per desktop session. A second launch does not start another
//! backend (it could not bind UDP 4200 anyway); it brings the running
//! instance's monitor window to the front, or reopens it, and exits.
#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(
            attributes: *mut c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn GetLastError() -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn CreateEventW(
            attributes: *mut c_void,
            manual: i32,
            initial: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        fn SetEvent(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn FindWindowW(class_name: *const u16, window_name: *const u16) -> *mut c_void;
        fn IsIconic(window: *mut c_void) -> i32;
        fn IsWindowVisible(window: *mut c_void) -> i32;
        fn ShowWindow(window: *mut c_void, command: i32) -> i32;
        fn SetForegroundWindow(window: *mut c_void) -> i32;
    }

    const ERROR_ALREADY_EXISTS: u32 = 183;
    const SW_RESTORE: i32 = 9;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Held for the process lifetime; Windows releases the mutex when the
    /// process exits, so a crashed instance never blocks the next launch.
    pub struct InstanceGuard(*mut c_void);

    impl Drop for InstanceGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    pub struct MonitorSignal(*mut c_void);
    impl MonitorSignal {
        pub fn new() -> Self {
            Self(unsafe {
                CreateEventW(
                    std::ptr::null_mut(),
                    0,
                    0,
                    wide("Local\\SysX.NativeMonitor.Show").as_ptr(),
                )
            })
        }
        pub fn requested(&self) -> bool {
            !self.0.is_null() && unsafe { WaitForSingleObject(self.0, 0) } == 0
        }
    }
    impl Drop for MonitorSignal {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }
    pub fn request_monitor() -> bool {
        let handle = unsafe { OpenEventW(2, 0, wide("Local\\SysX.NativeMonitor.Show").as_ptr()) };
        if handle.is_null() {
            return false;
        }
        let result = unsafe { SetEvent(handle) } != 0;
        unsafe {
            CloseHandle(handle);
        }
        result
    }

    /// `None` when another SysX already holds the instance mutex.
    pub fn acquire() -> Option<InstanceGuard> {
        let name = wide("Local\\SysX.Receiver.SingleInstance");
        let handle = unsafe { CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr()) };
        // Read the error immediately; nothing may run between the two calls.
        let error = unsafe { GetLastError() };
        if handle.is_null() {
            // Cannot tell: run normally rather than refuse to start.
            return Some(InstanceGuard(handle));
        }
        if error == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(InstanceGuard(handle))
    }

    /// Focuses the receiver window titled exactly "SysX" (the native app
    /// window). Browser tabs carry a
    /// " - Microsoft Edge" style suffix, so they never match.
    pub fn focus_monitor_window() -> bool {
        let title = wide("SysX");
        let window = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
        if window.is_null() || unsafe { IsWindowVisible(window) } == 0 {
            return false;
        }
        unsafe {
            if IsIconic(window) != 0 {
                ShowWindow(window, SW_RESTORE);
            }
            SetForegroundWindow(window);
        }
        true
    }
}

#[cfg(target_os = "windows")]
pub use win::{acquire, focus_monitor_window, request_monitor, MonitorSignal};

#[cfg(target_os = "macos")]
mod mac {
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    extern "C" {
        fn flock(fd: i32, operation: i32) -> i32;
    }

    fn ipc_path() -> PathBuf {
        std::env::temp_dir().join("sysx-native-monitor.sock")
    }

    pub struct InstanceGuard {
        _file: Option<File>,
    }

    pub struct MonitorSignal {
        listener: Option<UnixListener>,
        path: PathBuf,
    }

    impl MonitorSignal {
        pub fn new() -> Self {
            Self::at(ipc_path())
        }

        fn at(path: PathBuf) -> Self {
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).ok();
            if let Some(listener) = &listener {
                let _ = listener.set_nonblocking(true);
            }
            Self { listener, path }
        }

        pub fn requested(&self) -> bool {
            self.listener
                .as_ref()
                .is_some_and(|listener| match listener.accept() {
                    Ok((stream, _)) => {
                        drop(stream);
                        true
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(_) => false,
                })
        }
    }

    impl Drop for MonitorSignal {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub fn request_monitor() -> bool {
        request_monitor_at(&ipc_path())
    }

    fn request_monitor_at(path: &Path) -> bool {
        for _ in 0..30 {
            if UnixStream::connect(path).is_ok() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    pub fn acquire() -> Option<InstanceGuard> {
        acquire_at(&std::env::temp_dir().join("sysx-receiver.lock"))
    }

    fn acquire_at(path: &Path) -> Option<InstanceGuard> {
        let Ok(file) = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)
        else {
            return Some(InstanceGuard { _file: None });
        };
        // flock is released by the OS even after a crash; the file itself is intentionally kept.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
            Some(InstanceGuard { _file: Some(file) })
        } else {
            None
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{acquire_at, request_monitor_at, MonitorSignal};
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        fn unique_path(suffix: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!(
                "sysx-macos-{}-{}-{suffix}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
        }

        #[test]
        fn lock_allows_one_owner_and_recovers_after_drop() {
            let path = unique_path("lock");
            let owner = acquire_at(&path).expect("first instance acquires lock");
            assert!(acquire_at(&path).is_none());
            drop(owner);
            assert!(acquire_at(&path).is_some());
            let _ = fs::remove_file(path);
        }

        #[test]
        fn second_launch_signal_is_received_by_the_running_monitor() {
            let path = unique_path("sock");
            let signal = MonitorSignal::at(path.clone());
            assert!(request_monitor_at(&path));
            assert!(signal.requested());
            drop(signal);
            assert!(!path.exists());
        }
    }
}

#[cfg(target_os = "macos")]
pub use mac::{acquire, request_monitor, MonitorSignal};
#[cfg(target_os = "macos")]
pub fn focus_monitor_window() -> bool {
    false
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub struct InstanceGuard;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn acquire() -> Option<InstanceGuard> {
    Some(InstanceGuard)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn focus_monitor_window() -> bool {
    false
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub struct MonitorSignal;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl MonitorSignal {
    pub fn new() -> Self {
        Self
    }
    pub fn requested(&self) -> bool {
        false
    }
}
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub fn request_monitor() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn second_acquire_in_the_same_session_is_refused() {
        let first = acquire().expect("first instance");
        assert!(acquire().is_none());
        drop(first);
        assert!(acquire().is_some());
    }
}
