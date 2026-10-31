#![cfg(windows)]
//! Real Shell clipboard probe. Run explicitly, serially, on an interactive desktop:
//! cargo test -p vnt-clip --test transfer_source_probe -- --ignored --nocapture --test-threads=1
//! Both consumers use OleInitialize (STA). MTA Shell paste silently falls back to
//! a legacy clipboard adapter that loses FileContents.lindex across processes.
use std::os::windows::fs::OpenOptionsExt;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use vnt_clip::native::{FileOperation, FileProvider};
use vnt_clip::source::{SourceOffer, SourceOperation};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{OleInitialize, OleSetClipboard, OleUninitialize};

struct Provider {
    offer: Arc<SourceOffer>,
    reads: Arc<AtomicUsize>,
    ended: Arc<AtomicUsize>,
    delay: Duration,
    outcomes: Arc<Mutex<Vec<bool>>>,
}
struct Operation {
    source: Mutex<SourceOperation>,
    reads: Arc<AtomicUsize>,
    ended: Arc<AtomicUsize>,
    done: AtomicBool,
    delay: Duration,
    outcomes: Arc<Mutex<Vec<bool>>>,
}
impl FileProvider for Provider {
    fn available(&self) -> bool {
        true
    }
    fn begin(&self) -> Result<Arc<dyn FileOperation>, String> {
        Ok(Arc::new(Operation {
            source: Mutex::new(SourceOperation::new(self.offer.clone())),
            reads: self.reads.clone(),
            ended: self.ended.clone(),
            done: AtomicBool::new(false),
            delay: self.delay,
            outcomes: self.outcomes.clone(),
        }))
    }
}
impl FileOperation for Operation {
    fn read(&self, i: u32, offset: u64, length: u32) -> Result<Vec<u8>, String> {
        assert_ne!(
            std::thread::current().name(),
            Some("probe-ole-owner"),
            "file reads must not block the clipboard owner STA"
        );
        std::thread::sleep(self.delay);
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.source.lock().unwrap().read(i, offset, length)
    }
    fn close(&self, i: u32) {
        self.source.lock().unwrap().close(i);
    }
    fn finish(&self, ok: bool, why: &str) {
        eprintln!("Shell finished: ok={ok}, {why}");
        if !self.done.swap(true, Ordering::SeqCst) {
            self.outcomes.lock().unwrap().push(ok);
            if ok {
                self.ended.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}
fn verify(folder: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let valid = [
            ("probe-a.bin", 300 * 1024usize),
            ("payload/probe-b.bin", 120 * 1024),
            ("payload/空文件.txt", 0),
        ]
        .iter()
        .all(|(name, size)| {
            let expected: Vec<u8> = (0..*size).map(|i| (i % 251) as u8).collect();
            std::fs::read(folder.join(name)).ok().as_ref() == Some(&expected)
        }) && folder.join("payload/空目录").is_dir();
        if valid {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[test]
#[ignore = "uses the real clipboard and Shell; interactive desktop required"]
fn shell_pastes_virtual_files_across_processes() {
    let _guard = vnt_clip::CLIPBOARD_LOCK.lock().unwrap();
    let source = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(source.path().join("payload/空目录")).unwrap();
    for (name, size) in [
        ("probe-a.bin", 300 * 1024usize),
        ("payload/probe-b.bin", 120 * 1024),
        ("payload/空文件.txt", 0),
    ] {
        std::fs::write(
            source.path().join(name),
            (0..size).map(|i| (i % 251) as u8).collect::<Vec<_>>(),
        )
        .unwrap();
    }
    let offer = Arc::new(
        SourceOffer::collect(
            1,
            &[
                source.path().join("probe-a.bin"),
                source.path().join("payload"),
            ],
        )
        .unwrap(),
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let ended = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Provider {
        offer: offer.clone(),
        reads: reads.clone(),
        ended: ended.clone(),
        delay: Duration::ZERO,
        outcomes: Arc::new(Mutex::new(Vec::new())),
    });
    let owner = publish(Arc::new(offer.manifest.clone()), provider);
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "copy must not fetch file bytes"
    );
    let mut results = Vec::new();
    // Each consumer is a fresh process and fresh directory. Repeated paste must work.
    for _ in 0..2 {
        let destination = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "child_pastes",
                "--ignored",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("VNT_PASTE_TARGET", destination.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match child.try_wait().unwrap() {
                Some(status) => {
                    let output = child.wait_with_output().unwrap();
                    let verified = status.success() && verify(destination.path());
                    if !verified {
                        eprintln!(
                            "child stdout: {}\nchild stderr: {}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    results.push(verified);
                    break;
                }
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    results.push(false);
                    break;
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
    drop(owner);
    assert_eq!(
        results,
        [true, true],
        "every file, directory and byte must survive two independent pastes"
    );
    assert!(reads.load(Ordering::SeqCst) > 0);
    assert_eq!(
        ended.load(Ordering::SeqCst),
        2,
        "Shell must confirm completion, not merely finish reading"
    );
}
#[test]
#[ignore = "only launched by the parent clipboard probe"]
fn child_pastes() {
    let Ok(folder) = std::env::var("VNT_PASTE_TARGET") else {
        return;
    };
    assert!(paste_into_folder(std::path::Path::new(&folder)));
    assert!(verify(std::path::Path::new(&folder)));
    // Keep the STA alive for asynchronous Shell completion notifications.
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        vnt_clip::native::pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe {
        OleUninitialize();
    }
}
fn paste_into_folder(folder: &std::path::Path) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Ole::OleInitialize;

    use windows::Win32::UI::Shell::{
        IContextMenu, SHGetDesktopFolder, CMF_NORMAL, CMINVOKECOMMANDINFO,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateMenu, DestroyMenu, GetMenuItemCount, GetMenuItemID, HMENU,
    };
    unsafe {
        if OleInitialize(None).is_err() {
            eprintln!("CoInitializeEx 失败");
            return false;
        }
        let Ok(desktop) = SHGetDesktopFolder() else {
            eprintln!("拿不到桌面文件夹");
            return false;
        };
        let path: Vec<u16> = folder
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut pidl = std::ptr::null_mut();
        let mut attributes = 0u32;
        let pbc: Option<&windows::Win32::System::Com::IBindCtx> = None;
        if desktop
            .ParseDisplayName(
                windows::Win32::Foundation::HWND::default(),
                pbc,
                windows::core::PCWSTR(path.as_ptr()),
                None,
                &mut pidl,
                &mut attributes,
            )
            .is_err()
        {
            eprintln!("ParseDisplayName 失败");
            return false;
        }
        let items: [*const windows::Win32::UI::Shell::Common::ITEMIDLIST; 1] = [pidl as *const _];
        let menu: IContextMenu = match desktop.GetUIObjectOf(HWND::default(), &items, None) {
            Ok(menu) => menu,
            Err(e) => {
                eprintln!("GetUIObjectOf 失败: {e}");
                return false;
            }
        };
        let hmenu: HMENU = match CreateMenu() {
            Ok(menu) => menu,
            Err(e) => {
                eprintln!("CreateMenu 失败: {e}");
                return false;
            }
        };
        if menu
            .QueryContextMenu(hmenu, 0, 1, 0x7fff, CMF_NORMAL)
            .is_err()
        {
            eprintln!("QueryContextMenu 失败");
            let _ = DestroyMenu(hmenu);
            return false;
        }
        let count = GetMenuItemCount(Some(hmenu));
        eprintln!("上下文菜单项: {count}");
        let mut paste_id: Option<u32> = None;
        for index in 0..count {
            let id = GetMenuItemID(hmenu, index);
            if id == 0 || id == u32::MAX {
                continue;
            }
            // 动词名用 ANSI（GCS_VERBA=0）：crate 的签名只收 PSTR
            let mut verb = [0u8; 64];
            let written = menu.GetCommandString(
                (id - 1) as usize,
                0,
                Some(std::ptr::null_mut()),
                windows::core::PSTR(verb.as_mut_ptr()),
                verb.len() as u32,
            );
            if let Ok(()) = written {
                let end = verb.iter().position(|b| *b == 0).unwrap_or(verb.len());
                let text = String::from_utf8_lossy(&verb[..end]).to_string();
                eprintln!("  菜单 {index}: id={id} verb={text:?}");
                if text.eq_ignore_ascii_case("paste") {
                    paste_id = Some(id);
                }
            }
        }
        let Some(paste_id) = paste_id else {
            eprintln!("没找到粘贴菜单项（剪贴板里可能没有可粘贴的内容）");
            let _ = DestroyMenu(hmenu);
            return false;
        };
        eprintln!("调用粘贴动词 id={paste_id}");
        let info = CMINVOKECOMMANDINFO {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFO>() as u32,
            fMask: 0,
            hwnd: HWND::default(),
            lpVerb: windows::core::PCSTR((paste_id - 1) as *const u8),
            lpParameters: windows::core::PCSTR::null(),
            lpDirectory: windows::core::PCSTR::null(),
            nShow: 1,
            dwHotKey: 0,
            hIcon: windows::Win32::Foundation::HANDLE::default(),
        };
        let result = menu.InvokeCommand(&info);
        let _ = DestroyMenu(hmenu);
        if let Err(e) = result {
            eprintln!("InvokeCommand 失败: {e}");
            return false;
        }
    }
    true
}

struct OleOwner {
    stop: Arc<AtomicBool>,
    owner: Option<std::thread::JoinHandle<()>>,
}
impl Drop for OleOwner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(owner) = self.owner.take() {
            owner.join().unwrap();
        }
    }
}
fn publish(offer: Arc<vnt_proto::pb::ClipboardOffer>, provider: Arc<dyn FileProvider>) -> OleOwner {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let owner = std::thread::Builder::new()
        .name("probe-ole-owner".into())
        .spawn(move || {
            unsafe { OleInitialize(None) }.unwrap();
            let object = vnt_clip::native::data_object(offer, provider).unwrap();
            unsafe { OleSetClipboard(&object) }.unwrap();
            ready_tx.send(()).unwrap();
            while !stopped.load(Ordering::SeqCst) {
                vnt_clip::native::pump();
                std::thread::sleep(Duration::from_millis(5));
            }
            if vnt_clip::native::is_current_object(&object) {
                let _ = unsafe { OleSetClipboard(None::<&IDataObject>) };
            }
            drop(object);
            unsafe {
                OleUninitialize();
            }
        })
        .unwrap();
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    OleOwner {
        stop,
        owner: Some(owner),
    }
}

/// Throttled stream forces the actual Shell progress dialog to stay visible.
/// UI Automation is restricted to the disposable consumer process, never other copies.
#[test]
#[ignore = "shows and cancels a real Shell copy dialog on the interactive desktop"]
fn shell_native_progress_and_cancel() {
    let _guard = vnt_clip::CLIPBOARD_LOCK.lock().unwrap();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("slow.bin");
    std::fs::write(&file, vec![42u8; 16 * 1024 * 1024]).unwrap();
    let offer = Arc::new(SourceOffer::collect(1, std::slice::from_ref(&file)).unwrap());
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let reads = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Provider {
        offer: offer.clone(),
        reads: reads.clone(),
        ended: Arc::new(AtomicUsize::new(0)),
        delay: Duration::from_millis(20),
        outcomes: outcomes.clone(),
    });
    let owner = publish(Arc::new(offer.manifest.clone()), provider);
    let destination = tempfile::tempdir().unwrap();
    let stopped = destination.path().join("consumer.stop");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "child_slow_pastes",
            "--ignored",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("VNT_PASTE_TARGET", destination.path())
        .env("VNT_PASTE_STOP", &stopped)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let script = r#"
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class VntProgressTest { [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp); }'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$processCondition = [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::ProcessIdProperty, [int]$env:VNT_CONSUMER_PID)
$deadline = [DateTime]::UtcNow.AddSeconds(15)
$reported = $false
while ([DateTime]::UtcNow -lt $deadline) {
  $windows = [System.Windows.Automation.AutomationElement]::RootElement.FindAll([System.Windows.Automation.TreeScope]::Children, $processCondition)
  foreach ($window in $windows) {
    if ($window.Current.ClassName -notin @('OperationStatusWindow', '#32770')) { continue }
    if (-not $reported) {
      Write-Output ('NATIVE_PROGRESS ' + $window.Current.Name)
      $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
      foreach ($element in $elements) { Write-Output ('ELEMENT ' + $element.Current.ControlType.ProgrammaticName + ' ' + $element.Current.Name + ' ID=' + $element.Current.AutomationId) }
      $reported = $true
    }
    $condition = [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::AutomationIdProperty, 'CancelButton')
    $buttons = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $condition)
    foreach ($button in $buttons) {
      Write-Output ('BUTTON ' + $button.Current.Name + ' ID=' + $button.Current.AutomationId)
      if ($button.Current.Name -match '取消|Cancel') {
        # IDCANCEL on this standard Shell dialog; hwnd is owned by our disposable child.
        if (-not [VntProgressTest]::PostMessage([IntPtr]$window.Current.NativeWindowHandle, 0x0111, [IntPtr]2, [IntPtr]::Zero)) { exit 3 }
        Write-Output 'NATIVE_CANCEL_INVOKED'
        exit 0
      }
    }
  }
  Start-Sleep -Milliseconds 100
}
exit 2
"#;
    let result = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .env("VNT_CONSUMER_PID", child.id().to_string())
        .output()
        .unwrap();
    eprintln!("{}", String::from_utf8_lossy(&result.stdout));
    eprintln!("{}", String::from_utf8_lossy(&result.stderr));
    let deadline = Instant::now() + Duration::from_secs(5);
    while outcomes.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(&stopped, []).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(owner);
    assert!(
        result.status.success(),
        "could not find/invoke the native copy dialog's Cancel button"
    );
    assert_eq!(
        &*outcomes.lock().unwrap(),
        &[false],
        "Shell cancellation must not report success"
    );
    assert!(
        reads.load(Ordering::SeqCst) < 512,
        "cancel must stop the actual data stream early"
    );
    assert!(
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(file)
            .is_ok(),
        "cancel must release the source handle"
    );
}
#[test]
#[ignore = "only launched by the progress/cancellation probe"]
fn child_slow_pastes() {
    let Ok(folder) = std::env::var("VNT_PASTE_TARGET") else {
        return;
    };
    let stopped = std::path::PathBuf::from(std::env::var("VNT_PASTE_STOP").unwrap());
    assert!(paste_into_folder(std::path::Path::new(&folder)));
    let deadline = Instant::now() + Duration::from_secs(30);
    while !stopped.exists() && Instant::now() < deadline {
        vnt_clip::native::pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    unsafe {
        OleUninitialize();
    }
}

#[test]
#[ignore = "uses the real clipboard to verify S_FALSE ownership handling"]
fn ownership_check_preserves_a_newer_local_clipboard() {
    let _guard = vnt_clip::CLIPBOARD_LOCK.lock().unwrap();
    unsafe { OleInitialize(None) }.unwrap();
    let source = tempfile::tempdir().unwrap();
    let file = source.path().join("a");
    std::fs::write(&file, b"x").unwrap();
    let offer = Arc::new(SourceOffer::collect(1, &[file]).unwrap());
    let provider = Arc::new(Provider {
        offer: offer.clone(),
        reads: Arc::new(AtomicUsize::new(0)),
        ended: Arc::new(AtomicUsize::new(0)),
        delay: Duration::ZERO,
        outcomes: Arc::new(Mutex::new(Vec::new())),
    });
    let object = vnt_clip::native::data_object(Arc::new(offer.manifest.clone()), provider).unwrap();
    unsafe { OleSetClipboard(&object) }.unwrap();
    assert!(vnt_clip::native::is_current_object(&object));
    vnt_clip::win::set_text("new local copy").unwrap();
    assert!(
        !vnt_clip::native::is_current_object(&object),
        "S_FALSE must not be mistaken for ownership"
    );
    assert!(
        matches!(vnt_clip::win::read().unwrap(), Some(vnt_clip::ClipboardContent::Text(text)) if text == "new local copy")
    );
    drop(object);
    unsafe {
        OleUninitialize();
    }
}
