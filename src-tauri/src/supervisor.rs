//! Starts SysX again when it ends without the user closing it (a crash, the
//! backend dying, the process being killed), so it need not be relaunched by
//! hand. The launched `SysX.exe` stays a small parent that only runs the real
//! app as a child process with [`CHILD_ARG`] and waits for it.
//!
//! Each restart adds one line to `last-exit.txt` in app data (user request,
//! 2026-11-01, to find why SysX sometimes ends mid-use): the time, the
//! child's exit code (an NTSTATUS such as `0xC0000005` for a crash, 3 when
//! SysX ended itself) and the reason the child recorded, if any. Nothing is
//! written while SysX runs normally or when the user closes it.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Marks the child that runs the app itself.
pub const CHILD_ARG: &str = "--supervised";
/// Exit code of a run that should not be restarted: the user closed SysX, or
/// another SysX was already running.
pub const EXIT_DONE: i32 = 0;
/// Exit code of a run that ended for any other reason.
pub const EXIT_RESTART: i32 = 3;

const RESTART_DELAY: Duration = Duration::from_secs(2);
/// More restarts than this within [`RESTART_WINDOW`] means SysX cannot run
/// (for example the backend fails at every start); stop instead of looping.
const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// The child writes why it ended here; the supervisor moves it into
/// [`EXIT_LOG`].
const EXIT_REASON: &str = "exit-reason.txt";
const EXIT_LOG: &str = "last-exit.txt";
/// Lines kept in [`EXIT_LOG`].
const EXIT_LOG_LINES: usize = 20;

pub fn is_child() -> bool {
    std::env::args().skip(1).any(|arg| arg == CHILD_ARG)
}

/// Runs the app as a child until it ends as intended. Returns false when no
/// child could be started at all, so the caller runs the app in-process.
pub fn run() -> bool {
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let mut restarts = Restarts::default();
    let mut started_once = false;
    loop {
        let Ok(status) = Command::new(&executable).arg(CHILD_ARG).status() else {
            return started_once;
        };
        started_once = true;
        if status.code() == Some(EXIT_DONE) {
            return true;
        }
        let allowed = restarts.allow(Instant::now());
        record_exit(
            &crate::native_app::app_data_dir(),
            status.code(),
            &local_time(),
            !allowed,
        );
        if !allowed {
            return true;
        }
        std::thread::sleep(RESTART_DELAY);
    }
}

/// Called by the child before it ends with [`EXIT_RESTART`].
pub fn record_reason(reason: &str) {
    let dir = crate::native_app::app_data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(EXIT_REASON), reason);
}

fn record_exit(dir: &Path, code: Option<i32>, time: &str, gave_up: bool) {
    let reason_path = dir.join(EXIT_REASON);
    let reason = std::fs::read_to_string(&reason_path).unwrap_or_default();
    let _ = std::fs::remove_file(&reason_path);
    let code = match code {
        Some(code) => format!("0x{:08X}", code as u32),
        None => "none".into(),
    };
    let reason = reason.trim().replace(['\r', '\n'], " ");
    let mut line = format!(
        "{time}  exit={code}  {}",
        if reason.is_empty() {
            "no reason recorded (crashed or killed)"
        } else {
            &reason
        }
    );
    if gave_up {
        line.push_str("  [too many restarts, stopped]");
    }
    let path = dir.join(EXIT_LOG);
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().filter(|line| !line.is_empty()).collect();
    lines.push(&line);
    let keep = &lines[lines.len().saturating_sub(EXIT_LOG_LINES)..];
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&path, keep.join("\r\n") + "\r\n");
}

#[cfg(windows)]
fn local_time() -> String {
    // SAFETY: GetLocalTime only fills the struct.
    let time = unsafe {
        let mut time = std::mem::zeroed::<windows_sys::Win32::Foundation::SYSTEMTIME>();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
        time
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        time.wYear, time.wMonth, time.wDay, time.wHour, time.wMinute, time.wSecond
    )
}

#[cfg(not(windows))]
fn local_time() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_secs());
    format!("unix {secs}")
}

#[derive(Default)]
struct Restarts(VecDeque<Instant>);

impl Restarts {
    fn allow(&mut self, now: Instant) -> bool {
        self.0
            .retain(|at| now.saturating_duration_since(*at) < RESTART_WINDOW);
        if self.0.len() >= MAX_RESTARTS {
            return false;
        }
        self.0.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restarts_stop_after_five_within_a_minute_and_resume_later() {
        let start = Instant::now();
        let mut restarts = Restarts::default();
        for second in 0..5 {
            assert!(restarts.allow(start + Duration::from_secs(second)));
        }
        assert!(!restarts.allow(start + Duration::from_secs(6)));
        assert!(restarts.allow(start + Duration::from_secs(61)));
    }

    #[test]
    fn exits_are_recorded_with_code_and_reason_keeping_the_last_lines() {
        let dir = std::env::temp_dir().join(format!("sysx-exit-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(EXIT_REASON), "backend connection lost\n").unwrap();
        record_exit(&dir, Some(EXIT_RESTART), "t1", false);
        assert!(!dir.join(EXIT_REASON).exists(), "the reason is used once");
        record_exit(&dir, Some(0xC000_0005_u32 as i32), "t2", false);
        let log = std::fs::read_to_string(dir.join(EXIT_LOG)).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0], "t1  exit=0x00000003  backend connection lost");
        assert_eq!(
            lines[1],
            "t2  exit=0xC0000005  no reason recorded (crashed or killed)"
        );
        for n in 0..30 {
            record_exit(&dir, Some(3), &format!("x{n}"), n == 29);
        }
        let log = std::fs::read_to_string(dir.join(EXIT_LOG)).unwrap();
        assert_eq!(log.lines().count(), EXIT_LOG_LINES);
        assert!(log.lines().last().unwrap().ends_with("[too many restarts, stopped]"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
