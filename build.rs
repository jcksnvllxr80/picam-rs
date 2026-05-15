fn main() {
    slint_build::compile("ui/app.slint").unwrap();

    // Only compile the C++ camera wrapper when targeting Linux (i.e. on the Pi).
    // Cross-compiling from Windows is not supported; builds on the Pi directly.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .file("src/camera_ffi.cpp")
            .flag("-Wno-unused-parameter")
            .flag("-Wno-missing-field-initializers")
            // libcamera headers
            .include("/usr/include/libcamera")
            .compile("camera_ffi");

        println!("cargo:rustc-link-lib=camera");
        println!("cargo:rustc-link-lib=camera-base");
        println!("cargo:rustc-link-search=/usr/lib/aarch64-linux-gnu");
    }
}
