#![cfg(windows)]
//! 验证我们写入的 CF_HDROP 能被**独立消费者**按 shell 的方式读出来。
//!
//! 比用自己的读回函数强：DROPFILES 布局写错时自读可能碰巧对得上，
//! 而资源管理器/桌面粘贴走的是 shell 的 DragQueryFile 路径——那才是
//! 用户"粘贴到桌面/文件夹"时真正用的代码。

#[test]
fn shell_consumer_reads_our_cf_hdrop_verbatim() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("报表-final.xlsx");
    std::fs::write(&file, b"x").unwrap();
    let folder = dir.path().join("资料");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("a.txt"), b"y").unwrap();

    // 剪贴板可能被其他进程占用：拿不到就跳过（布局问题只在正常桌面暴露）
    if vnt_clip::win::set_files(&[file.clone(), folder.clone()]).is_err() {
        eprintln!("跳过：本机剪贴板不可写");
        return;
    }
    let out_file = dir.path().join("shell-read.txt");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         [Windows.Forms.Clipboard]::GetFileDropList() | Out-File -FilePath '{}' -Encoding utf8",
        out_file.display()
    );
    let mut read_back = String::new();
    for _ in 0..10 {
        std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &script])
            .output()
            .unwrap();
        // Out-File 会带 UTF-8 BOM 与 CRLF，比对前剥掉
        read_back = std::fs::read_to_string(&out_file)
            .unwrap_or_default()
            .trim_start_matches('\u{feff}')
            .replace("\r\n", "\n");
        if read_back.lines().count() >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let lines: Vec<&str> = read_back.lines().collect();
    assert_eq!(
        lines,
        vec![
            file.to_string_lossy().as_ref(),
            folder.to_string_lossy().as_ref()
        ],
        "shell 读回的顶层条目必须与写入逐一相符"
    );
}
