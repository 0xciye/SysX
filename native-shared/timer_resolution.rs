//! Windows wakes timed waits (condvar and channel timeouts) on the system
//! timer, which ticks every 15.6 ms by default. The 4 ms movement ticker,
//! batcher, and device loop would then run near 64 Hz instead of 250 Hz.
//!
//! macOS has no timer period, but App Nap and timer coalescing slow an app
//! whose windows are hidden or covered (SysX runs behind the game): a
//! latency-critical activity keeps both off for the life of the process.

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
        unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                (&state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            );
            timeBeginPeriod(1);
        }
    }
    #[cfg(target_os = "macos")]
    mac::begin_latency_critical_activity();
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void};

    type Id = *mut c_void;

    // NSProcessInfo lives in Foundation; the runtime functions in libobjc.
    #[link(name = "Foundation", kind = "framework")]
    extern "C" {}

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Id;
        fn objc_msgSend();
    }

    // NSActivityUserInitiated (includes NSActivityIdleSystemSleepDisabled)
    // | NSActivityLatencyCritical.
    const OPTIONS: u64 = 0x00FF_FFFF | 0xFF_0000_0000;

    pub(super) fn begin_latency_critical_activity() {
        // SAFETY: Objective-C runtime calls with each message cast to its
        // exact signature; the class and selectors are part of Foundation.
        unsafe {
            let send0: unsafe extern "C" fn(Id, Id) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let send_str: unsafe extern "C" fn(Id, Id, *const c_char) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let begin: unsafe extern "C" fn(Id, Id, u64, Id) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let process_info = objc_getClass(c"NSProcessInfo".as_ptr());
            let string = objc_getClass(c"NSString".as_ptr());
            if process_info.is_null() || string.is_null() {
                return;
            }
            let info = send0(process_info, sel_registerName(c"processInfo".as_ptr()));
            let reason = send_str(
                string,
                sel_registerName(c"stringWithUTF8String:".as_ptr()),
                c"SysX receives frames and drives the mouse in real time".as_ptr(),
            );
            let activity = begin(
                info,
                sel_registerName(c"beginActivityWithOptions:reason:".as_ptr()),
                OPTIONS,
                reason,
            );
            if activity.is_null() {
                return;
            }
            // Keep the activity (and so the assertion) for the process life;
            // an enclosing autorelease pool must not free it.
            send0(activity, sel_registerName(c"retain".as_ptr()));
        }
    }
}
