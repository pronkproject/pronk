fn main() {
    println!("cargo:rerun-if-changed=fixture.c");
    let drm = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libdrm")
        .expect("libdrm development files");
    let mut build = cc::Build::new();
    build
        .file("fixture.c")
        .warnings(true)
        .warnings_into_errors(true);
    for include in drm.include_paths {
        build.include(include);
    }
    build.compile("capture_fixture");
    pkg_config::Config::new()
        .probe("libdrm")
        .expect("libdrm development files");

    println!("cargo:rerun-if-changed=pattern.c");
    let gtk = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("gtk4")
        .expect("GTK 4 development files for the Wayland pattern client");
    let mut pattern = cc::Build::new();
    pattern
        .file("pattern.c")
        .warnings(true)
        .warnings_into_errors(true);
    for include in gtk.include_paths {
        pattern.include(include);
    }
    pattern.compile("capture_pattern");
    pkg_config::Config::new()
        .probe("gtk4")
        .expect("GTK 4 development files for the Wayland pattern client");
}
