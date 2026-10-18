//! Cold-path diagnostics shared by the native host and backend.
use std::backtrace::Backtrace;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

struct Session {
    log: PathBuf,
    logs: PathBuf,
    crashes: PathBuf,
    dump: PathBuf,
    report: PathBuf,
    marker: PathBuf,
    component: String,
    started_ms: u128,
    related_logs: Vec<PathBuf>,
}

static SESSION: OnceLock<Session> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn install(component: &str) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("SysX");
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Library")
        .join("Logs")
        .join("SysX");
    // Other platforms are development hosts only; LOCALAPPDATA lets a test
    // isolate the folder there as on Windows.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("SysX");
    let logs = root.join("logs");
    let crashes = root.join("crashes");
    let sessions = root.join("sessions");
    fs::create_dir_all(&logs)?;
    fs::create_dir_all(&crashes)?;
    fs::create_dir_all(&sessions)?;
    let stamp = format!("{component}-{}-{}", timestamp_ms(), std::process::id());
    let started_ms = timestamp_ms();
    let log = logs.join(format!("{stamp}.log"));
    OpenOptions::new().create_new(true).write(true).open(&log)?;
    if SESSION
        .set(Session {
            log,
            logs,
            crashes: crashes.clone(),
            dump: crashes.join(format!("{stamp}.dmp")),
            report: crashes.join(format!("{stamp}-crash.txt")),
            marker: sessions.join(format!("{component}.active")),
            component: component.to_owned(),
            started_ms,
            related_logs: ["SYSX_PARENT_LOG", "SYSX_PARENT_BACKEND_STDERR"]
                .into_iter()
                .filter_map(std::env::var_os)
                .map(PathBuf::from)
                .collect(),
        })
        .is_err()
    {
        return Ok(());
    }
    record(&format!(
        "startup component={component} pid={} version={}",
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    ));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record(&format!(
            "Rust panic: {info}\n{}",
            Backtrace::force_capture()
        ));
        #[cfg(windows)]
        request_dump(std::ptr::null());
        write_crash_report(&format!("Rust panic: {info}"));
        previous(info);
    }));
    #[cfg(windows)]
    {
        set_exception_filter();
        start_dump_writer();
        // C code (libjpeg-turbo, ABCurves) that calls abort() raises SIGABRT
        // first; without this it ends the process with no dump.
        extern "C" {
            fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
        }
        const SIGABRT: i32 = 22;
        unsafe {
            signal(SIGABRT, on_abort);
        }
    }
    Ok(())
}

#[cfg(windows)]
extern "C" fn on_abort(_: i32) {
    request_dump(std::ptr::null());
}

#[cfg(windows)]
fn set_exception_filter() {
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(
            unhandled_exception,
        ));
    }
}

/// A dump is written from this thread, not the crashing one: after a stack
/// overflow the crashing thread has only a few KiB left, and formatting,
/// file I/O and MiniDumpWriteDump there overflow again and kill the process
/// before any dump exists.
#[cfg(windows)]
static DUMP_WRITER: OnceLock<std::thread::Thread> = OnceLock::new();
#[cfg(windows)]
static DUMP_PENDING: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static DUMP_DONE: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static DUMP_POINTERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static DUMP_THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(windows)]
fn start_dump_writer() {
    let spawned = std::thread::Builder::new()
        .name("sysx-dump-writer".into())
        .spawn(|| loop {
            std::thread::park_timeout(std::time::Duration::from_secs(2));
            // Graphics drivers and overlays (GPU, RTSS, Discord, Steam) load
            // later and often install their own filter, which would send our
            // crashes to Windows Error Reporting instead of a dump.
            set_exception_filter();
            if DUMP_PENDING.swap(false, Ordering::AcqRel) {
                write_dump(
                    DUMP_POINTERS.load(Ordering::Acquire) as *const _,
                    DUMP_THREAD.load(Ordering::Acquire),
                );
                DUMP_DONE.store(true, Ordering::Release);
            }
        });
    if let Ok(handle) = spawned {
        let _ = DUMP_WRITER.set(handle.thread().clone());
    }
}

