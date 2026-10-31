fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 未显式指定 PROTOC 时，回退到 vendored 的 protoc 二进制，避免依赖系统安装。
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }
    prost_build::compile_protos(&["proto/vnt.proto"], &["proto"])?;
    Ok(())
}
