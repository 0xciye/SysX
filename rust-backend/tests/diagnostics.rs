use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn panic_report_survives_backend_process_exit() {
    let root = std::env::temp_dir().join(format!(
        "sysx-diagnostics-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_receiver-backend"))
        .arg("--diagnostics-self-test")
        .env("LOCALAPPDATA", &root)
        .output()
        .expect("run isolated panic fixture");
    assert!(!output.status.success());
    let logs = fs::read_dir(root.join("SysX/logs")).unwrap()
        .map(|entry| entry.unwrap().path()).collect::<Vec<_>>();
    assert_eq!(logs.len(), 1);
    let report = fs::read_to_string(&logs[0]).unwrap();
    assert!(report.contains("Rust panic:"));
    assert!(report.contains("SysX diagnostics self-test"));
    assert!(report.contains("main.rs"));
    #[cfg(windows)]
    {
        let dumps = fs::read_dir(root.join("SysX/crashes")).unwrap()
            .map(|entry| entry.unwrap().path()).collect::<Vec<_>>();
        assert_eq!(dumps.len(), 1, "{report}");
        let dump = fs::read(&dumps[0]).unwrap();
        assert!(dump.starts_with(b"MDMP"), "expected a Windows minidump");
    }
    fs::remove_dir_all(root).unwrap();
}
