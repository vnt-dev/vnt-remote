//! HEVC Main 8-bit software encoder, linked statically from vendor/kvazaar.
//! Closed GOP, no reordering or frame overlap, Annex B with inline parameter sets.
//! Lambda rate control targets the average bitrate; individual frame sizes vary.

use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::RwLock;

use crate::{color, EncodeError, EncodeParams, EncodedPacket, VideoEncoder};

unsafe extern "C" {
    fn vnt_hevc_open(w: c_int, h: c_int, fps: c_int, kbps: c_int, gop: c_int) -> *mut c_void;
    fn vnt_hevc_close(ctx: *mut c_void);
    fn vnt_hevc_encode(
        ctx: *mut c_void,
        i420: *const u8,
        pts: i64,
        data: *mut *const u8,
        len: *mut u32,
        keyframe: *mut c_int,
    ) -> c_int;
}

// encoder_open rewrites Kvazaar's global SIMD dispatch tables. Keep all opens
// and closes exclusive with encoding. owf=0 ensures no worker still uses those
// tables after encode returns; independent sessions may encode concurrently.
static API_LOCK: RwLock<()> = RwLock::new(());

struct Handle(NonNull<c_void>);

// Each handle is owned exclusively by one encoder, with no thread-affine APIs.
unsafe impl Send for Handle {}

impl Handle {
    fn new(p: &EncodeParams) -> Result<Self, EncodeError> {
        let _guard = API_LOCK.write().unwrap_or_else(|e| e.into_inner());
        // SAFETY: parameters were validated; the bridge returns an owned handle.
        let handle = unsafe {
            vnt_hevc_open(
                p.width as c_int,
                p.height as c_int,
                p.fps as c_int,
                p.bitrate_kbps as c_int,
                p.gop.clamp(1, c_int::MAX as u32) as c_int,
            )
        };
        NonNull::new(handle)
            .map(Self)
            .ok_or_else(|| EncodeError::Create("kvazaar HEVC 初始化失败".into()))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _guard = API_LOCK.write().unwrap_or_else(|e| e.into_inner());
        // SAFETY: this uniquely owned handle was returned by vnt_hevc_open.
        unsafe { vnt_hevc_close(self.0.as_ptr()) };
    }
}

pub struct KvazaarEncoder {
    handle: Option<Handle>,
    params: Option<EncodeParams>,
    i420: Vec<u8>,
    force_idr: bool,
}

impl KvazaarEncoder {
    pub fn new() -> Result<Self, EncodeError> {
        // Exercise the real initialization path before advertising HEVC support.
        let p = EncodeParams::new(64, 64, 30, 1000);
        drop(Handle::new(&p)?);
        Ok(Self {
            handle: None,
            params: None,
            i420: Vec::new(),
            force_idr: false,
        })
    }
}

impl VideoEncoder for KvazaarEncoder {
    fn codec(&self) -> vnt_proto::Codec {
        vnt_proto::Codec::Hevc
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        "kvazaar"
    }

    fn configure(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
        p.validate()?;
        if self.params != Some(*p) {
            let handle = Handle::new(p)?;
            self.handle = Some(handle);
            log::info!(
                "kvazaar 使用 lambda 平均码率控制，目标 {} kbps",
                p.bitrate_kbps
            );
            self.params = Some(*p);
            self.i420
                .resize(p.width as usize * p.height as usize * 3 / 2, 0);
            self.force_idr = false; // A fresh encoder starts with an IDR.
        }
        Ok(())
    }

