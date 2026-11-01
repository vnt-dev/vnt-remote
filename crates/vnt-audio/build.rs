fn main() {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/opus");
    // Match upstream Makefile.unix's portable floating-point source set. macOS
    // needs only Apple's C toolchain; Windows retains its existing CMake build.
    if matches!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("macos" | "android")
    ) {
        let mut build = cc::Build::new();
        build
            .opt_level(3)
            .warnings(false)
            .define("OPUS_BUILD", None)
            .define("USE_ALLOCA", None)
            .define("HAVE_LRINTF", None)
            .define("HAVE_LRINT", None)
            .define("PACKAGE_VERSION", Some("\"1.6.1\""));
        for include in ["include", "silk", "silk/float", "celt"] {
            build.include(source.join(include));
        }
        for (file, names) in [
            (
                "opus_sources.mk",
                &["OPUS_SOURCES", "OPUS_SOURCES_FLOAT"][..],
            ),
            ("celt_sources.mk", &["CELT_SOURCES"][..]),
            (
                "silk_sources.mk",
                &["SILK_SOURCES", "SILK_SOURCES_FLOAT"][..],
            ),
        ] {
            let path = source.join(file);
            let text = std::fs::read_to_string(&path).expect("vendored Opus source manifest");
            let joined = text.replace("\\\n", " ");
            for name in names {
                let prefix = format!("{name} =");
                let line = joined
                    .lines()
                    .find_map(|line| line.strip_prefix(&prefix))
                    .expect("Opus source list");
                for file in line.split_whitespace() {
                    build.file(source.join(file));
                }
            }
            println!("cargo:rerun-if-changed={}", path.display());
        }
        build.compile("opus");
        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-changed={}", source.display());
        return;
    }
    let dst = cmake::Config::new(&source)
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("OPUS_BUILD_SHARED_LIBRARY", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_STATIC_RUNTIME", "OFF")
        .define("OPUS_DRED", "OFF")
        .define("OPUS_OSCE", "OFF")
        .define("OPUS_DEEP_PLC", "OFF")
        .build();
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=opus");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", source.display());
}
