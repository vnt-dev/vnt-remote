//! Shared bounds for decoded stream metadata and input events.

use crate::pb;

/// Permit wide monitor layouts and 8K video, but bound raw frame memory.
pub const MAX_VIDEO_DIMENSION: u32 = 16_384;
pub const MAX_VIDEO_PIXELS: u64 = 64 * 1024 * 1024;

/// 一次文件传输的最大条目数（目录已递归展开）。
pub const MAX_CLIPBOARD_ITEMS: usize = 4096;
/// 单个相对路径的字节上限（Windows 长路径上限的 1/8）。
pub const MAX_CLIPBOARD_PATH_BYTES: usize = 1024;
/// 单次传输总字节上限（1 TiB）：超出视为异常，避免 u64 溢出与假进度。
pub const MAX_CLIPBOARD_TOTAL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;

/// 文件清单中的相对路径是否合法。
///
/// 对端不可信：拒绝绝对路径、盘符、父目录跳转与 Windows 保留字符，
/// 防止粘贴目标越过清单声明的目录。
pub fn valid_rel_path(path: &str) -> bool {
    if path.is_empty() || path.len() > MAX_CLIPBOARD_PATH_BYTES || path.contains('\0') {
        return false;
    }
    // 统一按两种分隔符切分；路径里混用两者也合法（Windows 接受）。
    let mut components = 0usize;
    for component in path.split(['/', '\\']) {
        components += 1;
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.contains([':', '*', '?', '"', '<', '>', '|'])
            || component.ends_with(' ')
        {
            return false;
        }
    }
    components <= 64
}

pub fn valid_clipboard_text(text: &str) -> bool {
    text.len() <= crate::control::MAX_CLIPBOARD_TEXT
}

pub fn valid_link_feedback(report: &pb::LinkFeedback) -> bool {
    let frames = u64::from(report.received_frames) + u64::from(report.lost_frames);
    report.fec.len() <= 129
        && report.fec.iter().all(|f| {
            f.stream_id <= 128
                && f.expected <= 1_000_000
                && f.received <= f.expected
                && f.recovered <= f.expected - f.received
                && f.residual == f.expected - f.received - f.recovered
                && f.parity_bytes <= 64 * 1024 * 1024
                && f.ratio_permille <= 300
                && (!f.audio || (f.stream_id == 0 && f.generation != 0))
        })
        && report.sequence > 0
        && (500..=5000).contains(&report.interval_ms)
        && frames <= u64::from(report.interval_ms) * 120 / 1000 + 32
        && report.received_bytes <= 200_000 * u64::from(report.interval_ms) / 8 + 8 * 1024 * 1024
        && report.rtt_ms.is_none_or(|rtt| rtt <= 60_000)
        && report.frame_gap_ms.is_none_or(|gap| gap <= 60_000)
        && (report.received_frames != 0 || report.received_bytes == 0)
}

pub fn valid_video_size(width: u32, height: u32) -> bool {
    (1..=MAX_VIDEO_DIMENSION).contains(&width)
        && (1..=MAX_VIDEO_DIMENSION).contains(&height)
        && u64::from(width) * u64::from(height) <= MAX_VIDEO_PIXELS
}