/// Write a dump of the current thread's crash, through the writer thread when
/// it runs (waiting up to 30 s for it), else in place.
#[cfg(windows)]
fn request_dump(
    pointers: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    let thread = unsafe { GetCurrentThreadId() };
    let Some(writer) = DUMP_WRITER
        .get()
        .filter(|writer| writer.id() != std::thread::current().id())
    else {
        write_dump(pointers, thread);
        return;
    };
    DUMP_POINTERS.store(pointers as usize, Ordering::Release);
    DUMP_THREAD.store(thread, Ordering::Release);
    DUMP_DONE.store(false, Ordering::Release);
    DUMP_PENDING.store(true, Ordering::Release);
    writer.unpark();
    for _ in 0..30_000 {
        if DUMP_DONE.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Mark the owning process after singleton acquisition, so a second launch
/// cannot mistake the live first instance for an unclean shutdown.
pub fn begin_session() {
    let Some(session) = SESSION.get() else { return };
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return;
    }
    match report_unclean_session(
        &session.marker,
        &session.logs,
        &session.crashes,
        &session.component,
    ) {
        Ok(Some(report)) => record(&format!(
            "unclean previous session report saved: {}",
            report.display()
        )),
        Ok(None) => {}
        Err(error) => record(&format!(
            "could not process previous session marker: {error}"
        )),
    }
    let marker = format!(
        "component={}\npid={}\nstarted_ms={}\nlog={}\n",
        session.component,
        std::process::id(),
        session.started_ms,
        session.log.display()
    );
    if let Err(error) = fs::write(&session.marker, marker) {
        record(&format!("could not create active session marker: {error}"));
        ACTIVE.store(false, Ordering::Release);
    }
}

pub fn finish_session(reason: &str) {
    record(&format!("shutdown reason: {reason}"));
    if ACTIVE.swap(false, Ordering::AcqRel) {
        if let Some(session) = SESSION.get() {
            if let Err(error) = fs::remove_file(&session.marker) {
                if error.kind() != io::ErrorKind::NotFound {
                    record(&format!("could not clear active session marker: {error}"));
                }
            }
        }
    }
}

pub fn record(message: &str) {
    if let Some(session) = SESSION.get() {
        let _ = append(&session.log, message);
    }
}

fn write_crash_report(reason: &str) {
    let Some(session) = SESSION.get() else { return };
    let result = write_crash_report_to(
        &session.report,
        &session.logs,
        session.started_ms,
        &session.related_logs,
        reason,
    );
    if let Err(error) = result {
        record(&format!("crash report write failed: {error}"));
    }
}

fn write_crash_report_to(
    report_path: &Path,
    logs_dir: &Path,
    started_ms: u128,
    related_logs: &[PathBuf],
    reason: &str,
) -> io::Result<()> {
    let mut created = false;
    let result = (|| {
        let mut report = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(report_path)?;
        created = true;
        writeln!(report, "SysX crash report\nreason: {reason}\n")?;
        let mut logs = Vec::new();
        for entry in fs::read_dir(logs_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "log") {
                let modified_ms = entry
                    .metadata()?
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|time| time.as_millis());
                if modified_ms.map_or(true, |time| time >= started_ms) {
                    logs.push(path);
                }
            }
        }
        logs.extend(related_logs.iter().filter(|path| path.is_file()).cloned());
        logs.sort();
        logs.dedup();
        for path in logs {
            writeln!(report, "\n===== {} =====", path.display())?;
            report.write_all(&fs::read(path)?)?;
            writeln!(report)?;
        }
        report.sync_all()
    })();
    if created && result.is_err() {
        let _ = fs::remove_file(report_path);
    }
    result
}

