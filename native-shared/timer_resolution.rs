//! Windows wakes timed waits (condvar and channel timeouts) on the system
//! timer, which ticks every 15.6 ms by default. The 4 ms movement ticker,
//! batcher, and device loop would then run near 64 Hz instead of 250 Hz.

/// Request 1 ms timer resolution for the rest of this process's life (the
/// system releases it at exit).
pub fn request_fine_timer() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Media::timeBeginPeriod;
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        };
        // Windows 11 ignores the request for windowless, minimized, or
        // occluded processes unless they opt out of that throttling.
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        // SAFETY: plain Win32 calls with a valid, correctly sized struct.
        let (opted_out, period) = unsafe {
            let opted_out = SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
            (opted_out, timeBeginPeriod(1))
        };
        if opted_out == 0 || period != 0 {
            crate::diagnostics::record(&format!(
                "1 ms timer resolution not fully granted (throttling opt-out={}, timeBeginPeriod={period})",
                opted_out != 0
            ));
        }
    }
}
