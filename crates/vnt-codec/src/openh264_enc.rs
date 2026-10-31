//! openh264 纯软件 H264 兜底编码器（无 MF 硬件/软件 MFT 可用时的最后手段）。
//! 使用 Quality 码控和平均目标码率，允许各帧输出大小随内容变化。

use crate::color;
use crate::{EncodeError, EncodeParams, EncodedPacket, VideoEncoder};

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

pub struct OpenH264Encoder {
    encoder: Option<Encoder>,
    params: Option<EncodeParams>,
    /// I420 转换目标（复用）：由 BGRA 直接转换
    i420: Vec<u8>,
    force_idr: bool,
}

impl OpenH264Encoder {
    pub fn new() -> Result<Self, EncodeError> {
        Ok(Self {
            encoder: None,
            params: None,
            i420: Vec::new(),
            force_idr: false,
        })
    }

    fn build(p: &EncodeParams) -> Result<Encoder, EncodeError> {
        let config = EncoderConfig::new()
            // Explicitly retain quality-oriented variable output rather than
            // depending on the wrapper's default rate-control mode.
            .rate_control_mode(RateControlMode::Quality)
            .bitrate(BitRate::from_bps(p.bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(p.fps as f32))
            // GOP 必须显式设置：否则默认仅首帧 IDR，软编错过关键帧即黑屏到底
            .intra_frame_period(IntraFramePeriod::from_num_frames(p.gop.max(1)));
        Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| EncodeError::Create(format!("openh264: {e}")))
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn codec(&self) -> vnt_proto::Codec {
        vnt_proto::Codec::H264
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        "openh264"
    }

    fn configure(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
        p.validate()?;
        // 当前封装用重建应用参数变化。OpenH264 底层 SetOption 支持运行时
        // 调参，此处尚未接入；不能只修改 params 而不更新实际编码器。
        let rebuild = self
            .params
            .map(|old| {
                old.width != p.width
                    || old.height != p.height
                    || old.fps != p.fps
                    || old.bitrate_kbps != p.bitrate_kbps
                    || old.gop != p.gop
            })
            .unwrap_or(true);
        if rebuild || self.encoder.is_none() {
            self.encoder = Some(Self::build(p)?);
            log::info!(
                "openh264 使用 Quality 码控，平均目标 {} kbps",
                p.bitrate_kbps
            );
            // 重建即断参考链：下一帧必须 IDR，否则客户端收到花屏
            self.force_idr = true;
        }
        self.i420
            .resize(p.width as usize * p.height as usize * 3 / 2, 0);
        self.params = Some(*p);
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
        let Some(enc) = &mut self.encoder else {
            return Err(EncodeError::Encode("编码器未初始化".into()));
        };
        // BGRA → I420 直转（复用缓冲），避免额外的整帧
        // 色度重排与一次逐帧分配
        let (w, h) = (p.width as usize, p.height as usize);
        if bgra.len() != w * h * 4 {
            return Err(EncodeError::Encode("BGRA 尺寸不符".into()));
        }
        color::bgra_to_i420(bgra, w, h, &mut self.i420);
        // 以切片视图喂给编码器（YUVSlices 借用，零拷贝）
        let y_size = w * h;
        let uv_size = (h / 2) * (w / 2);
        let (y_plane, uv) = self.i420[..y_size + uv_size * 2].split_at(y_size);
        let (u_plane, v_plane) = uv.split_at(uv_size);
        let yuv = YUVSlices::new((y_plane, u_plane, v_plane), (w, h), (w, w / 2, w / 2));

        if self.force_idr {
            enc.force_intra_frame();
            self.force_idr = false;
        }
        let packet = enc
            .encode(&yuv)
            .map_err(|e| EncodeError::Encode(format!("openh264 encode: {e}")))?;
        let keyframe = matches!(packet.frame_type(), FrameType::IDR | FrameType::I);
        let data = packet.to_vec();
        if data.is_empty() {
            return Ok(()); // Rate control may skip a frame; this is not a video packet.
        }
        out.push(EncodedPacket {
            keyframe,
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
    use crate::{EncodeParams, VideoEncoder};

    #[test]
    fn invalid_reconfiguration_preserves_a_working_encoder() {
        let mut encoder = OpenH264Encoder::new().unwrap();
        encoder
            .configure(&EncodeParams::new(64, 48, 30, 4000))
            .unwrap();
        for p in [
            EncodeParams::new(0, 48, 30, 4000),
            EncodeParams::new(63, 48, 30, 4000),
            EncodeParams::new(u32::MAX, u32::MAX, 30, 4000),
            EncodeParams::new(64, 48, 0, 4000),
            EncodeParams::new(64, 48, 30, u32::MAX),
        ] {
            assert!(encoder.configure(&p).is_err());
        }
        let mut output = Vec::new();
        encoder
            .encode(&vec![80; 64 * 48 * 4], 0, &mut output)
            .unwrap();
        assert!(output.iter().any(|p| !p.data.is_empty()));
    }

    #[test]
    fn changing_only_gop_reconfigures_the_encoder() {
        let mut encoder = OpenH264Encoder::new().unwrap();
        let mut params = EncodeParams::new(64, 48, 30, 4000);
        encoder.configure(&params).unwrap();
        let bgra = vec![80; 64 * 48 * 4];
        let mut output = Vec::new();
        encoder.encode(&bgra, 0, &mut output).unwrap();
        params.gop = 2;
        encoder.configure(&params).unwrap();
        let mut keyframes = 0;
        for i in 1..=8 {
            output.clear();
            encoder.encode(&bgra, i * 33, &mut output).unwrap();
            keyframes += output.iter().filter(|p| p.keyframe).count();
        }
        assert!(keyframes >= 4, "the new GOP must take effect: {keyframes}");
    }

    #[test]
    fn invalid_bgra_returns_an_error_and_encoder_remains_usable() {
        let mut encoder = OpenH264Encoder::new().unwrap();
        encoder
            .configure(&EncodeParams::new(64, 48, 30, 4000))
            .unwrap();
        let mut output = Vec::new();
        assert!(encoder.encode(&[0; 4], 0, &mut output).is_err());
        assert!(output.is_empty());
        encoder
            .encode(&vec![80; 64 * 48 * 4], 33, &mut output)
            .unwrap();
        assert!(!output.is_empty());
        assert!(output[0].keyframe);
    }

    #[test]
    fn adaptive_fps_changes_start_with_an_idr() {
        let mut encoder = OpenH264Encoder::new().unwrap();
        let frame = vec![80; 64 * 48 * 4];
        for fps in [60, 24, 30] {
            encoder
                .configure(&EncodeParams::new(64, 48, fps, 4000))
                .unwrap();
            let mut packets = Vec::new();
            encoder.encode(&frame, 0, &mut packets).unwrap();
            assert!(packets.first().is_some_and(|packet| packet.keyframe));
        }
    }

    /// 回归：GOP 未应用到 openh264 时，181 帧只有第 0 帧是关键帧，
    /// 软编错过首帧即永久黑屏。
    #[test]
    fn gop_is_applied() {
        let (w, h) = (320u32, 240u32);
        let mut enc = OpenH264Encoder::new().expect("创建 openh264");
        let params = EncodeParams::new(w, h, 30, 4000);
        assert_eq!(params.gop, 60);
        enc.configure(&params).expect("configure");

        let mut keyframes = 0;
        let mut total = 0;
        for i in 0..150u64 {
            // 内容逐帧变化，避免跳帧
            let mut frame = vec![0u8; w as usize * h as usize * 4];
            for (j, b) in frame.iter_mut().enumerate() {
                *b = ((j / 4 + i as usize * 7 + j / w as usize / 4) % 256) as u8;
            }
            let mut pkts = Vec::new();
            enc.encode(&frame, i * 33, &mut pkts).expect("编码");
            for p in &pkts {
                total += 1;
                if p.keyframe {
                    keyframes += 1;
                }
            }
        }
        assert!(total > 100, "输出帧过少: {total}");
        assert!(
            keyframes >= 2,
            "GOP=60 编码 150 帧应至少 2 个关键帧，实际 {keyframes}"
        );
    }
}
