//! 编码能力探测 + 小规模编码自测。
//! 用法: codec-probe [任意参数则编码 60 帧做实测] [h264|hevc|av1] [WxH] [soft]
//! （soft = 纯软件路径，对应被控端关闭硬件编码）
//! 设 CODEC_PROBE_DUMP=path 时把首个关键帧裸流导出到该文件。
//! 设 CODEC_PROBE_DUMP_STREAM=path 时导出全部帧裸流，供外部解码校验。

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    println!("=== vnt-codec 探测 ===");

    #[cfg(windows)]
    for name in diag::all_hardware_encoder_names() {
        println!("  硬件 MFT: {name}");
    }

    for cap in vnt_codec::probe_caps() {
        println!(
            "  {:?}\t{}\t{}",
            cap.codec,
            if cap.hardware { "硬件" } else { "软件" },
            cap.name
        );
    }

    if std::env::args().len() > 1 {
        let codec = match std::env::args().nth(2).as_deref() {
            Some("hevc") => vnt_proto::Codec::Hevc,
            Some("av1") => vnt_proto::Codec::Av1,
            _ => vnt_proto::Codec::H264,
        };
        let (mut w, mut h) = (1280u32, 720u32);
        if let Some(dims) = std::env::args().nth(3) {
            if let Some((pw, ph)) = dims.split_once('x') {
                if let (Ok(vw), Ok(vh)) = (pw.parse(), ph.parse()) {
                    (w, h) = (vw, vh);
                }
            }
        }
        println!("\n=== 编码自测 ({w}x{h}@30, 60 帧, {codec:?}) ===");
        let mut frame = vec![0u8; w as usize * h as usize * 4];
        for (i, b) in frame.iter_mut().enumerate() {
            *b = ((i / 4 + i / w as usize / 4) % 256) as u8;
        }
        // 附带 "soft" 参数时测纯软件路径（对应被控端关闭硬件编码）
        let hardware = !std::env::args().any(|a| a == "soft");
        match vnt_codec::create_encoder(codec, hardware) {
            Ok(mut enc) => {
                println!("使用: {} (硬件={})", enc.name(), enc.is_hardware());
                let params = vnt_codec::EncodeParams::new(w, h, 30, 8000);
                if let Err(e) = enc.configure(&params) {
                    println!("configure 失败: {e}");
                    return;
                }
                let t0 = std::time::Instant::now();
                let mut all = Vec::new();
                for i in 0..60u64 {
                    let mut pkts = Vec::new();
                    match enc.encode(&frame, i * 33, &mut pkts) {
                        Ok(()) => all.extend(pkts),
                        Err(e) => {
                            println!("第 {i} 帧失败: {e}");
                            return;
                        }
                    }
                }
                let secs = t0.elapsed().as_secs_f64();
                let bytes: usize = all.iter().map(|p| p.data.len()).sum();
                let keyframes = all.iter().filter(|p| p.keyframe).count();
                println!(
                    "60 帧完成: {secs:.2}s => {:.1} fps, 输出 {} KB, 关键帧 {keyframes}",
                    60.0 / secs,
                    bytes / 1024
                );
                if let Ok(path) = std::env::var("CODEC_PROBE_DUMP_STREAM") {
                    let stream: Vec<u8> = all.iter().flat_map(|p| p.data.iter().copied()).collect();
                    match std::fs::write(&path, stream) {
                        Ok(()) => println!("完整裸流已导出: {path}"),
                        Err(e) => println!("导出失败: {e}"),
                    }
                }
                if let Some(first_key) = all.iter().find(|p| p.keyframe && !p.data.is_empty()) {
                    println!("首关键帧结构: {:?}", parse_stream(first_key));
                    if let Ok(path) = std::env::var("CODEC_PROBE_DUMP") {
                        match std::fs::write(&path, &first_key.data) {
                            Ok(()) => println!("关键帧已导出: {path}"),
                            Err(e) => println!("导出失败: {e}"),
                        }
                    }
                }
            }
            Err(e) => println!("创建编码器失败: {e}"),
        }
    }
}

/// 概览首关键帧的裸流结构（H264/HEVC NAL 或 AV1 OBU）。
fn parse_stream(p: &vnt_codec::EncodedPacket) -> Vec<String> {
    let data = &p.data;
    if data.len() >= 4 && (data[..4] == [0, 0, 0, 1] || data[1..4] == [0, 0, 1]) {
        // Annex B：逐起始码切分
        let mut starts = Vec::new();
        let mut i = 0usize;
        while i + 3 <= data.len() {
            if data[i..i + 3] == [0, 0, 1] {
                starts.push(if i > 0 && data[i - 1] == 0 { i - 1 } else { i });
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut out = Vec::new();
        for (idx, &s) in starts.iter().enumerate() {
            let hdr = s + 3 + usize::from(data[s..].starts_with(&[0, 0, 0, 1]));
            if hdr >= data.len() {
                break;
            }
            let end = starts.get(idx + 1).copied().unwrap_or(data.len());
            let b = data[hdr];
            out.push(format!(
                "h{}|v{}({}B)",
                b & 0x1f,
                (b >> 1) & 0x3f,
                end - hdr
            ));
        }
        return out;
    }
    // AV1 OBU 低开销格式
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < data.len() && out.len() < 8 {
        let b = data[i];
        let obu_type = (b >> 3) & 0xF;
        let has_ext = (b & 0x4) != 0;
        let has_size = (b & 0x2) != 0;
        let mut j = i + 1 + usize::from(has_ext);
        if !has_size {
            out.push(format!("obu{obu_type}(1B)"));
            i = j;
            continue;
        }
        let mut size: u64 = 0;
        let mut shift = 0;
        let mut ok = false;
        while j < data.len() {
            let byte = data[j];
            size |= u64::from(byte & 0x7F) << shift;
            j += 1;
            shift += 7;
            if byte & 0x80 == 0 {
                ok = true;
                break;
            }
        }
        if !ok {
            break;
        }
        out.push(format!("obu{obu_type}({size}B)"));
        i = j + size as usize;
    }
    out
}

#[cfg(windows)]
mod diag {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
    use windows_core::GUID;

    pub fn all_hardware_encoder_names() -> Vec<String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
        }
        let mut out = Vec::new();
        unsafe {
            let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            if MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_ASYNCMFT,
                None,
                None,
                &mut arr,
                &mut count,
            )
            .is_ok()
                && !arr.is_null()
            {
                for i in 0..count as usize {
                    if let Some(a) = (*arr.add(i)).take() {
                        out.push(attr_string(&a, &MFT_FRIENDLY_NAME_Attribute));
                    }
                }
                CoTaskMemFree(Some(arr as *const core::ffi::c_void));
            }
        }
        out
    }

    fn attr_string(attrs: &IMFAttributes, key: &GUID) -> String {
        unsafe {
            let len = attrs.GetStringLength(key).unwrap_or(0) as usize;
            let mut buf = vec![0u16; len + 1];
            if attrs.GetString(key, &mut buf, None).is_err() {
                return "unknown".into();
            }
            let mut used = len;
            if used > 0 && buf[used - 1] == 0 {
                used -= 1;
            }
            String::from_utf16_lossy(&buf[..used])
        }
    }
}
