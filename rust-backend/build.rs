fn main() {
    let root = "../third-party/abcurves";
    println!("cargo:rerun-if-changed={root}");
    println!("cargo:rerun-if-changed=src/movement/abcurves_bridge.c");
    let mut build = cc::Build::new();
    build
        .include(format!("{root}/include"))
        .include(format!("{root}/src"))
        .file(format!("{root}/src/abc_online.c"))
        .file(format!("{root}/src/abc_fixed.c"))
        .file(format!("{root}/src/abc_w5_stream.c"))
        .file("src/movement/abcurves_bridge.c");
    if build.get_compiler().is_like_msvc() {
        build.flag("/fp:strict").flag_if_supported("/std:c11");
    } else {
        build
            .flag_if_supported("-std=c99")
            .flag_if_supported("-ffp-contract=off");
        println!("cargo:rustc-link-lib=m");
    }
    build.compile("sysx_abcurves");
}
