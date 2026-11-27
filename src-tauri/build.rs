fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
        let builder = manifest.join("../crates/vnt-codec/linux/build.py");
        for file in ["build.py", "encoder.c", "exports.map", "sources.json"] {
            println!("cargo:rerun-if-changed=../crates/vnt-codec/linux/{file}");
        }
        let status = std::process::Command::new("python3")
            .arg(builder)
            .status()
            .expect("python3 is required for the Linux encoder module");
        assert!(status.success(), "Linux encoder module build failed");
    }
    tauri_build::build()
}
