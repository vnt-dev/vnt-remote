//! SVT-AV1 纯软件 AV1 编码器（无 MF 硬件 AV1 时的兜底，Windows）。
//!
//! - 低延迟：`LOW_DELAY_B` + 关闭 look-ahead/场景切换，逐帧 1 进 1 出
//!   （`svt_av1_enc_get_packet` 在低延迟模式下阻塞到本帧编码完成）。
//! - CBR 码控，GOP 由 `intra_period_length` 控制；`force_keyframe` 通过
//!   输入头 `pic_type = EB_AV1_KEY_PICTURE` 逐帧请求（v2.x 低延迟支持）。
//!   所绑定的 2.3.0 在 LOW_DELAY_B 下明确拒绝 VBR，因此保留 CBR。
//! - 输出为 OBU 低开销码流（带 obu_size），关键帧携带序列头内联，
//!   与 WebCodecs（无 description）的 AV1 输入格式一致。

use crate::color;
use crate::{EncodeError, EncodeParams, EncodedPacket, VideoEncoder};

use crate::svt_av1_ffi::*;

/// 实时档位：分辨率越大越偏速度。SVT M9→M12，M12 最快。
fn preset_for(width: u32, height: u32) -> i8 {
    let pixels = width as u64 * height as u64;
    if pixels <= 1280 * 720 {
        10
    } else if pixels <= 1920 * 1080 {
        11
    } else {
        12
    }
}

unsafe impl Send for SvtAv1Encoder {}

pub struct SvtAv1Encoder {
    handle: *mut EbComponentType,
    params: Option<EncodeParams>,
    /// BGRA→I420 直转目标（复用）。
    i420: Vec<u8>,
    input: Box<EbSvtIOFormat>,
    header: Box<EbBufferHeaderType>,
    force_idr: bool,
}

/// SVT 的 rtcd 全局初始化非线程安全：首个 init 与后续 init 并发会
/// 触发库内 assert。所有句柄创建（init_handle→set_parameter→init）
/// 全程持锁串行化；编码/收包本身线程安全，无需持锁。
static INIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn check(code: EbErrorType, what: &str) -> Result<(), EncodeError> {
    if code == EbErrorType::EB_ErrorNone {
        Ok(())
    } else {
        Err(EncodeError::Create(format!("svt-av1 {what}: {code:?}")))
    }
}

impl SvtAv1Encoder {
    /// 创建（编码器本体延迟到 configure 按参数构建，这里只验证库可用）。
    pub fn new() -> Result<Self, EncodeError> {
        // EbBitDepth 等枚举无 0 值（Rust 2024 禁止零初始化这类类型），
        // 逐字段给显式初值
        let input = EbSvtIOFormat {
            luma: std::ptr::null_mut(),
            cb: std::ptr::null_mut(),
            cr: std::ptr::null_mut(),
            y_stride: 0,
            cr_stride: 0,
            cb_stride: 0,
            width: 0,
            height: 0,
            org_x: 0,
            org_y: 0,
            color_fmt: EbColorFormat::EB_YUV420,
            bit_depth: EbBitDepth::EB_EIGHT_BIT,
        };
        let header = EbBufferHeaderType {
            size: 0,
            p_buffer: std::ptr::null_mut(),
            n_filled_len: 0,
            n_alloc_len: 0,
            p_app_private: std::ptr::null_mut(),
            wrapper_ptr: std::ptr::null_mut(),
            n_tick_count: 0,
            dts: 0,
            pts: 0,
            qp: 0,
            pic_type: EbAv1PictureType::EB_AV1_INTER_PICTURE,
            luma_sse: 0,
            cr_sse: 0,
            cb_sse: 0,
            flags: 0,
            luma_ssim: 0.0,
            cr_ssim: 0.0,
            cb_ssim: 0.0,
            metadata: std::ptr::null_mut(),
        };
        Ok(Self {
            handle: std::ptr::null_mut(),
            params: None,
            i420: Vec::new(),
            input: Box::new(input),
            header: Box::new(header),
            force_idr: false,
        })
    }

