//! Cold-path diagnostics shared by the native host and backend.
use std::backtrace::Backtrace;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

struct Session {
    log: PathBuf,
    dump: PathBuf,
}

static SESSION: OnceLock<Session> = OnceLock::new();

pub fn install(component: &str) -> io::Result<()> {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("SysX");
    let logs = root.join("logs");
    let crashes = root.join("crashes");
    fs::create_dir_all(&logs)?;
    fs::create_dir_all(&crashes)?;
    let stamp = format!("{component}-{}-{}", timestamp_ms(), std::process::id());
    let log = logs.join(format!("{stamp}.log"));
    OpenOptions::new().create_new(true).write(true).open(&log)?;
    if SESSION.set(Session { log, dump: crashes.join(format!("{stamp}.dmp")) }).is_err() {
        return Ok(());
    }
    record(&format!("startup component={component} pid={} version={}", std::process::id(), env!("CARGO_PKG_VERSION")));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record(&format!("Rust panic: {info}\n{}", Backtrace::force_capture()));
        #[cfg(windows)]
        write_dump(std::ptr::null());
        previous(info);
    }));
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(unhandled_exception));
    }
    Ok(())
}

pub fn record(message: &str) {
    if let Some(session) = SESSION.get() {
        let _ = append(&session.log, message);
    }
}

fn append(path: &Path, message: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    writeln!(file, "[unix_ms={}] {message}", timestamp_ms())?;
    file.flush()
}

/// Only backend stderr is redirected; protocol stdout stays piped to the host.
pub fn backend_stderr() -> io::Result<File> {
    let session = SESSION.get().ok_or_else(|| io::Error::other("diagnostics not initialized"))?;
    let name = session.log.file_stem().unwrap_or_default().to_string_lossy();
    OpenOptions::new().create(true).append(true)
        .open(session.log.with_file_name(format!("backend-stderr-{name}.log")))
}

fn timestamp_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
}

#[cfg(windows)]
unsafe extern "system" fn unhandled_exception(
    pointers: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    if let Some(info) = pointers.as_ref() {
        if let Some(exception) = info.ExceptionRecord.as_ref() {
            record(&format!("unhandled Windows exception code=0x{:08x} address={:p}",
                exception.ExceptionCode, exception.ExceptionAddress));
        }
    }
    write_dump(pointers);
    0 // EXCEPTION_CONTINUE_SEARCH: preserve normal Windows crash handling.
}

#[cfg(windows)]
fn write_dump(pointers: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        MiniDumpNormal, MiniDumpWriteDump, MINIDUMP_EXCEPTION_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};
    static WRITING: AtomicBool = AtomicBool::new(false);
    if WRITING.swap(true, Ordering::AcqRel) { return; }
    let Some(session) = SESSION.get() else {
        WRITING.store(false, Ordering::Release);
        return;
    };
    let result = (|| -> io::Result<()> {
        let file = File::create(&session.dump)?;
        let exception = MINIDUMP_EXCEPTION_INFORMATION {
            ThreadId: unsafe { GetCurrentThreadId() },
            ExceptionPointers: pointers.cast_mut(),
            ClientPointers: 0,
        };
        let ok = unsafe {
            MiniDumpWriteDump(GetCurrentProcess(), GetCurrentProcessId(), file.as_raw_handle(),
                MiniDumpNormal, if pointers.is_null() { std::ptr::null() } else { &exception },
                std::ptr::null(), std::ptr::null())
        };
        if ok == 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    })();
    match result {
        Ok(()) => record(&format!("minidump saved: {}", session.dump.display())),
        Err(error) => {
            let _ = fs::remove_file(&session.dump);
            record(&format!("minidump failed: {error}"));
        }
    }
    WRITING.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_records_append_without_overwriting_prior_errors() {
        let path = std::env::temp_dir().join(format!("sysx-log-test-{}-{}.log", std::process::id(), timestamp_ms()));
        File::create(&path).unwrap();
        append(&path, "first failure").unwrap();
        append(&path, "second failure").unwrap();
        let log = fs::read_to_string(&path).unwrap();
        assert!(log.contains("first failure\n"));
        assert!(log.contains("second failure\n"));
        assert_eq!(log.lines().count(), 2);
        fs::remove_file(path).unwrap();
    }
}
