use crate::*;
pub struct VtEncoder {
    codec: vnt_proto::Codec,
    encoder: vnt_macos::Encoder,
    params: EncodeParams,
    key: bool,
}
impl VtEncoder {
    pub fn new(codec: vnt_proto::Codec) -> Result<Self, EncodeError> {
        if codec == vnt_proto::Codec::Av1 {
            return Err(EncodeError::Unsupported);
        }
        let params = EncodeParams::new(1280, 720, 30, 4000);
        Ok(Self {
            codec,
            encoder: vnt_macos::Encoder::open(
                codec == vnt_proto::Codec::Hevc,
                params.width,
                params.height,
                params.fps,
                params.bitrate_kbps,
            )
            .map_err(EncodeError::Create)?,
            params,
            key: true,
        })
    }
}
impl VideoEncoder for VtEncoder {
    fn codec(&self) -> vnt_proto::Codec {
        self.codec
    }
    fn is_hardware(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "VideoToolbox"
    }
    fn configure(&mut self, params: &EncodeParams) -> Result<(), EncodeError> {
        params.validate()?;
        let encoder = vnt_macos::Encoder::open(
            self.codec == vnt_proto::Codec::Hevc,
            params.width,
            params.height,
            params.fps,
            params.bitrate_kbps,
        )
        .map_err(EncodeError::Reconfigure)?;
        self.encoder = encoder;
        self.params = *params;
        self.key = true;
        Ok(())
    }
    fn encode(
        &mut self,
        bgra: &[u8],
        pts_ms: u64,
        out: &mut Vec<EncodedPacket>,
    ) -> Result<(), EncodeError> {
        if bgra.len() != self.params.width as usize * self.params.height as usize * 4 {
            return Err(EncodeError::Encode("BGRA 尺寸不匹配".into()));
        }
        let mut data = vec![0; 8 * 1024 * 1024];
        let (n, keyframe) = self
            .encoder
            .frame(bgra, pts_ms, self.key, &mut data)
            .map_err(EncodeError::Encode)?;
        self.key = false;
        data.truncate(n);
        if n > 0 {
            out.push(EncodedPacket {
                keyframe,
                pts_ms,
                data,
            });
        }
        Ok(())
    }
    fn force_keyframe(&mut self) -> Result<(), EncodeError> {
        self.key = true;
        Ok(())
    }
}
