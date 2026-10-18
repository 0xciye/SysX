fn main() {
    let _ = sysx_backend::diagnostics::install("backend");
    if std::env::args().any(|arg| arg == "--diagnostics-self-test") {
        panic!("SysX diagnostics self-test");
    }
    sysx_backend::timer_resolution::request_fine_timer();
    if let Err(error) = sysx_backend::run(std::env::args().skip(1).collect()) {
        sysx_backend::diagnostics::record(&format!("fatal backend error: {error}"));
        eprintln!("{error}");
        sysx_backend::diagnostics::finish_session(&format!(
            "backend exited with fatal error: {error}"
        ));
        std::process::exit(1);
    }
    sysx_backend::diagnostics::finish_session("backend returned normally");
}
