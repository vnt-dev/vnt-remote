//! Native, physical-pixel screenshot selector. All HWND/GDI state lives on its UI thread.
use std::ffi::c_void;
use vnt_capture::{MonitorInfo, ScreenRegion};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, SetFocus},
            WindowsAndMessaging::*,
        },
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}
impl Rect {
    fn width(self) -> i32 {
        self.r - self.l
    }
    fn height(self) -> i32 {
        self.b - self.t
    }
    fn has(self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    fn native(self) -> RECT {
        RECT {
            left: self.l,
            top: self.t,
            right: self.r,
            bottom: self.b,
        }
    }
    fn aligned(self) -> Self {
        Self {
            r: self.l + (self.width() & !1),
            b: self.t + (self.height() & !1),
            ..self
        }
    }
}
fn drag_rect(b: Rect, (sx, sy): (i32, i32), old: Rect, mode: u8, (x, y): (i32, i32)) -> Rect {
    let x = x.clamp(b.l, b.r);
    let y = y.clamp(b.t, b.b);
    let mut r = old;
    if mode == 0 {
        r = Rect {
            l: sx.min(x),
            t: sy.min(y),
            r: sx.max(x),
            b: sy.max(y),
        };
    } else if mode == 16 {
        let dx = (x - sx).clamp(b.l - old.l, b.r - old.r);
        let dy = (y - sy).clamp(b.t - old.t, b.b - old.b);
        r = Rect {
            l: old.l + dx,
            t: old.t + dy,
            r: old.r + dx,
            b: old.b + dy,
        };
    } else {
        if mode & 1 != 0 {
            r.l = x;
        }
        if mode & 2 != 0 {
            r.r = x;
        }
        if mode & 4 != 0 {
            r.t = y;
        }
        if mode & 8 != 0 {
            r.b = y;
        }
        if r.l > r.r {
            std::mem::swap(&mut r.l, &mut r.r);
        }
        if r.t > r.b {
            std::mem::swap(&mut r.t, &mut r.b);
        }
    }
    r.aligned()
}
struct Shot {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}
impl Drop for Shot {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}
impl Shot {
    unsafe fn new(screen: HDC, width: i32, height: i32) -> Result<(Self, *mut u8), String> {
        let dc = CreateCompatibleDC(Some(screen));
        if dc.0.is_null() {
            return Err("无法创建框选画布".into());
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(b) => b,
            Err(e) => {
                let _ = DeleteDC(dc);
                return Err(e.to_string());
            }
        };
        let previous = SelectObject(dc, bitmap.into());
        Ok((
            Self {
                dc,
                bitmap,
                previous,
            },
            bits.cast(),
        ))
    }
}
struct Picker {
    monitors: Vec<(MonitorInfo, Rect)>,
    size: (i32, i32),
    original: Shot,
    dim: Shot,
    backbuffer: Shot,
    selected: Option<Rect>,
    monitor: Option<usize>,
    drag: Option<(i32, i32, Rect, u8)>,
    result: Option<ScreenRegion>,
}
impl Picker {
    fn toolbar(&self) -> Rect {
        let bounds = self.monitor.map(|i| self.monitors[i].1).unwrap_or(Rect {
            l: 0,
            t: 0,
            r: self.size.0,
            b: self.size.1,
        });
        let s = self.selected.unwrap_or(bounds);
        let l = s.l.clamp(bounds.l, (bounds.r - 300).max(bounds.l));
        let t = (s.b + 10).min(bounds.b - 44).max(bounds.t);
        Rect {
            l,
            t,
            r: l + 300,
            b: t + 36,
        }
    }
    fn valid(&self) -> bool {
        self.selected
            .is_some_and(|r| r.width() >= 64 && r.height() >= 64)
    }
    fn confirm(&mut self, hwnd: HWND) {
        if !self.valid() {
            return;
        }
        let r = self.selected.unwrap();
        let (m, bounds) = &self.monitors[self.monitor.unwrap()];
        self.result = Some(ScreenRegion {
            id: String::new(),
            name: String::new(),
            monitor_name: m.name.clone(),
            screen_width: m.width,
            screen_height: m.height,
            orientation: m.orientation,
            x: (r.l - bounds.l) as u32,
            y: (r.t - bounds.t) as u32,
            width: r.width() as u32,
            height: r.height() as u32,
        });
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
    fn down(&mut self, hwnd: HWND, x: i32, y: i32) {
        if self.selected.is_some() {
            let bar = self.toolbar();
            if bar.has(x, y) {
                match (x - bar.l) / 100 {
                    0 => self.confirm(hwnd),
                    1 => {
                        self.selected = None;
                        self.monitor = None;
                    }
                    _ => unsafe {
                        let _ = DestroyWindow(hwnd);
                    },
                }
                return;
            }
        }
        if let (Some(r), Some(i)) = (self.selected, self.monitor) {
            let mut edge = 0;
            if y >= r.t - 8 && y <= r.b + 8 {
                if (x - r.l).abs() <= 8 {
                    edge |= 1;
                }
                if (x - r.r).abs() <= 8 {
                    edge |= 2;
                }
            }
            if x >= r.l - 8 && x <= r.r + 8 {
                if (y - r.t).abs() <= 8 {
                    edge |= 4;
                }
                if (y - r.b).abs() <= 8 {
                    edge |= 8;
                }
            }
            if edge != 0 || r.has(x, y) {
                self.monitor = Some(i);
                self.drag = Some((x, y, r, if edge == 0 { 16 } else { edge }));
                unsafe {
                    SetCapture(hwnd);
                }
                return;
            }
        }
        self.monitor = self.monitors.iter().position(|(_, b)| b.has(x, y));
        if self.monitor.is_some() {
            let r = Rect {
                l: x,
                t: y,
                r: x,
                b: y,
            };
            self.selected = Some(r);
            self.drag = Some((x, y, r, 0));
            unsafe {
                SetCapture(hwnd);
            }
        }
    }
    fn motion(&mut self, x: i32, y: i32) -> bool {
        let Some((sx, sy, old, mode)) = self.drag else {
            return false;
        };
        let b = self.monitors[self.monitor.unwrap()].1;
        let next = Some(drag_rect(b, (sx, sy), old, mode, (x, y)));
        if self.selected == next {
            return false;
        }
        self.selected = next;
        true
    }
    unsafe fn paint(&self, hwnd: HWND) {
        self.compose();
        let mut ps = PAINTSTRUCT::default();
        let window_dc = BeginPaint(hwnd, &mut ps);
        let _ = BitBlt(
            window_dc,
            0,
            0,
            self.size.0,
            self.size.1,
            Some(self.backbuffer.dc),
            0,
            0,
            SRCCOPY,
        );
        let _ = EndPaint(hwnd, &ps);
    }
    unsafe fn compose(&self) {
        // Compose the entire frame offscreen; never expose the dim-only stage
        // while updating the selection, handles, labels and toolbar.
        let dc = self.backbuffer.dc;
        let _ = BitBlt(
            dc,
            0,
            0,
            self.size.0,
            self.size.1,
            Some(self.dim.dc),
            0,
            0,
            SRCCOPY,
        );
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0xffffff));
        for (_, b) in &self.monitors {
            label(
                dc,
                "拖动选择区域 · 最小 64 × 64 · Enter 确认 · Esc 取消",
                Rect {
                    l: b.l + 16,
                    t: b.t + 20,
                    r: b.r - 16,
                    b: b.t + 60,
                },
            );
        }
        if let Some(r) = self.selected {
            let _ = BitBlt(
                dc,
                r.l,
                r.t,
                r.width(),
                r.height(),
                Some(self.original.dc),
                r.l,
                r.t,
                SRCCOPY,
            );
            let brush = CreateSolidBrush(COLORREF(0xe8c345));
            let _ = FrameRect(dc, &r.native(), brush);
            for x in [r.l, (r.l + r.r) / 2, r.r] {
                for y in [r.t, (r.t + r.b) / 2, r.b] {
                    if x == (r.l + r.r) / 2 && y == (r.t + r.b) / 2 {
                        continue;
                    }
                    let h = RECT {
                        left: x - 4,
                        top: y - 4,
                        right: x + 4,
                        bottom: y + 4,
                    };
                    let _ = FillRect(dc, &h, brush);
                }
            }
            let _ = DeleteObject(brush.into());
            let bounds = self.monitors[self.monitor.unwrap()].1;
            let t = (r.t - 32).max(bounds.t + 60);
            label(
                dc,
                &format!(
                    "{} × {} 像素{}",
                    r.width(),
                    r.height(),
                    if self.valid() { "" } else { " · 区域过小" }
                ),
                Rect {
                    l: r.l,
                    t,
                    r: (r.l + 320).min(bounds.r),
                    b: t + 28,
                },
            );
            if self.drag.is_none() {
                let bar = self.toolbar();
                let brush = CreateSolidBrush(COLORREF(0x282420));
                let _ = FillRect(dc, &bar.native(), brush);
                let _ = DeleteObject(brush.into());
                for (i, text) in [
                    if self.valid() {
                        "确认"
                    } else {
                        "区域过小"
                    },
                    "重选",
                    "取消",
                ]
                .iter()
                .enumerate()
                {
                    label(
                        dc,
                        text,
                        Rect {
                            l: bar.l + i as i32 * 100,
                            r: bar.l + (i as i32 + 1) * 100,
                            ..bar
                        },
                    );
                }
            }
        }
    }
}
unsafe fn label(dc: HDC, text: &str, r: Rect) {
    let mut text: Vec<u16> = text.encode_utf16().collect();
    let mut rect = r.native();
    DrawTextW(
        dc,
        &mut text,
        &mut rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}
unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(l.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Picker;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    let state = &mut *ptr;
    let x = (l.0 as u16 as i16) as i32;
    let y = ((l.0 >> 16) as u16 as i16) as i32;
    match msg {
        WM_PAINT => {
            state.paint(hwnd);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_LBUTTONDOWN => state.down(hwnd, x, y),
        WM_MOUSEMOVE => {
            if !state.motion(x, y) {
                return LRESULT(0);
            }
        }
        WM_LBUTTONUP => {
            state.motion(x, y);
            state.drag = None;
            let _ = ReleaseCapture();
        }
        WM_KEYDOWN => match w.0 {
            13 => state.confirm(hwnd),
            27 => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        },
        WM_DISPLAYCHANGE | WM_DPICHANGED | WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
        }
        WM_ACTIVATE if w.0 & 0xffff == WA_INACTIVE as usize => {
            let _ = DestroyWindow(hwnd);
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => return DefWindowProcW(hwnd, msg, w, l),
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
    LRESULT(0)
}

pub fn pick() -> Result<Option<ScreenRegion>, String> {
    pick_with_hook(|_| {})
}

fn pick_with_hook(on_ready: impl FnOnce(HWND)) -> Result<Option<ScreenRegion>, String> {
    use windows::Win32::UI::HiDpi::*;
    struct Dpi(DPI_AWARENESS_CONTEXT);
    impl Drop for Dpi {
        fn drop(&mut self) {
            unsafe {
                SetThreadDpiAwarenessContext(self.0);
            }
        }
    }
    unsafe {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        if previous.0.is_null() {
            return Err("无法启用物理像素框选，请重试".into());
        }
        let _dpi = Dpi(previous);
        let monitors = vnt_capture::list_monitors().map_err(|e| e.to_string())?;
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if width <= 0 || height <= 0 || width as u64 * height as u64 > 134_217_728 {
            return Err("桌面尺寸不支持框选".into());
        }
        let screen = GetDC(None);
        let shots = (|| {
            let (original, _) = Shot::new(screen, width, height)?;
            let (dim, bits) = Shot::new(screen, width, height)?;
            let (backbuffer, _) = Shot::new(screen, width, height)?;
            BitBlt(
                original.dc,
                0,
                0,
                width,
                height,
                Some(screen),
                vx,
                vy,
                SRCCOPY | CAPTUREBLT,
            )
            .map_err(|e| e.to_string())?;
            BitBlt(
                dim.dc,
                0,
                0,
                width,
                height,
                Some(original.dc),
                0,
                0,
                SRCCOPY,
            )
            .map_err(|e| e.to_string())?;
            let _ = GdiFlush();
            for pixel in std::slice::from_raw_parts_mut(bits, width as usize * height as usize * 4)
                .as_chunks_mut::<4>()
                .0
            {
                for c in &mut pixel[..3] {
                    *c = (*c as u16 * 45 / 100) as u8;
                }
            }
            Ok::<_, String>((original, dim, backbuffer))
        })();
        ReleaseDC(None, screen);
        let (original, dim, backbuffer) = shots?;
        let mut entries = Vec::new();
        for monitor in monitors {
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(HMONITOR(monitor.hmonitor as *mut _), &mut info).as_bool() {
                return Err("显示器已变化，请重试".into());
            }
            let r = info.rcMonitor;
            entries.push((
                monitor,
                Rect {
                    l: r.left - vx,
                    t: r.top - vy,
                    r: r.right - vx,
                    b: r.bottom - vy,
                },
            ));
        }
        let mut state = Box::new(Picker {
            monitors: entries,
            size: (width, height),
            original,
            dim,
            backbuffer,
            selected: None,
            monitor: None,
            drag: None,
            result: None,
        });
        let module = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = w!("VntScreenRegionPicker");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: module.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_CROSS).map_err(|e| e.to_string())?,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("选择屏幕区域"),
            WS_POPUP,
            vx,
            vy,
            width,
            height,
            None,
            None,
            Some(module.into()),
            Some((&mut *state as *mut Picker).cast::<c_void>()),
        )
        .map_err(|e| e.to_string())?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        on_ready(hwnd);
        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0).0;
            if result == 0 {
                break;
            }
            if result == -1 {
                let _ = DestroyWindow(hwnd);
                return Err("屏幕区域选择器异常退出".into());
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if let Some(r) = &state.result {
            r.validate()?;
            r.resolve(&vnt_capture::list_monitors().map_err(|e| e.to_string())?)?;
        }
        Ok(state.result.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moving_selection_composes_complete_frames_without_changing_screenshots() {
        unsafe {
            let screen = GetDC(None);
            let (original, _) = Shot::new(screen, 400, 300).unwrap();
            let (dim, _) = Shot::new(screen, 400, 300).unwrap();
            let (backbuffer, _) = Shot::new(screen, 400, 300).unwrap();
            ReleaseDC(None, screen);
            let bounds = Rect {
                l: 0,
                t: 0,
                r: 400,
                b: 300,
            };
            for (shot, color) in [(&original, 0xffffff), (&dim, 0x404040)] {
                let brush = CreateSolidBrush(COLORREF(color));
                assert_ne!(FillRect(shot.dc, &bounds.native(), brush), 0);
                let _ = DeleteObject(brush.into());
            }
            let selected = Rect {
                l: 100,
                t: 100,
                r: 220,
                b: 220,
            };
            let mut picker = Picker {
                monitors: vec![(vnt_capture::synthetic_monitor(0, 400, 300), bounds)],
                size: (400, 300),
                original,
                dim,
                backbuffer,
                selected: Some(selected),
                monitor: Some(0),
                drag: None,
                result: None,
            };
            assert!(!picker.motion(150, 180), "hover must not trigger repaint");
            picker.compose();
            assert_eq!(GetPixel(picker.backbuffer.dc, 150, 180), COLORREF(0xffffff));
            assert_eq!(GetPixel(picker.backbuffer.dc, 230, 180), COLORREF(0x404040));

            picker.drag = Some((150, 180, selected, 16));
            assert!(!picker.motion(150, 180));
            assert!(picker.motion(230, 180));
            assert!(!picker.motion(230, 180), "unchanged drag must not repaint");
            picker.compose();
            assert_eq!(GetPixel(picker.backbuffer.dc, 150, 180), COLORREF(0x404040));
            assert_eq!(GetPixel(picker.backbuffer.dc, 230, 180), COLORREF(0xffffff));
            assert_eq!(GetPixel(picker.original.dc, 150, 180), COLORREF(0xffffff));
            assert_eq!(GetPixel(picker.dim.dc, 230, 180), COLORREF(0x404040));
        }
    }

    #[test]
    fn reverse_drag_and_alignment_stay_inside_original_bounds() {
        let r = Rect {
            l: 101,
            t: 55,
            r: 900,
            b: 700,
        }
        .aligned();
        assert_eq!((r.width(), r.height()), (798, 644));
        assert_eq!((r.l, r.t), (101, 55));
    }

    #[test]
    #[ignore = "briefly opens the native selector on the interactive desktop"]
    fn native_selector_accepts_drag_and_enter_and_cancels_with_escape() {
        let monitor = vnt_capture::list_monitors()
            .unwrap()
            .into_iter()
            .find(|m| m.primary)
            .unwrap();
        let result = pick_with_hook(|hwnd| {
            let hwnd = hwnd.0 as usize;
            std::thread::spawn(move || unsafe {
                // Match the selector's physical-pixel coordinates. Otherwise
                // Windows rescales posted mouse messages at e.g. 150% DPI.
                use windows::Win32::UI::HiDpi::{
                    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                };
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                std::thread::sleep(std::time::Duration::from_millis(200));
                let hwnd = HWND(hwnd as *mut _);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                assert!(GetMonitorInfoW(HMONITOR(monitor.hmonitor as *mut _), &mut info).as_bool());
                let x = info.rcMonitor.left - GetSystemMetrics(SM_XVIRTUALSCREEN) + 50;
                let y = info.rcMonitor.top - GetSystemMetrics(SM_YVIRTUALSCREEN) + 100;
                let point = |x: i32, y: i32| {
                    LPARAM(((y as u16 as usize) << 16 | x as u16 as usize) as isize)
                };
                PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(0), point(x, y)).unwrap();
                PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), point(x + 201, y + 151)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(100));
                PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), point(x + 201, y + 151)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(200));
                PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(13), LPARAM(0)).unwrap();
            });
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            (result.x, result.y, result.width, result.height),
            (50, 100, 200, 150)
        );
        let canceled = pick_with_hook(|hwnd| unsafe {
            PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(27), LPARAM(0)).unwrap();
        })
        .unwrap();
        assert!(canceled.is_none());
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;
    const SCREEN: Rect = Rect {
        l: 0,
        t: 0,
        r: 1920,
        b: 1080,
    };
    #[test]
    fn reverse_drag_clamps_to_one_display_and_aligns_inward() {
        let origin = Rect {
            l: 900,
            t: 700,
            r: 900,
            b: 700,
        };
        let r = drag_rect(SCREEN, (900, 700), origin, 0, (101, 55));
        assert_eq!(
            r,
            Rect {
                l: 101,
                t: 55,
                r: 899,
                b: 699
            }
        );
        let r = drag_rect(SCREEN, (900, 700), origin, 0, (4000, 2000));
        assert_eq!(
            r,
            Rect {
                l: 900,
                t: 700,
                r: 1920,
                b: 1080
            }
        );
    }
    #[test]
    fn move_preserves_size_and_resize_stays_inside_the_display() {
        let old = Rect {
            l: 100,
            t: 200,
            r: 740,
            b: 680,
        };
        let moved = drag_rect(SCREEN, (200, 300), old, 16, (-400, -400));
        assert_eq!(
            moved,
            Rect {
                l: 0,
                t: 0,
                r: 640,
                b: 480
            }
        );
        let resized = drag_rect(SCREEN, (740, 680), old, 10, (4000, 4000));
        assert_eq!(
            resized,
            Rect {
                l: 100,
                t: 200,
                r: 1920,
                b: 1080
            }
        );
    }
}
