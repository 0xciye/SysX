//! Starts SysX again when it ends without the user closing it (a crash, the
//! backend dying, the process being killed), so it need not be relaunched by
//! hand. The launched `SysX.exe` stays a small parent that only runs the real
//! app as a child process with [`CHILD_ARG`] and waits for it.

use std::collections::VecDeque;
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
        if status.code() == Some(EXIT_DONE) || !restarts.allow(Instant::now()) {
            return true;
        }
        std::thread::sleep(RESTART_DELAY);
    }
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
}