fn report_unclean_session(
    marker: &Path,
    logs_dir: &Path,
    crashes_dir: &Path,
    component: &str,
) -> io::Result<Option<PathBuf>> {
    let previous = match fs::read_to_string(marker) {
        Ok(previous) => previous,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let previous_start = previous
        .lines()
        .find_map(|line| line.strip_prefix("started_ms="))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let previous_pid = previous
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .unwrap_or_default();
    let previous_log = previous
        .lines()
        .find_map(|line| line.strip_prefix("log="))
        .unwrap_or_default();
    let crash_prefix = format!("{component}-");
    let crash_suffix = format!("-{previous_pid}-crash.txt");
    if !previous_pid.is_empty()
        && !previous_log.is_empty()
        && fs::read_dir(crashes_dir)?
            .filter_map(Result::ok)
            .any(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with(&crash_prefix)
                    && name.ends_with(&crash_suffix)
                    && fs::read_to_string(entry.path())
                        .is_ok_and(|contents| contents.contains(previous_log))
            })
    {
        fs::remove_file(marker)?;
        return Ok(None);
    }
    let report = crashes_dir.join(format!("{}-{}-unclean-exit.txt", component, timestamp_ms()));
    let reason = format!(
        "Previous {component} process ended without a clean-shutdown record. Exact cause is unavailable; possible causes include a forced termination, power loss, OS failure, or a crash before the handler could run. Previous session marker: {}",
        previous.trim().replace('\n', "; ")
    );
    write_crash_report_to(&report, logs_dir, previous_start, &[], &reason)?;
    fs::remove_file(marker)?;
    Ok(Some(report))
}

fn append(path: &Path, message: &str) -> io::Result<()> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    writeln!(file, "[unix_ms={}] {message}", timestamp_ms())?;
    file.flush()
}

/// Only backend stderr is redirected; protocol stdout stays piped to the host.
pub fn backend_stderr() -> io::Result<File> {
    let session = SESSION
        .get()
        .ok_or_else(|| io::Error::other("diagnostics not initialized"))?;
    let name = session
        .log
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    OpenOptions::new().create(true).append(true).open(
        session
            .log
            .with_file_name(format!("backend-stderr-{name}.log")),
    )
}

pub fn log_path() -> Option<PathBuf> {
    SESSION.get().map(|session| session.log.clone())
}

pub fn backend_stderr_path() -> Option<PathBuf> {
    let session = SESSION.get()?;
    let name = session.log.file_stem()?.to_string_lossy();
    Some(
        session
            .log
            .with_file_name(format!("backend-stderr-{name}.log")),
    )
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(windows)]
unsafe extern "system" fn unhandled_exception(
    pointers: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
) -> i32 {
    if let Some(info) = pointers.as_ref() {
        if let Some(exception) = info.ExceptionRecord.as_ref() {
            // First, while the stack may be nearly gone: the dump is written
            // by the writer thread.
            request_dump(pointers);
            let kind = windows_exception_name(exception.ExceptionCode as u32);
            record(&format!(
                "unhandled Windows exception kind={kind} code=0x{:08x} address={:p} flags=0x{:08x}",
                exception.ExceptionCode, exception.ExceptionAddress, exception.ExceptionFlags
            ));
            write_crash_report(&format!(
                "unhandled Windows exception: {kind} (0x{:08x}) at {:p}",
                exception.ExceptionCode, exception.ExceptionAddress
            ));
            return 0;
        }
    }
    request_dump(pointers);
    write_crash_report("unhandled Windows exception with missing exception details");
    0 // EXCEPTION_CONTINUE_SEARCH: preserve normal Windows crash handling.
}

#[cfg(windows)]
fn windows_exception_name(code: u32) -> &'static str {
    match code {
        0xc0000005 => "access violation",
        0xc000001d => "illegal instruction",
        0xc0000094 => "integer divide by zero",
        0xc00000fd => "stack overflow",
        0xc0000374 => "heap corruption",
        0xc0000409 => "fail-fast or stack buffer overrun",
        0xc0000602 => "fail-fast exception",
        _ => "unhandled structured exception",
    }
}

