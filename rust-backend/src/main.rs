fn main() {
    let _ = sysx_backend::diagnostics::install("backend");
    if std::env::args().any(|arg| arg == "--diagnostics-self-test") {
        panic!("SysX diagnostics self-test");
    }
    sysx_backend::security::start_release_guard();
    if let Err(error) = sysx_backend::run(std::env::args().skip(1).collect()) {
        sysx_backend::diagnostics::record(&format!("fatal backend error: {error}"));
        eprintln!("{error}");
        std::process::exit(1);
    }
    sysx_backend::diagnostics::record("backend stopped");
}