    fn destroy(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                let _ = svt_av1_enc_deinit(self.handle);
                let _ = svt_av1_enc_deinit_handle(self.handle);
            }
            self.handle = std::ptr::null_mut();
        }
    }

    fn build(p: &EncodeParams) -> Result<*mut EbComponentType, EncodeError> {
        let _guard = INIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            // init_handle 首步即 svt_av1_set_default_params 整体覆写 config，
            // 零值不会被按枚举语义读取（MaybeUninit 绕过 Rust 2024 对
            // invalid-zero 枚举类型的 zeroed 运行时检查）
            let mut config: EbSvtAv1EncConfiguration =
                std::mem::MaybeUninit::zeroed().assume_init();
            let mut handle: *mut EbComponentType = std::ptr::null_mut();
            check(
                svt_av1_enc_init_handle(&mut handle, std::ptr::null_mut(), &mut config),
                "init_handle",
            )?;
            if handle.is_null() {
                return Err(EncodeError::Create("svt-av1 init_handle 返回空句柄".into()));
            }

            config.enc_mode = preset_for(p.width, p.height);
            // GOP：SVT 的 intra period 语义是两个 keyframe 的间隔-1
            config.intra_period_length = p.gop as i32 - 1;
            config.intra_refresh_type = SvtAv1IntraRefreshType::SVT_AV1_KF_REFRESH; // IDR 闭合 GOP
            config.pred_structure = SvtAv1PredStructure::SVT_AV1_PRED_LOW_DELAY_B as u8;
            config.hierarchical_levels = 4;
            config.source_width = p.width;
            config.source_height = p.height;
            config.frame_rate_numerator = p.fps;
            config.frame_rate_denominator = 1;
            config.encoder_bit_depth = 8;
            config.encoder_color_format = EbColorFormat::EB_YUV420;
            // Pinned SVT 2.3.0 rejects VBR + LOW_DELAY_B in enc_settings.c.
            // Random-access VBR also breaks the one-input/one-output contract.
            config.rate_control_mode = SvtAv1RcMode::SVT_AV1_RC_MODE_CBR as u32;
            config.target_bit_rate = p.bitrate_kbps * 1000;
            // 低延迟与稳定关键帧节奏由外部网关控制，编码器不做自主决策
            config.scene_change_detection = 0;
            config.look_ahead_distance = 0;
            // 线程数：物理核数的一半（桌面远控不能吃满 CPU），至少 2
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            config.logical_processors = (cores / 2).clamp(2, 8) as u32;

            let init_result = svt_av1_enc_set_parameter(handle, &mut config);
            if init_result != EbErrorType::EB_ErrorNone {
                let _ = svt_av1_enc_deinit_handle(handle);
                return Err(EncodeError::Create(format!(
                    "svt-av1 set_parameter: {init_result:?}"
                )));
            }
            let init_result = svt_av1_enc_init(handle);
            if init_result != EbErrorType::EB_ErrorNone {
                let _ = svt_av1_enc_deinit(handle);
                let _ = svt_av1_enc_deinit_handle(handle);
                return Err(EncodeError::Create(format!(
                    "svt-av1 init: {init_result:?}"
                )));
            }
            Ok(handle)
        }
    }

    /// 低延迟模式下 `svt_av1_enc_get_packet` 是阻塞调用且逐帧 1:1：
    /// 每帧 send 后恰好取一次，不能循环取（第二次调用会阻塞等下一帧）。
    fn receive_one(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        unsafe {
            let mut packet: *mut EbBufferHeaderType = std::ptr::null_mut();
            let code = svt_av1_enc_get_packet(self.handle, &mut packet, 0);
            if code == EbErrorType::EB_NoErrorEmptyQueue {
                return Ok(()); // 理论上低延迟不会走到；容忍为空
            }
            if code == EbErrorType::EB_NoErrorFifoShutdown {
                return Err(EncodeError::Encode("svt-av1 编码线程已退出".into()));
            }
            if code != EbErrorType::EB_ErrorNone {
                return Err(EncodeError::Encode(format!("svt-av1 get_packet: {code:?}")));
            }
            if packet.is_null() {
                return Ok(());
            }
            let header = &*packet;
            let len = header.n_filled_len as usize;
            if len > 0 && !header.p_buffer.is_null() {
                let data = std::slice::from_raw_parts(header.p_buffer, len).to_vec();
                let keyframe = header.pic_type == EbAv1PictureType::EB_AV1_KEY_PICTURE;
                let pts_ms = (header.pts.max(0) as u64) / 1_000; // 输入侧用 μs，这里换算回 ms
                out.push(EncodedPacket {
                    keyframe,
                    pts_ms,
                    data,
                });
            }
            let eos = header.flags & EB_BUFFERFLAG_EOS != 0;
            svt_av1_enc_release_out_buffer(&mut { packet });
            if eos {
                return Err(EncodeError::Encode("svt-av1 提前收到 EOS".into()));
            }
            Ok(())
        }
    }
}

