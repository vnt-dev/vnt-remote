#![cfg(target_os = "linux")]
use std::time::{Duration, Instant};
use vnt_input::{InputInjector, LinuxInputInjector, MouseButton};
use vnt_linux::x11rb::{
    self,
    connection::Connection,
    protocol::{
        xproto::{self, ConnectionExt},
        Event,
    },
};
#[test]
#[ignore = "requires an isolated X11 display"]
fn xtest_routes_real_pointer_keyboard_and_releases_held_keys() {
    let (conn, index) = x11rb::connect(None).unwrap();
    let screen = &conn.setup().roots[index];
    let window = conn.generate_id().unwrap();
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        100,
        100,
        300,
        300,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &xproto::CreateWindowAux::new().event_mask(
            xproto::EventMask::KEY_PRESS
                | xproto::EventMask::KEY_RELEASE
                | xproto::EventMask::BUTTON_PRESS
                | xproto::EventMask::BUTTON_RELEASE,
        ),
    )
    .unwrap()
    .check()
    .unwrap();
    conn.map_window(window).unwrap().check().unwrap();
    conn.set_input_focus(xproto::InputFocus::PARENT, window, x11rb::CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    let input = LinuxInputInjector::for_display(0).unwrap();
    input
        .mouse_move(
            150. / (screen.width_in_pixels - 1) as f32,
            150. / (screen.height_in_pixels - 1) as f32,
        )
        .unwrap();
    let pointer = conn.query_pointer(screen.root).unwrap().reply().unwrap();
    assert_eq!((pointer.root_x, pointer.root_y), (150, 150));
    input.key(0x1d, false, true).unwrap();
    input.key(0x2f, false, true).unwrap();
    input.key(0x2f, false, false).unwrap();
    input.mouse_button(MouseButton::Left, true).unwrap();
    input.mouse_button(MouseButton::Left, false).unwrap();
    input.wheel(1., 1.).unwrap();
    drop(input); // releases Ctrl even without an explicit key-up
    let due = Instant::now() + Duration::from_secs(2);
    let mut keys = (0, 0);
    let mut buttons = (0, 0);
    while Instant::now() < due {
        match conn.poll_for_event().unwrap() {
            Some(Event::KeyPress(_)) => keys.0 += 1,
            Some(Event::KeyRelease(_)) => keys.1 += 1,
            Some(Event::ButtonPress(_)) => buttons.0 += 1,
            Some(Event::ButtonRelease(_)) => buttons.1 += 1,
            _ => (),
        }
        if keys == (2, 2) && buttons == (3, 3) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(keys, (2, 2));
    assert_eq!(buttons, (3, 3));
}
