use std::time::{Duration, Instant};
use vnt_linux::x11rb::{
    self,
    connection::Connection,
    protocol::{
        xproto::{self, ConnectionExt},
        Event,
    },
};
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub fn pick() -> Result<Option<vnt_capture::ScreenRegion>, String> {
    vnt_linux::session::check_x11()?;
    let monitors = screencap::Monitor::all().map_err(error)?;
    let (conn, screen) = x11rb::connect(None).map_err(error)?;
    let screen = &conn.setup().roots[screen];
    let pixmap = conn.generate_id().map_err(error)?;
    let gc = conn.generate_id().map_err(error)?;
    conn.create_pixmap(
        screen.root_depth,
        pixmap,
        screen.root,
        screen.width_in_pixels,
        screen.height_in_pixels,
    )
    .map_err(error)?
    .check()
    .map_err(error)?;
    conn.create_gc(
        gc,
        pixmap,
        &xproto::CreateGCAux::new()
            .foreground(screen.white_pixel)
            .line_width(2),
    )
    .map_err(error)?
    .check()
    .map_err(error)?;
    conn.copy_area(
        screen.root,
        pixmap,
        gc,
        0,
        0,
        0,
        0,
        screen.width_in_pixels,
        screen.height_in_pixels,
    )
    .map_err(error)?
    .check()
    .map_err(error)?;
    let overlay = conn.generate_id().map_err(error)?;
    conn.create_window(
        screen.root_depth,
        overlay,
        screen.root,
        0,
        0,
        screen.width_in_pixels,
        screen.height_in_pixels,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &xproto::CreateWindowAux::new()
            .override_redirect(1)
            .background_pixmap(pixmap)
            .event_mask(
                xproto::EventMask::EXPOSURE
                    | xproto::EventMask::BUTTON_PRESS
                    | xproto::EventMask::BUTTON_RELEASE
                    | xproto::EventMask::POINTER_MOTION
                    | xproto::EventMask::KEY_PRESS,
            ),
    )
    .map_err(error)?
    .check()
    .map_err(error)?;
    conn.map_window(overlay)
        .map_err(error)?
        .check()
        .map_err(error)?;
    let keyboard = conn
        .grab_keyboard(
            false,
            overlay,
            x11rb::CURRENT_TIME,
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
        )
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let pointer = conn
        .grab_pointer(
            false,
            overlay,
            xproto::EventMask::BUTTON_PRESS
                | xproto::EventMask::BUTTON_RELEASE
                | xproto::EventMask::POINTER_MOTION,
            xproto::GrabMode::ASYNC,
            xproto::GrabMode::ASYNC,
            overlay,
            x11rb::NONE,
            x11rb::CURRENT_TIME,
        )
        .map_err(error)?
        .reply()
        .map_err(error)?;
    if keyboard.status != xproto::GrabStatus::SUCCESS
        || pointer.status != xproto::GrabStatus::SUCCESS
    {
        return Err("无法开始框选：键盘或鼠标正被其他应用占用".into());
    }
    let setup = conn.setup();
    let mapping = conn
        .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
        .map_err(error)?
        .reply()
        .map_err(error)?;
    let escape = mapping
        .keysyms
        .chunks(usize::from(mapping.keysyms_per_keycode))
        .position(|keys| keys.contains(&0xff1b))
        .map(|i| setup.min_keycode + i as u8);
    let mut start = None;
    let mut selected = None;
    let due = Instant::now() + Duration::from_secs(120);
    conn.flush().map_err(error)?;
    while Instant::now() < due {
        let Some(event) = conn.poll_for_event().map_err(error)? else {
            std::thread::sleep(Duration::from_millis(8));
            continue;
        };
        match event {
            Event::KeyPress(e) if Some(e.detail) == escape => return Ok(None),
            Event::ButtonPress(e) if e.detail == 3 => return Ok(None),
            Event::ButtonPress(e) if e.detail == 1 => {
                start = Some((i32::from(e.root_x), i32::from(e.root_y)));
                selected = monitors.iter().position(|m| {
                    let (x, y) = m.origin();
                    let (w, h) = m.size().unwrap_or_default();
                    e.root_x >= x
                        && e.root_y >= y
                        && i32::from(e.root_x) < i32::from(x) + w as i32
                        && i32::from(e.root_y) < i32::from(y) + h as i32
                });
            }
            Event::MotionNotify(e) if start.is_some() => {
                conn.copy_area(
                    pixmap,
                    overlay,
                    gc,
                    0,
                    0,
                    0,
                    0,
                    screen.width_in_pixels,
                    screen.height_in_pixels,
                )
                .map_err(error)?;
                if let (Some(start), Some(index)) = (start, selected) {
                    let m = &monitors[index];
                    if let Some((x, y, w, h)) = rectangle(
                        start,
                        (i32::from(e.root_x), i32::from(e.root_y)),
                        m.origin(),
                        m.size().map_err(error)?,
                    ) {
                        let (ox, oy) = m.origin();
                        conn.poly_rectangle(
                            overlay,
                            gc,
                            &[xproto::Rectangle {
                                x: (i32::from(ox) + x as i32) as i16,
                                y: (i32::from(oy) + y as i32) as i16,
                                width: w as u16,
                                height: h as u16,
                            }],
                        )
                        .map_err(error)?;
                    }
                }
                conn.flush().map_err(error)?;
            }
            Event::ButtonRelease(e) if e.detail == 1 => {
                if let (Some(start), Some(index)) = (start, selected) {
                    let m = &monitors[index];
                    let size = m.size().map_err(error)?;
                    if let Some((x, y, width, height)) = rectangle(
                        start,
                        (i32::from(e.root_x), i32::from(e.root_y)),
                        m.origin(),
                        size,
                    ) {
                        let mut random = [0; 16];
                        getrandom::fill(&mut random).map_err(error)?;
                        let result = vnt_capture::ScreenRegion {
                            id: random.iter().map(|v| format!("{v:02x}")).collect(),
                            name: "屏幕区域".into(),
                            monitor_name: m.device_name().map_err(error)?,
                            screen_width: size.0,
                            screen_height: size.1,
                            orientation: m.orientation(),
                            x,
                            y,
                            width,
                            height,
                        };
                        result.validate()?;
                        return Ok(Some(result));
                    }
                }
                return Ok(None);
            }
            _ => (),
        }
    }
    Ok(None)
}
fn rectangle(
    start: (i32, i32),
    end: (i32, i32),
    origin: (i16, i16),
    size: (u32, u32),
) -> Option<(u32, u32, u32, u32)> {
    let sx = (start.0 - i32::from(origin.0)).clamp(0, size.0 as i32);
    let sy = (start.1 - i32::from(origin.1)).clamp(0, size.1 as i32);
    let ex = (end.0 - i32::from(origin.0)).clamp(0, size.0 as i32);
    let ey = (end.1 - i32::from(origin.1)).clamp(0, size.1 as i32);
    let width = sx.abs_diff(ex) & !1;
    let height = sy.abs_diff(ey) & !1;
    (width >= 64 && height >= 64).then_some((sx.min(ex) as u32, sy.min(ey) as u32, width, height))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reverse_drag_clamps_to_one_monitor_and_even_physical_pixels() {
        assert_eq!(
            rectangle((200, 200), (50, 49), (0, 0), (1920, 1080)),
            Some((50, 49, 150, 150))
        );
        assert_eq!(
            rectangle((2000, 100), (0, 900), (1920, 0), (1920, 1080)),
            Some((0, 100, 80, 800))
        );
        assert_eq!(rectangle((10, 10), (70, 70), (0, 0), (1920, 1080)), None);
    }
    #[test]
    #[ignore = "requires an isolated X11 display"]
    fn linux_native_picker_drag_and_escape_release_grabs() {
        use vnt_linux::x11rb::protocol::xtest;
        let (conn, index) = x11rb::connect(None).unwrap();
        let screen = &conn.setup().roots[index];
        let ready = || {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let children = conn
                    .query_tree(screen.root)
                    .unwrap()
                    .reply()
                    .unwrap()
                    .children;
                if children.into_iter().any(|w| {
                    let attributes = conn.get_window_attributes(w).unwrap().reply().ok();
                    attributes.is_some_and(|a| {
                        a.override_redirect && a.map_state == xproto::MapState::VIEWABLE
                    })
                }) {
                    std::thread::sleep(Duration::from_millis(50));
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let send = |kind, detail, x, y| {
            xtest::fake_input(
                &conn,
                kind,
                detail,
                x11rb::CURRENT_TIME,
                screen.root,
                x,
                y,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
        };
        let picker = std::thread::spawn(pick);
        ready();
        send(xproto::MOTION_NOTIFY_EVENT, 0, 100, 100);
        send(xproto::BUTTON_PRESS_EVENT, 1, 0, 0);
        send(xproto::MOTION_NOTIFY_EVENT, 0, 400, 300);
        send(xproto::BUTTON_RELEASE_EVENT, 1, 0, 0);
        let result = picker.join().unwrap().unwrap().unwrap();
        assert_eq!(
            (result.x, result.y, result.width, result.height),
            (100, 100, 300, 200)
        );
        let picker = std::thread::spawn(pick);
        ready();
        let setup = conn.setup();
        let mapping = conn
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
            .unwrap()
            .reply()
            .unwrap();
        let escape = mapping
            .keysyms
            .chunks(usize::from(mapping.keysyms_per_keycode))
            .position(|keys| keys.contains(&0xff1b))
            .unwrap() as u8
            + setup.min_keycode;
        send(xproto::KEY_PRESS_EVENT, escape, 0, 0);
        send(xproto::KEY_RELEASE_EVENT, escape, 0, 0);
        assert!(picker.join().unwrap().unwrap().is_none());
        assert_eq!(
            conn.grab_keyboard(
                false,
                screen.root,
                x11rb::CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC
            )
            .unwrap()
            .reply()
            .unwrap()
            .status,
            xproto::GrabStatus::SUCCESS
        );
        conn.ungrab_keyboard(x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
    }
}