pub fn valid_input(event: &pb::InputEvent) -> bool {
    match &event.ev {
        Some(pb::input_event::Ev::MouseMove(m)) => m.x.is_finite() && m.y.is_finite(),
        Some(pb::input_event::Ev::Wheel(w)) => w.dx.is_finite() && w.dy.is_finite(),
        Some(pb::input_event::Ev::Key(k)) => (1..=255).contains(&k.scancode),
        Some(pb::input_event::Ev::MouseButton(b)) => matches!(
            b.button(),
            pb::MouseButton::MouseLeft
                | pb::MouseButton::MouseRight
                | pb::MouseButton::MouseMiddle
                | pb::MouseButton::MouseX1
                | pb::MouseButton::MouseX2
        ),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_bounds_reject_invalid_counts_durations_and_untrusted_extremes() {
        let valid = pb::LinkFeedback {
            fec: Vec::new(),
            sequence: 1,
            interval_ms: 1000,
            received_frames: 30,
            lost_frames: 0,
            received_bytes: 1000,
            frame_gap_ms: Some(10),
            rtt_ms: Some(20),
        };
        assert!(valid_link_feedback(&valid));
        for bad in [
            pb::LinkFeedback {
                sequence: 0,
                ..valid.clone()
            },
            pb::LinkFeedback {
                interval_ms: 499,
                ..valid.clone()
            },
            pb::LinkFeedback {
                interval_ms: 5001,
                ..valid.clone()
            },
            pb::LinkFeedback {
                lost_frames: u32::MAX,
                ..valid.clone()
            },
            pb::LinkFeedback {
                received_bytes: u64::MAX,
                ..valid.clone()
            },
            pb::LinkFeedback {
                received_frames: 0,
                ..valid.clone()
            },
            pb::LinkFeedback {
                rtt_ms: Some(60_001),
                ..valid.clone()
            },
            pb::LinkFeedback {
                frame_gap_ms: Some(60_001),
                ..valid.clone()
            },
        ] {
            assert!(!valid_link_feedback(&bad));
        }
    }

    #[test]
    fn video_sizes_are_nonzero_and_bounded_without_integer_overflow() {
        assert!(valid_video_size(7680, 4320));
        for (w, h) in [(0, 480), (640, 0), (u32::MAX, u32::MAX), (16_384, 16_384)] {
            assert!(!valid_video_size(w, h));
        }
    }

    #[test]
    fn malformed_inputs_cannot_become_real_keys_or_nonfinite_mouse_events() {
        use pb::input_event::Ev;
        for ev in [
            Ev::Key(pb::KeyEvent {
                scancode: 65566,
                extended: false,
                down: true,
            }),
            Ev::MouseMove(pb::MouseMove {
                x: f32::NAN,
                y: 0.5,
            }),
            Ev::Wheel(pb::WheelEvent {
                dx: 0.0,
                dy: f32::INFINITY,
            }),
            Ev::MouseButton(pb::MouseButtonEvent {
                button: 99,
                down: true,
            }),
        ] {
            assert!(!valid_input(&pb::InputEvent { ev: Some(ev) }));
        }
        assert!(valid_input(&pb::InputEvent {
            ev: Some(Ev::Key(pb::KeyEvent {
                scancode: 30,
                extended: false,
                down: false,
            }))
        }));
    }

    #[test]
    fn relative_paths_reject_traversal_absolute_and_reserved_names() {
        for ok in [
            "a.txt",
            "dir/sub/b.txt",
            "dir\\sub\\b.txt",
            "..dots",
            "a..b",
            "CON.txt",
            "日本語/ファイル.pdf",
            &"x".repeat(MAX_CLIPBOARD_PATH_BYTES),
        ] {
            assert!(valid_rel_path(ok), "应接受: {ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "..\\..\\etc\\passwd",
            "dir/../../x",
            "/abs/path",
            "\\abs\\path",
            "C:\\Windows\\x",
            "C:/x",
            "\\\\server\\share\\x",
            "dir//x",
            "dir/",
            "dir/ ",
            "a\0b",
            "a*b",
            "a?b",
            &"x".repeat(MAX_CLIPBOARD_PATH_BYTES + 1),
        ] {
            assert!(!valid_rel_path(bad), "应拒绝: {bad:?}");
        }
        // 深层嵌套目录仍在分量数量上限内
        let deep = std::iter::repeat_n("d", 64).collect::<Vec<_>>().join("/");
        assert!(valid_rel_path(&deep));
        let deeper = std::iter::repeat_n("d", 65).collect::<Vec<_>>().join("/");
        assert!(!valid_rel_path(&deeper));
    }
}

/// Shell FILEDESCRIPTORW has 260 UTF-16 slots including the terminator.
pub fn valid_clipboardoffer(offer: &pb::ClipboardOffer) -> bool {
    if offer.offer_id == 0 {
        return offer.items.is_empty() && offer.total_bytes == 0 && offer.error.len() <= 1024;
    }
    if !offer.error.is_empty() || offer.items.is_empty() || offer.items.len() > MAX_CLIPBOARD_ITEMS
    {
        return false;
    }
    let mut paths = std::collections::HashMap::new();
    let mut total = 0u64;
    for item in &offer.items {
        if !valid_rel_path(&item.rel_path)
            || item.rel_path.chars().any(|c| c < ' ')
            || item.rel_path.encode_utf16().count() >= 260
            || item.rel_path.split(['/', '\\']).any(|part| {
                let base = part.split('.').next().unwrap_or("").to_uppercase();
                part.ends_with('.')
                    || matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                    || (base.len() == 4
                        && (base.starts_with("COM") || base.starts_with("LPT"))
                        && matches!(base.as_bytes()[3], b'1'..=b'9'))
            })
            || (item.directory && item.size != 0)
            || item.attributes & !(0x1 | 0x2 | 0x4 | 0x10 | 0x20 | 0x80) != 0
        {
            return false;
        }
        let key = item.rel_path.replace('\\', "/").to_lowercase();
        if paths.insert(key.clone(), item.directory).is_some() {
            return false;
        }
        let mut parent = key.as_str();
        while let Some((path, _)) = parent.rsplit_once('/') {
            if paths.get(path) != Some(&true) {
                return false;
            }
            parent = path;
        }
        total = match total.checked_add(item.size) {
            Some(n) if n <= MAX_CLIPBOARD_TOTAL_BYTES => n,
            _ => return false,
        };
    }
    total == offer.total_bytes
}
pub fn valid_clipboardrequest(r: &pb::ClipboardRequest) -> bool {
    use pb::clipboard_request::Action;
    r.offer_id != 0
        && r.operation_id != 0
        && r.reason.len() <= 1024
        && match Action::try_from(r.action) {
            Ok(Action::Start) => r.request_id != 0 && r.length == 0,
            Ok(Action::Read) => {
                r.request_id != 0
                    && r.length > 0
                    && r.length as usize <= crate::control::CLIPBOARD_CHUNK_BYTES
                    && r.offset.checked_add(r.length as u64).is_some()
            }
            Ok(_) => r.length == 0,
            Err(_) => false,
        }
}
pub fn valid_clipboardreply(r: &pb::ClipboardReply) -> bool {
    r.offer_id != 0
        && r.operation_id != 0
        && r.request_id != 0
        && r.data.len() <= crate::control::CLIPBOARD_CHUNK_BYTES
        && r.reason.len() <= 1024
        && (r.reason.is_empty() || r.data.is_empty())
}

#[cfg(test)]
mod native_tests {
    use super::*;
    fn item(path: &str, directory: bool) -> pb::NativeFileItem {
        pb::NativeFileItem {
            rel_path: path.into(),
            directory,
            ..Default::default()
        }
    }
    #[test]
    fn native_manifest_preserves_dirs_and_rejects_ambiguous_names() {
        let offer = pb::ClipboardOffer {
            offer_id: 1,
            items: vec![item("dir", true), item("dir/空文件", false)],
            total_bytes: 0,
            ..Default::default()
        };
        assert!(valid_clipboardoffer(&offer));
        assert!(valid_clipboardoffer(&pb::ClipboardOffer::default()));
        for items in [
            vec![item("dir/a", false)],
            vec![item("A", false), item("a", false)],
            vec![item("NUL.txt", false)],
            vec![item("dir.", true)],
            vec![item(&"a".repeat(260), false)],
            vec![item("../bad", false)],
        ] {
            assert!(!valid_clipboardoffer(&pb::ClipboardOffer {
                items,
                ..offer.clone()
            }));
        }
        let mut bad = offer.clone();
        bad.total_bytes = 1;
        assert!(!valid_clipboardoffer(&bad));
        bad = offer;
        bad.items[0].attributes = 0x400;
        assert!(!valid_clipboardoffer(&bad));
    }
    #[test]
    fn requests_and_replies_are_bounded_and_round_trip() {
        use crate::control::{decode_message, parse_header, ControlMessage};
        let offer = pb::ClipboardOffer {
            offer_id: 7,
            items: vec![item("a", false)],
            ..Default::default()
        };
        let read = pb::ClipboardRequest {
            offer_id: 7,
            operation_id: 8,
            request_id: 9,
            action: 1,
            length: 32768,
            ..Default::default()
        };
        let reply = pb::ClipboardReply {
            offer_id: 7,
            operation_id: 8,
            request_id: 9,
            data: vec![8; 32768],
            ..Default::default()
        };
        for msg in [
            ControlMessage::ClipboardOffer(offer),
            ControlMessage::ClipboardRequest(read.clone()),
            ControlMessage::ClipboardReply(reply.clone()),
        ] {
            let encoded = msg.encode_frame();
            let (ty, _) = parse_header(encoded[..5].try_into().unwrap()).unwrap();
            assert_eq!(
                decode_message(ty, &encoded[5..]).unwrap().encode_frame(),
                encoded
            );
        }
        assert!(!valid_clipboardrequest(&pb::ClipboardRequest {
            length: 32769,
            ..read.clone()
        }));
        assert!(!valid_clipboardrequest(&pb::ClipboardRequest {
            offset: u64::MAX,
            ..read
        }));
        assert!(!valid_clipboardreply(&pb::ClipboardReply {
            reason: "bad".into(),
            ..reply
        }));
    }
}