impl VideoEncoder for SvtAv1Encoder {
    fn codec(&self) -> vnt_proto::Codec {
        vnt_proto::Codec::Av1
    }

    fn is_hardware(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        "svt-av1"
    }

    fn configure(&mut self, p: &EncodeParams) -> Result<(), EncodeError> {
        p.validate()?;
        if p.width < 64 || p.height < 64 {
            // SVT-AV1 的硬性下限；真实显示器不会小于它
            return Err(EncodeError::Reconfigure(
                "svt-av1 要求宽高均不小于 64".into(),
            ));
        }
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
        if rebuild {
            // 先在新句柄上完成全部协商，成功后再销毁旧实例（失败保持可用）
            let handle = Self::build(p)?;
            self.destroy();
            self.handle = handle;
            log::info!(
                "svt-av1 使用低延迟 CBR，目标 {} kbps（2.3.0 低延迟模式不支持 VBR）",
                p.bitrate_kbps
            );
            // 重建即断参考链：下一帧必须 IDR
            self.force_idr = true;
        }
        self.i420
            .resize(p.width as usize * p.height as usize * 3 / 2, 0);
        let (w, h) = (p.width as usize, p.height as usize);
        let chroma_stride = w / 2;
        unsafe {
            self.input.luma = self.i420.as_mut_ptr();
            self.input.cb = self.i420.as_mut_ptr().add(w * h);
            self.input.cr = self.i420.as_mut_ptr().add(w * h + (h / 2) * chroma_stride);
            self.input.y_stride = w as u32;
            self.input.cb_stride = chroma_stride as u32;
            self.input.cr_stride = chroma_stride as u32;
            self.input.width = p.width;
            self.input.height = p.height;
            self.input.color_fmt = EbColorFormat::EB_YUV420;
            self.input.bit_depth = EbBitDepth::EB_EIGHT_BIT;

            *self.header = EbBufferHeaderType {
                size: std::mem::size_of::<EbBufferHeaderType>() as u32,
                p_buffer: self.input.as_mut() as *mut EbSvtIOFormat as *mut u8,
                // SVT 用 n_filled_len 校验 YUV 数据量（w*h*3/2），与 p_buffer
                // 指向的描述结构体本身无关
                n_alloc_len: (w * h * 3 / 2) as u32,
                n_filled_len: 0,
                p_app_private: std::ptr::null_mut(),
                wrapper_ptr: std::ptr::null_mut(),
                n_tick_count: 0,
                dts: 0,
                pts: 0,
                qp: 0,
                pic_type: EbAv1PictureType::EB_AV1_INTER_PICTURE,
                luma_sse: 0,
                cr_sse: 0,
                cb_sse: 0,
                flags: 0,
                luma_ssim: 0.0,
                cr_ssim: 0.0,
                cb_ssim: 0.0,
                metadata: std::ptr::null_mut(),
            };
        }
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
        if self.handle.is_null() {
            return Err(EncodeError::Encode("编码器未初始化".into()));
        }
        let (w, h) = (p.width as usize, p.height as usize);
        if bgra.len() != w * h * 4 {
            return Err(EncodeError::Encode("BGRA 尺寸不符".into()));
        }
        color::bgra_to_i420(bgra, w, h, &mut self.i420);

        self.header.n_filled_len = self.header.n_alloc_len;
        self.header.pts = (pts_ms * 1_000) as i64; // μs，输出侧换算回 ms
        self.header.dts = self.header.pts;
        self.header.flags = 0;
        self.header.pic_type = if self.force_idr {
            EbAv1PictureType::EB_AV1_KEY_PICTURE
        } else {
            EbAv1PictureType::EB_AV1_INTER_PICTURE
        };
        self.force_idr = false;

        unsafe {
            check(
                svt_av1_enc_send_picture(self.handle, &mut *self.header),
                "send_picture",
            )
            .map_err(|e| EncodeError::Encode(e.to_string()))?;
        }
        self.receive_one(out)
    }

    fn force_keyframe(&mut self) -> Result<(), EncodeError> {
        self.force_idr = true;
        Ok(())
    }
}

