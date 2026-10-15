//! One SysX per Windows session. A second launch does not start another
//! backend (it could not bind UDP 4200 anyway); it brings the running
//! instance's monitor window to the front, or reopens it, and exits.
use std::path::Path;

const INSTANCE_FILE: &str = "instance.json";

/// Remembers the running instance's loopback web port so a second launch can
/// reopen its monitor page.
pub fn record_config_port(data_dir: &Path, config_port: u16) {
    let _ = std::fs::write(
        data_dir.join(INSTANCE_FILE),
        serde_json::json!({ "config_port": config_port }).to_string(),
    );
}

pub fn clear_config_port(data_dir: &Path) {
    let _ = std::fs::remove_file(data_dir.join(INSTANCE_FILE));
}

pub fn running_config_port(data_dir: &Path) -> Option<u16> {
    let text = std::fs::read_to_string(data_dir.join(INSTANCE_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    u16::try_from(value.get("config_port")?.as_u64()?)
        .ok()
        .filter(|port| *port > 0)
}

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
    }

    #[link(name = "user32")]
    extern "system" {
        fn FindWindowW(class_name: *const u16, window_name: *const u16) -> *mut c_void;
        fn IsIconic(window: *mut c_void) -> i32;
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

    /// Focuses the Edge app window titled exactly "SysX" (the monitor).
    /// Browser tabs carry a " - Microsoft Edge" suffix, so they never match.
    pub fn focus_monitor_window() -> bool {
        let class = wide("Chrome_WidgetWin_1");
        let title = wide("SysX");
        let window = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
        if window.is_null() {
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
pub use win::{acquire, focus_monitor_window};

#[cfg(not(target_os = "windows"))]
pub struct InstanceGuard;

#[cfg(not(target_os = "windows"))]
pub fn acquire() -> Option<InstanceGuard> {
    Some(InstanceGuard)
}

#[cfg(not(target_os = "windows"))]
pub fn focus_monitor_window() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_port_round_trips_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("sysx-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(running_config_port(&dir), None);
        record_config_port(&dir, 4202);
        assert_eq!(running_config_port(&dir), Some(4202));
        std::fs::write(dir.join(INSTANCE_FILE), "{\"config_port\":0}").unwrap();
        assert_eq!(running_config_port(&dir), None);
        std::fs::write(dir.join(INSTANCE_FILE), "not json").unwrap();
        assert_eq!(running_config_port(&dir), None);
        clear_config_port(&dir);
        assert!(!dir.join(INSTANCE_FILE).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn second_acquire_in_the_same_session_is_refused() {
        let first = acquire().expect("first instance");
        assert!(acquire().is_none());
        drop(first);
        assert!(acquire().is_some());
    }
}
