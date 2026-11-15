#![cfg(target_os = "linux")]
use std::{
    io::Write,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use vnt_clip::{
    native::NativeClipboard,
    win::{self, LocalSnapshot},
};
struct External(Child);
impl Drop for External {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn external(data: &[u8], target: &str) -> External {
    let before = win::sequence_number();
    let mut child = Command::new("xclip")
        .args([
            "-selection",
            "clipboard",
            "-in",
            "-quiet",
            "-target",
            target,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let due = Instant::now() + Duration::from_secs(2);
    while win::sequence_number() == before {
        assert!(Instant::now() < due);
        std::thread::sleep(Duration::from_millis(5));
    }
    External(child)
}
fn read_external(target: &str) -> Vec<u8> {
    let output = Command::new("xclip")
        .args(["-selection", "clipboard", "-out", "-target", target])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
#[test]
#[ignore = "requires an isolated X11 display and xclip"]
fn bounded_incr_text_png_and_ownership_roundtrip() {
    let _guard = vnt_clip::CLIPBOARD_LOCK.lock().unwrap();
    let clipboard = NativeClipboard::start().unwrap();
    let text = "Linux 剪贴板 ✅\n".repeat(100_000);
    clipboard
        .text(text.clone(), win::sequence_number())
        .unwrap();
    assert!(matches!(win::snapshot().unwrap(), LocalSnapshot::Ours));
    assert_eq!(read_external("UTF8_STRING"), text.as_bytes());
    let old = win::sequence_number();
    {
        let _copy = external(text.as_bytes(), "UTF8_STRING");
        assert!(matches!(win::snapshot().unwrap(),LocalSnapshot::Text(t) if t==text));
        assert!(clipboard.text("stale".into(), old).is_err());
    }
    // A tiny valid PNG exercises the image target and lossless conversion.
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
            ])
            .unwrap();
    }
    {
        let _copy = external(&png, "image/png");
        let LocalSnapshot::Image(raw) = win::snapshot().unwrap() else {
            panic!("expected image");
        };
        assert_eq!(raw.decode().unwrap().png, png);
    }
    let image =
        vnt_clip::image::PreparedImage::new(vnt_clip::image::from_png(png.clone()).unwrap())
            .unwrap();
    let sequence = win::sequence_number();
    clipboard.image(image, sequence, Arc::new(|| true)).unwrap();
    assert_eq!(read_external("image/png"), png);
    let too_large = "x".repeat(vnt_proto::control::MAX_CLIPBOARD_TEXT + 1);
    assert!(clipboard
        .text(too_large.clone(), win::sequence_number())
        .is_err());
    let _copy = external(too_large.as_bytes(), "UTF8_STRING");
    assert!(win::snapshot().is_err());
}