impl Drop for SvtAv1Encoder {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_reconfiguration_preserves_a_working_encoder() {
        let mut encoder = SvtAv1Encoder::new().unwrap();
        encoder
            .configure(&EncodeParams::new(128, 128, 30, 4000))
            .unwrap();
        for bad in [
            EncodeParams::new(0, 128, 30, 4000),
            EncodeParams::new(63, 128, 30, 4000),
            EncodeParams::new(128, 128, 0, 4000),
            EncodeParams::new(128, 128, 30, u32::MAX),
        ] {
            assert!(encoder.configure(&bad).is_err());
        }
        let mut output = Vec::new();
        encoder
            .encode(&vec![80; 128 * 128 * 4], 0, &mut output)
            .unwrap();
        assert!(output.iter().any(|p| !p.data.is_empty()));
    }

    #[test]
    fn first_frame_is_a_keyframe_and_gop_emits_more() {
        // SVT 最小尺寸是 64x64
        let (w, h) = (128u32, 128u32);
        let mut enc = SvtAv1Encoder::new().unwrap();
        let mut params = EncodeParams::new(w, h, 30, 2000);
        params.gop = 4;
        enc.configure(&params).unwrap();
        let frame = vec![90u8; w as usize * h as usize * 4];
        let mut keyframes = 0;
        let mut total = 0;
        for i in 0..24u64 {
            let mut pkts = Vec::new();
            enc.encode(&frame, i * 33, &mut pkts).unwrap();
            for p in &pkts {
                total += 1;
                if p.keyframe {
                    keyframes += 1;
                }
            }
        }
        assert!(total >= 20, "输出帧过少: {total}");
        assert!(
            keyframes >= 4,
            "GOP=4 编码 24 帧应至少 4 个关键帧，实际 {keyframes}"
        );
    }

    /// 回归：force_keyframe 必须真的产生关键帧（丢包恢复依赖它）。
    #[test]
    fn forced_keyframes_are_emitted() {
        let (w, h) = (128u32, 128u32);
        let mut enc = SvtAv1Encoder::new().unwrap();
        let mut params = EncodeParams::new(w, h, 30, 2000);
        params.gop = 1000; // 长到自然 GOP 不触发
        enc.configure(&params).unwrap();
        let frame = vec![90u8; w as usize * h as usize * 4];
        let mut saw_natural = 0;
        for i in 0..8u64 {
            let mut pkts = Vec::new();
            enc.encode(&frame, i * 33, &mut pkts).unwrap();
            saw_natural += pkts.iter().filter(|p| p.keyframe).count();
        }
        assert_eq!(saw_natural, 1, "首帧 IDR 之后长 GOP 不应再有自然关键帧");
        for round in 0..3 {
            enc.force_keyframe().unwrap();
            let mut pkts = Vec::new();
            enc.encode(&frame, 100_000 + round, &mut pkts).unwrap();
            assert!(
                pkts.iter().any(|p| p.keyframe),
                "第 {round} 轮强制关键帧未生效"
            );
        }
    }

    /// 输出必须是 WebCodecs 可直接解的 OBU 低开销码流：关键帧包含
    /// 序列头 OBU（type=1）与帧 OBU（type=6）。
    #[test]
    fn keyframe_stream_contains_sequence_header_obu() {
        let (w, h) = (128u32, 128u32);
        let mut enc = SvtAv1Encoder::new().unwrap();
        enc.configure(&EncodeParams::new(w, h, 30, 2000)).unwrap();
        let frame = vec![90u8; w as usize * h as usize * 4];
        let mut pkts = Vec::new();
        enc.encode(&frame, 0, &mut pkts).unwrap();
        let key = pkts.iter().find(|p| p.keyframe).expect("首帧关键帧");
        let mut obu_types = Vec::new();
        let data = &key.data;
        let mut i = 0usize;
        while i < data.len() {
            let b = data[i];
            let obu_type = (b >> 3) & 0xF;
            let has_ext = (b & 0x4) != 0;
            let has_size = (b & 0x2) != 0;
            let mut j = i + 1 + usize::from(has_ext);
            if !has_size {
                // 无 obu_size 的裸 OBU（TD 常见）：固定 1 字节，无负载
                obu_types.push(obu_type);
                i = j;
                continue;
            }
            // leb128 读 obu_size
            let mut size: u64 = 0;
            let mut shift = 0;
            loop {
                if j >= data.len() {
                    return;
                }
                let byte = data[j];
                size |= ((byte & 0x7F) as u64) << shift;
                j += 1;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            obu_types.push(obu_type);
            i = j + size as usize;
        }
        assert!(
            obu_types.contains(&1),
            "关键帧缺序列头 OBU（types={obu_types:?}）"
        );
        assert!(obu_types.contains(&6), "缺帧 OBU（types={obu_types:?}）");
    }
}