    fn encode(
        &mut self,
        bgra: &[u8],
        pts_ms: u64,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<(), EncodeError> {
        let p = self
            .params
            .ok_or_else(|| EncodeError::Encode("未 configure".into()))?;
        let (w, h) = (p.width as usize, p.height as usize);
        if bgra.len() != w * h * 4 || pts_ms > i64::MAX as u64 {
            return Err(EncodeError::Encode("BGRA 尺寸或时间戳不符".into()));
        }
        if self.force_idr {
            // The public API has no per-picture IDR flag. Reopen to break the
            // reference chain and repeat VPS/SPS/PPS for packet-loss recovery.
            let handle = Handle::new(&p)?;
            self.handle = Some(handle);
            self.force_idr = false;
        }
        color::bgra_to_i420(bgra, w, h, &mut self.i420);
        let handle = self
            .handle
            .as_mut()
            .ok_or_else(|| EncodeError::Encode("编码器未初始化".into()))?;
        let _guard = API_LOCK.read().unwrap_or_else(|e| e.into_inner());
        let mut data = std::ptr::null();
        let mut len = 0;
        let mut keyframe = 0;
        // SAFETY: input is a complete I420 frame; output pointers are writable.
        // The returned buffer belongs to the handle and is copied before reuse.
        let ok = unsafe {
            vnt_hevc_encode(
                handle.0.as_ptr(),
                self.i420.as_ptr(),
                pts_ms as i64,
                &mut data,
                &mut len,
                &mut keyframe,
            )
        };
        if ok == 0 || data.is_null() || len == 0 {
            return Err(EncodeError::Encode("kvazaar 未输出 HEVC 帧".into()));
        }
        // SAFETY: the bridge returns len initialized bytes on success.
        let data = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
        out.push(EncodedPacket {
            keyframe: keyframe != 0,
            pts_ms,
            data,
        });
        Ok(())
    }

    fn force_keyframe(&mut self) -> Result<(), EncodeError> {
        self.force_idr = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal_types(data: &[u8]) -> Vec<u8> {
        data.windows(4)
            .filter(|bytes| bytes[..3] == [0, 0, 1])
            .map(|bytes| (bytes[3] >> 1) & 0x3f)
            .collect()
    }

    fn frame(enc: &mut dyn VideoEncoder, w: usize, h: usize, pts: u64) -> EncodedPacket {
        let mut packets = Vec::new();
        enc.encode(&vec![80; w * h * 4], pts, &mut packets).unwrap();
        assert_eq!(
            packets.len(),
            1,
            "low delay must produce one frame immediately"
        );
        let packet = packets.pop().unwrap();
        assert_eq!(packet.pts_ms, pts);
        let types = nal_types(&packet.data);
        assert!(
            types.iter().any(|&t| t <= 31),
            "missing HEVC picture: {types:?}"
        );
        if packet.keyframe {
            for nal in [32, 33, 34] {
                assert!(
                    types.contains(&nal),
                    "IDR missing parameter set {nal}: {types:?}"
                );
            }
            assert!(
                types.contains(&19) || types.contains(&20),
                "missing IDR: {types:?}"
            );
        } else {
            assert!(!types.contains(&19) && !types.contains(&20));
        }
        packet
    }

    #[test]
    fn software_hevc_is_advertised_negotiated_and_created() {
        let caps: Vec<_> = crate::probe_caps()
            .into_iter()
            .filter(|c| !c.hardware)
            .collect();
        let cap = vnt_proto::negotiate::negotiate(&caps, &[vnt_proto::Codec::Hevc]).unwrap();
        assert_eq!(cap.name, "kvazaar");
        let mut enc = crate::create_encoder(cap.codec, false).unwrap();
        assert_eq!(enc.codec(), vnt_proto::Codec::Hevc);
        assert!(!enc.is_hardware());
        enc.configure(&EncodeParams::new(128, 128, 30, 2000))
            .unwrap();
        assert!(frame(enc.as_mut(), 128, 128, 0).keyframe);
    }

    #[test]
    fn closed_gop_and_forced_idr_repeat_parameter_sets_without_delay() {
        let mut enc = KvazaarEncoder::new().unwrap();
        let mut p = EncodeParams::new(128, 128, 30, 2000);
        p.gop = 4;
        enc.configure(&p).unwrap();
        for i in 0..10 {
            assert_eq!(frame(&mut enc, 128, 128, i * 33).keyframe, i % 4 == 0);
        }
        for i in 10..13 {
            enc.force_keyframe().unwrap();
            assert!(frame(&mut enc, 128, 128, i * 33).keyframe);
        }
    }

    #[test]
    fn reconfiguration_restarts_with_idr_and_invalid_input_preserves_encoder() {
        let mut enc = KvazaarEncoder::new().unwrap();
        assert!(enc.encode(&[0; 4], 0, &mut Vec::new()).is_err());
        for p in [
            EncodeParams::new(128, 128, 30, 2000),
            EncodeParams::new(128, 128, 30, 1000),
            EncodeParams::new(128, 128, 24, 1000),
            EncodeParams {
                gop: 2,
                ..EncodeParams::new(128, 128, 24, 1000)
            },
            EncodeParams::new(130, 74, 30, 1000), // even, but not CTU-aligned
        ] {
            enc.configure(&p).unwrap();
            assert!(frame(&mut enc, p.width as usize, p.height as usize, 0).keyframe);
            assert!(enc
                .configure(&EncodeParams::new(129, 74, 30, 1000))
                .is_err());
            let mut packets = Vec::new();
            assert!(enc.encode(&[0; 4], 33, &mut packets).is_err());
            assert!(packets.is_empty());
            assert!(!frame(&mut enc, p.width as usize, p.height as usize, 66).keyframe);
        }
    }

    #[test]
    fn simultaneous_sessions_can_encode_reopen_and_close_independently() {
        let threads: Vec<_> = (0..3)
            .map(|_| {
                std::thread::spawn(|| {
                    let mut enc = KvazaarEncoder::new().unwrap();
                    for round in 0..3 {
                        let w = 128 + round * 16;
                        enc.configure(&EncodeParams::new(w, 128, 30, 2000)).unwrap();
                        assert!(frame(&mut enc, w as usize, 128, 0).keyframe);
                        assert!(!frame(&mut enc, w as usize, 128, 33).keyframe);
                        enc.force_keyframe().unwrap();
                        assert!(frame(&mut enc, w as usize, 128, 66).keyframe);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}