#[cfg(windows)]
fn write_dump(
    pointers: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
    thread: u32,
) {
    use std::os::windows::io::AsRawHandle;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        MiniDumpNormal, MiniDumpWriteDump, MINIDUMP_EXCEPTION_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};
    static WRITING: AtomicBool = AtomicBool::new(false);
    if WRITING.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(session) = SESSION.get() else {
        WRITING.store(false, Ordering::Release);
        return;
    };
    let result = (|| -> io::Result<()> {
        let file = File::create(&session.dump)?;
        let exception = MINIDUMP_EXCEPTION_INFORMATION {
            ThreadId: thread,
            ExceptionPointers: pointers.cast_mut(),
            ClientPointers: 0,
        };
        let ok = unsafe {
            MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                file.as_raw_handle(),
                MiniDumpNormal,
                if pointers.is_null() {
                    std::ptr::null()
                } else {
                    &exception
                },
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
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
        let path = std::env::temp_dir().join(format!(
            "sysx-log-test-{}-{}.log",
            std::process::id(),
            timestamp_ms()
        ));
        File::create(&path).unwrap();
        append(&path, "first failure").unwrap();
        append(&path, "second failure").unwrap();
        let log = fs::read_to_string(&path).unwrap();
        assert!(log.contains("first failure\n"));
        assert!(log.contains("second failure\n"));
        assert_eq!(log.lines().count(), 2);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn crash_report_contains_reason_and_complete_current_logs() {
        let root = std::env::temp_dir().join(format!("sysx-crash-report-{}", timestamp_ms()));
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let log = logs.join("host-test.log");
        let backend = logs.join("backend-stderr-host-test.log");
        let parent = root.join("parent-host.log");
        fs::write(&log, "host startup\nhost failure\n").unwrap();
        fs::write(&backend, "backend diagnostic\n").unwrap();
        fs::write(&parent, "parent-host context\n").unwrap();
        let report = root.join("crash.txt");
        write_crash_report_to(&report, &logs, 0, &[parent], "access violation").unwrap();
        let contents = fs::read_to_string(&report).unwrap();
        assert!(contents.contains("reason: access violation"));
        assert!(contents.contains("host startup\nhost failure"));
        assert!(contents.contains("backend diagnostic"));
        assert!(contents.contains("parent-host context"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_session_is_reported_as_unknown_cause_and_all_logs_are_attached() {
        let root = std::env::temp_dir().join(format!("sysx-unclean-{}", timestamp_ms()));
        let logs = root.join("logs");
        let crashes = root.join("crashes");
        let sessions = root.join("sessions");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&crashes).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            logs.join("host-old.log"),
            "last message before abrupt exit\n",
        )
        .unwrap();
        let marker = sessions.join("host.active");
        fs::write(
            &marker,
            "component=host\npid=42\nstarted_ms=0\nlog=host-old.log\n",
        )
        .unwrap();
        let report = report_unclean_session(&marker, &logs, &crashes, "host")
            .unwrap()
            .unwrap();
        let contents = fs::read_to_string(report).unwrap();
        assert!(contents.contains("Exact cause is unavailable"));
        assert!(contents.contains("last message before abrupt exit"));
        assert!(!marker.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn known_crash_report_prevents_a_duplicate_unknown_exit_report() {
        let root = std::env::temp_dir().join(format!("sysx-known-crash-{}", timestamp_ms()));
        let logs = root.join("logs");
        let crashes = root.join("crashes");
        let sessions = root.join("sessions");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(&crashes).unwrap();
        fs::create_dir_all(&sessions).unwrap();
        let marker = sessions.join("host.active");
        let previous_log = logs.join("host-100-42.log");
        fs::write(
            &marker,
            format!(
                "component=host\npid=42\nstarted_ms=0\nlog={}\n",
                previous_log.display()
            ),
        )
        .unwrap();
        fs::write(
            crashes.join("host-100-42-crash.txt"),
            format!("known panic report\n{}\n", previous_log.display()),
        )
        .unwrap();
        assert!(report_unclean_session(&marker, &logs, &crashes, "host")
            .unwrap()
            .is_none());
        assert!(!marker.exists());
        assert_eq!(fs::read_dir(&crashes).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }
}
