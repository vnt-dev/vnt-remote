fn main() {
    println!("cargo:rerun-if-changed=native.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("native.m")
        .flag("-fobjc-arc")
        .flag("-fblocks")
        .flag("-Wno-unused-parameter")
        .flag("-mmacosx-version-min=14.0")
        .compile("vnt_macos");
    for framework in [
        "AppKit",
        "ApplicationServices",
        "ScreenCaptureKit",
        "CoreMedia",
        "CoreVideo",
        "VideoToolbox",
        "AVFoundation",
        "AudioToolbox",
        "CoreAudio",
        "Security",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
