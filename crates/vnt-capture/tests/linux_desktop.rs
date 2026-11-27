#![cfg(target_os = "linux")]
use vnt_linux::x11rb::{
    self,
    connection::Connection,
    protocol::xproto::{self, ConnectionExt},
};
#[test]
#[ignore = "requires an isolated X11 display"]
fn xcb_captures_known_pixels_and_stable_monitor_metadata() {
    let (conn, index) = x11rb::connect(None).unwrap();
    let screen = &conn.setup().roots[index];
    let window = conn.generate_id().unwrap();
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        200,
        200,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &xproto::CreateWindowAux::new().background_pixel(0x2244cc),
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(window).unwrap().check().unwrap();
    let monitors = vnt_capture::list_available_monitors().unwrap();
    assert!(!monitors[0].name.is_empty());
    assert!(monitors[0].orientation <= 3);
    let source = vnt_capture::SharedCapture::monitor(0);
    let mut sub = source.subscribe(10).unwrap();
    let frame = sub.recv().unwrap();
    assert_eq!(
        (frame.width, frame.height),
        (monitors[0].width, monitors[0].height)
    );
    assert_eq!(
        frame.source_monitor.as_ref().unwrap().name,
        monitors[0].name
    );
    assert!(
        frame
            .bgra
            .chunks_exact(4)
            .filter(|p| *p == [0xcc, 0x44, 0x22, 0xff])
            .count()
            > 10000
    );
    drop(sub);
    source.cancel();
}
