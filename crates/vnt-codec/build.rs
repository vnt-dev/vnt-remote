//! 静态构建 vendor/svt-av1 与 vendor/kvazaar（软件 AV1 / HEVC 兜底）。
//!
//! 首次构建用 CMake 静态编译 SvtAv1Enc（仅编码库，无 app/test），
//! 之后由 cargo 按指纹跳过。非 Windows 平台不参与构建。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let kvazaar = manifest.join("../../vendor/kvazaar");
    let dst = cmake::Config::new(&kvazaar)
        .out_dir(PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("kvazaar"))
        .profile("Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_TESTS", "OFF")
        .define("BUILD_KVAZAAR_BINARY", "OFF")
        .define("USE_CRYPTO", "OFF")
        .build();
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    cc::Build::new()
        .debug(false)
        .file(manifest.join("src/kvazaar_bridge.c"))
        .include(kvazaar.join("src"))
        .define("KVZ_STATIC_LIB", None)
        .compile("vnt_kvazaar_bridge");
    println!("cargo:rustc-link-lib=static=libkvazaar");
    println!("cargo:rerun-if-changed={}", kvazaar.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        kvazaar.join("CMakeLists.txt").display()
    );
    println!("cargo:rerun-if-changed=src/kvazaar_bridge.c");
    let svt = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("vendor").join("svt-av1"))
        .filter(|p| p.join("CMakeLists.txt").is_file());
    let Some(svt) = svt else {
        // vendor 源码缺失时仍允许编译（模块在 cfg(windows) 下引用符号会失败，
        // 但 CI 的非编码目标可显式关闭）。实际发布构建必须带上 vendor。
        if cfg!(windows) {
            panic!("vendor/svt-av1 缺失：软件 AV1 编码需要该源码（见 README）");
        }
        return;
    };

    let dst = cmake::Config::new(&svt)
        // Match Rust's dynamic release CRT even in cargo's debug profile.
        .define("CMAKE_POLICY_DEFAULT_CMP0091", "NEW")
        .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreadedDLL")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("BUILD_APPS", "OFF")
        .define("BUILD_TESTING", "OFF")
        .define("ENABLE_AVX512", "OFF") // 目标机兼容性优先
        // CMake ≥4 拒绝 third_party/cpuinfo 的旧 cmake_minimum_required，
        // 按官方兼容开关放行
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .build();

    // 多配置生成器（VS）与单配置生成器的产物目录不同，直接搜索定位。
    let lib = walk(&dst).into_iter().find(|p| {
        p.file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case("SvtAv1Enc.lib"))
            .unwrap_or(false)
    });
    let Some(lib) = lib else {
        panic!("SVT-AV1 构建完成但未找到 SvtAv1Enc.lib（{dst:?}）");
    };
    let dir = lib.parent().unwrap();
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=SvtAv1Enc");
    println!(
        "cargo:rerun-if-changed={}",
        svt.join("Source").join("Lib").display()
    );
}

fn walk(root: &std::path::Path) -> impl Iterator<Item = PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut out = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().map(|x| x == "lib").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out.into_iter()
}
