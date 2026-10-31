#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(windows)]
fn main() {
    if let Err(e) = run() {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}
#[cfg(not(windows))]
fn main() {
    eprintln!("Windows service only");
}
#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    use vnt_host::windows::{agent, management, native, service};
    let args: Vec<String> = std::env::args().collect();
    if matches!(
        args.get(1).map(String::as_str),
        Some("--install" | "--uninstall" | "--uninstall-upgrade" | "--prepare-upgrade")
    ) {
        let logs = vnt_host::windows::store::secure_log_directory()?;
        vnt_host::logging::init_file(&logs.join("vnt-installer.log"))?;
    }

    match args.get(1).map(String::as_str) {
        Some("--service") => service::dispatch(),
        Some("--install") => service::install(),
        Some("--prepare-upgrade") => service::prepare_upgrade(),
        Some("--uninstall") => service::uninstall(false),
        Some("--uninstall-upgrade") => service::uninstall(true),
        Some(mode @ ("--agent" | "--user-agent" | "--elevated")) => {
            let pipe = args.get(2).ok_or_else(|| anyhow::anyhow!("缺少管道"))?;
            let pid = args
                .get(3)
                .ok_or_else(|| anyhow::anyhow!("缺少父进程"))?
                .parse()?;
            let desktop = if mode == "--agent" {
                let d = native::Desktop::input()?;
                d.bind()?;
                Some(d)
            } else {
                None
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let result = runtime.block_on(async {
                if mode == "--elevated" {
                    management::helper(pipe, pid).await
                } else {
                    agent::run(pipe, pid, mode == "--user-agent").await
                }
            });
            drop(desktop);
            result
        }
        _ => anyhow::bail!("此程序仅由安装器、Windows 服务管理器或主程序启动"),
    }
}
