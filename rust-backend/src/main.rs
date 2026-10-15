fn main() {
    if let Err(error) = sysx_backend::run(std::env::args().skip(1).collect()) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
