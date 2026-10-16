//! Integrity of immutable extracted runtime files. No vision/device locks are
//! used here. The host opts in only for release application launches.
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub type ManifestEntry = (&'static str, u64, [u8; 32]);
pub const SCAN_INTERVAL: Duration = Duration::from_secs(2);
pub const CHUNK_PAUSE: Duration = Duration::from_millis(2);
pub const CHUNK_BYTES: usize = 256 * 1024;

struct ProtectedFile {
    path: PathBuf,
    size: u64,
    digest: [u8; 32],
    handle: File,
}

pub struct VerifiedRuntime {
    root: PathBuf,
    files: Vec<ProtectedFile>,
    paths: HashSet<PathBuf>,
    directories: HashSet<PathBuf>,
}

#[derive(Default)]
pub struct ScanReport {
    pub bytes: u64,
    pub chunks: u64,
    pub cancelled: bool,
}

#[derive(Default)]
struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    fn wait(&self, duration: Duration) -> bool {
        let stopped = self
            .stopped
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if duration.is_zero() {
            return *stopped;
        }
        let (stopped, _) = self
            .wake
            .wait_timeout_while(stopped, duration, |stop| !*stop)
            .unwrap_or_else(|error| error.into_inner());
        *stopped
    }
}

pub struct IntegrityGuard {
    stop: Arc<Stop>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for IntegrityGuard {
    fn drop(&mut self) {
        *self
            .stop
            .stopped
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.stop.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl VerifiedRuntime {
    /// Verify against build-time hashes before the backend or UI can start.
    /// Windows handles deny subsequent write/delete sharing for their lifetime.
    pub fn open(root: &Path, manifest: &[ManifestEntry]) -> io::Result<Self> {
        if manifest.is_empty() {
            return Err(invalid("empty runtime integrity manifest"));
        }
        let mut runtime = Self {
            root: root.to_owned(),
            files: Vec::new(),
            paths: HashSet::new(),
            directories: HashSet::from([PathBuf::new()]),
        };
        for (relative, _, _) in manifest {
            let relative = Path::new(relative);
            if relative.as_os_str().is_empty()
                || !relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
                || relative.to_string_lossy().contains([':', '\\'])
                || !runtime.paths.insert(relative.to_owned())
            {
                return Err(invalid("invalid or duplicate runtime manifest path"));
            }
            let mut parent = relative.parent();
            while let Some(path) = parent {
                runtime.directories.insert(path.to_owned());
                parent = path.parent();
            }
        }
        runtime.verify_layout()?;
        for (relative, size, digest) in manifest {
            let path = root.join(relative);
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_file() || is_link(&meta) || meta.len() != *size {
                return Err(invalid("runtime file missing, linked or resized"));
            }
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(1); // FILE_SHARE_READ, no WRITE or DELETE
            }
            let handle = options.open(&path)?;
            runtime.files.push(ProtectedFile {
                path,
                size: *size,
                digest: *digest,
                handle,
            });
        }
        runtime.scan_inner(&Stop::default(), Duration::ZERO)?;
        Ok(runtime)
    }

    /// Explicit scans are also usable by the portable test/benchmark harness.
    pub fn scan(&mut self, pause: Duration) -> io::Result<ScanReport> {
        self.scan_inner(&Stop::default(), pause)
    }

    pub fn watch(
        self,
        on_failure: impl FnOnce(io::Error) + Send + 'static,
    ) -> io::Result<IntegrityGuard> {
        let stop = Arc::new(Stop::default());
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("sysx-runtime-integrity".into())
            .spawn(move || {
                lower_worker_priority();
                let mut runtime = self; // owns the Windows read locks until shutdown
                while !worker_stop.wait(SCAN_INTERVAL) {
                    match runtime.scan_inner(&worker_stop, CHUNK_PAUSE) {
                        Ok(report) if report.cancelled => break,
                        Ok(_) => {}
                        Err(error) => {
                            on_failure(error);
                            break;
                        }
                    }
                }
            })?;
        Ok(IntegrityGuard {
            stop,
            worker: Some(worker),
        })
    }

    /// Stop precisely the sidecar owned by this host, never a PID lookup or
    /// an externally resolved termination executable. No lock is taken until
    /// a verification failure occurs.
    pub fn watch_backend(self, owned_child: Arc<Mutex<Child>>) -> io::Result<IntegrityGuard> {
        self.watch(move |error| {
            eprintln!("Runtime integrity check failed: {error}");
            let mut child = owned_child
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let _ = child.kill();
        })
    }

    fn verify_layout(&self) -> io::Result<()> {
        for relative in &self.directories {
            let path = self.root.join(relative);
            let meta = fs::symlink_metadata(&path)?;
            if !meta.is_dir() || is_link(&meta) {
                return Err(invalid("runtime directory replaced or linked"));
            }
            for entry in fs::read_dir(path)? {
                let relative = relative.join(entry?.file_name());
                if !self.paths.contains(&relative) && !self.directories.contains(&relative) {
                    return Err(invalid("unexpected file in immutable runtime"));
                }
            }
        }
        Ok(())
    }

    fn scan_inner(&mut self, stop: &Stop, pause: Duration) -> io::Result<ScanReport> {
        self.verify_layout()?;
        let mut report = ScanReport::default();
        let mut buffer = vec![0_u8; CHUNK_BYTES];
        for protected in &mut self.files {
            let path_meta = fs::symlink_metadata(&protected.path)?;
            if !path_meta.is_file() || is_link(&path_meta) || path_meta.len() != protected.size {
                return Err(invalid("runtime file missing, linked or resized"));
            }
            // Reopen the current path on every pass, so a directory/filename
            // replacement cannot hide behind the old pinned handle on Unix.
            let mut current = OpenOptions::new();
            current.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                current.share_mode(1);
            }
            protected.handle = current.open(&protected.path)?;
            protected.handle.seek(SeekFrom::Start(0))?;
            let mut hasher = Sha256::new();
            let mut total = 0_u64;
            loop {
                if stop.wait(Duration::ZERO) {
                    report.cancelled = true;
                    return Ok(report);
                }
                // At most the expected size plus one byte, even if an attacker
                // continuously grows a file on a platform without read locks.
                let limit = (protected.size.saturating_sub(total).saturating_add(1))
                    .min(CHUNK_BYTES as u64) as usize;
                let count = protected.handle.read(&mut buffer[..limit])?;
                if count == 0 {
                    break;
                }
                total += count as u64;
                if total > protected.size {
                    return Err(invalid("runtime file grew during verification"));
                }
                hasher.update(&buffer[..count]);
                report.bytes += count as u64;
                report.chunks += 1;
                if !pause.is_zero() && stop.wait(pause) {
                    report.cancelled = true;
                    return Ok(report);
                }
            }
            let actual: [u8; 32] = hasher.finalize().into();
            if total != protected.size || actual != protected.digest {
                return Err(invalid("runtime SHA-256 mismatch"));
            }
        }
        Ok(report)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn lower_worker_priority() {
    #[cfg(windows)]
    unsafe {
        // Lower CPU and I/O priority for this thread only, never the vision
        // sidecar, UI or 4 ms movement ticker. Unsupported policies are benign.
        let _ = SetThreadPriority(GetCurrentThread(), 0x0001_0000);
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThread() -> *mut std::ffi::c_void;
    fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
}

fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        } // reparse point
    }
    meta.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sysx-integrity-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("web-config")).unwrap();
            fs::write(path.join("backend"), b"backend").unwrap();
            fs::write(path.join("web-config/app.wasm"), b"wasm").unwrap();
            Self(path)
        }
        fn entries(&self) -> Vec<ManifestEntry> {
            [
                ("backend", b"backend".as_slice()),
                ("web-config/app.wasm", b"wasm".as_slice()),
            ]
            .map(|(name, bytes)| (name, bytes.len() as u64, Sha256::digest(bytes).into()))
            .to_vec()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn startup_rejects_same_length_patch_resize_delete_and_extra_file() {
        for mode in 0..4 {
            let fixture = Fixture::new();
            let entries = fixture.entries();
            match mode {
                0 => fs::write(fixture.0.join("backend"), b"patched").unwrap(),
                1 => fs::write(fixture.0.join("web-config/app.wasm"), b"resized").unwrap(),
                2 => fs::remove_file(fixture.0.join("backend")).unwrap(),
                _ => fs::write(fixture.0.join("web-config/injected.js"), b"extra").unwrap(),
            }
            assert!(VerifiedRuntime::open(&fixture.0, &entries).is_err());
        }
    }

    #[test]
    fn unchanged_files_pass_repeated_scans_and_guard_stops_promptly() {
        let fixture = Fixture::new();
        let mut runtime = VerifiedRuntime::open(&fixture.0, &fixture.entries()).unwrap();
        for _ in 0..3 {
            let report = runtime.scan(Duration::ZERO).unwrap();
            assert_eq!(report.bytes, 11);
            assert!(!report.cancelled);
        }
        let guard = runtime
            .watch(|error| panic!("unexpected integrity failure: {error}"))
            .unwrap();
        let started = std::time::Instant::now();
        drop(guard);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn traversal_and_duplicate_manifest_paths_are_rejected() {
        let fixture = Fixture::new();
        assert!(VerifiedRuntime::open(&fixture.0, &[]).is_err());
        for path in [
            "../outside",
            "/absolute",
            "web-config/../backend",
            "file:stream",
            "",
        ] {
            assert!(VerifiedRuntime::open(&fixture.0, &[(path, 0, [0; 32])]).is_err());
        }
        let entry = fixture.entries()[0];
        assert!(VerifiedRuntime::open(&fixture.0, &[entry, entry]).is_err());
    }

    #[test]
    fn cancellation_during_a_scan_is_not_an_integrity_failure() {
        let fixture = Fixture::new();
        let mut runtime = VerifiedRuntime::open(&fixture.0, &fixture.entries()).unwrap();
        let stop = Stop::default();
        *stop.stopped.lock().unwrap() = true;
        let report = runtime.scan_inner(&stop, CHUNK_PAUSE).unwrap();
        assert!(report.cancelled);
        assert_eq!(report.bytes, 0);
        assert_eq!(runtime.scan(Duration::ZERO).unwrap().bytes, 11);
    }

    #[test]
    fn a_file_injected_after_startup_is_detected() {
        let fixture = Fixture::new();
        let mut runtime = VerifiedRuntime::open(&fixture.0, &fixture.entries()).unwrap();
        fs::write(fixture.0.join("web-config/injected.js"), b"extra").unwrap();
        assert!(runtime.scan(Duration::ZERO).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_path_cannot_hide_behind_the_previous_open_handle() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let mut runtime = VerifiedRuntime::open(&fixture.0, &fixture.entries()).unwrap();
        fs::write(outside.0.join("backend"), b"patched").unwrap();
        fs::rename(outside.0.join("backend"), fixture.0.join("backend")).unwrap();
        assert!(runtime.scan(Duration::ZERO).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_runtime_symlink_is_rejected_even_when_its_contents_match() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fs::remove_file(fixture.0.join("backend")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("backend"), fixture.0.join("backend")).unwrap();
        assert!(VerifiedRuntime::open(&fixture.0, &fixture.entries()).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn watchdog_detects_an_in_place_patch_and_invokes_failure_once() {
        let fixture = Fixture::new();
        let runtime = VerifiedRuntime::open(&fixture.0, &fixture.entries()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let guard = runtime
            .watch(move |error| {
                tx.send(error.to_string()).unwrap();
            })
            .unwrap();
        fs::write(fixture.0.join("web-config/app.wasm"), b"evil").unwrap();
        assert!(rx
            .recv_timeout(Duration::from_secs(4))
            .unwrap()
            .contains("mismatch"));
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        drop(guard);
    }

    #[cfg(windows)]
    #[test]
    fn windows_read_locks_prevent_write_delete_and_replacement() {
        let fixture = Fixture::new();
        let runtime = VerifiedRuntime::open(&fixture.0, &fixture.entries()).unwrap();
        assert!(fs::write(fixture.0.join("backend"), b"patched").is_err());
        assert!(fs::remove_file(fixture.0.join("web-config/app.wasm")).is_err());
        assert!(fs::rename(fixture.0.join("backend"), fixture.0.join("replacement")).is_err());
        drop(runtime);
        fs::write(fixture.0.join("backend"), b"changed").unwrap();
    }
}
