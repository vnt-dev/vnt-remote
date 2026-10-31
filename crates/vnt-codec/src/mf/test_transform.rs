//! Deterministic COM MFT used to exercise backpressure without a GPU/driver.
#![allow(non_snake_case)]

use super::*;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::E_NOTIMPL;

#[derive(Default)]
struct Script {
    rejections: usize,
    inputs: Vec<i64>,
    pending: Option<i64>,
    polls: usize,
    input_announced: bool,
    output_announced: bool,
}

#[windows_core::implement(IMFTransform)]
struct Transform(Arc<Mutex<Script>>);

impl IMFTransform_Impl for Transform_Impl {
    fn GetStreamLimits(
        &self,
        _pdwinputminimum: *mut u32,
        _pdwinputmaximum: *mut u32,
        _pdwoutputminimum: *mut u32,
        _pdwoutputmaximum: *mut u32,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetStreamCount(
        &self,
        _pcinputstreams: *mut u32,
        _pcoutputstreams: *mut u32,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetStreamIDs(
        &self,
        _dwinputidarraysize: u32,
        _pdwinputids: *mut u32,
        _dwoutputidarraysize: u32,
        _pdwoutputids: *mut u32,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetInputStreamInfo(
        &self,
        _dwinputstreamid: u32,
        _pstreaminfo: *mut MFT_INPUT_STREAM_INFO,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetOutputStreamInfo(
        &self,
        _dwoutputstreamid: u32,
    ) -> windows_core::Result<MFT_OUTPUT_STREAM_INFO> {
        Err(E_NOTIMPL.into())
    }
    fn GetAttributes(&self) -> windows_core::Result<IMFAttributes> {
        Err(E_NOTIMPL.into())
    }
    fn GetInputStreamAttributes(
        &self,
        _dwinputstreamid: u32,
    ) -> windows_core::Result<IMFAttributes> {
        Err(E_NOTIMPL.into())
    }
    fn GetOutputStreamAttributes(
        &self,
        _dwoutputstreamid: u32,
    ) -> windows_core::Result<IMFAttributes> {
        Err(E_NOTIMPL.into())
    }
    fn DeleteInputStream(&self, _dwstreamid: u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn AddInputStreams(
        &self,
        _cstreams: u32,
        _adwstreamids: *const u32,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetInputAvailableType(
        &self,
        _dwinputstreamid: u32,
        _dwtypeindex: u32,
    ) -> windows_core::Result<IMFMediaType> {
        Err(E_NOTIMPL.into())
    }
    fn GetOutputAvailableType(
        &self,
        _dwoutputstreamid: u32,
        _dwtypeindex: u32,
    ) -> windows_core::Result<IMFMediaType> {
        Err(E_NOTIMPL.into())
    }
    fn SetInputType(
        &self,
        _dwinputstreamid: u32,
        _ptype: windows_core::Ref<'_, IMFMediaType>,
        _dwflags: u32,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn SetOutputType(
        &self,
        _dwoutputstreamid: u32,
        _ptype: windows_core::Ref<'_, IMFMediaType>,
        _dwflags: u32,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetInputCurrentType(&self, _dwinputstreamid: u32) -> windows_core::Result<IMFMediaType> {
        Err(E_NOTIMPL.into())
    }
    fn GetOutputCurrentType(&self, _dwoutputstreamid: u32) -> windows_core::Result<IMFMediaType> {
        Err(E_NOTIMPL.into())
    }
    fn GetInputStatus(&self, _dwinputstreamid: u32) -> windows_core::Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn GetOutputStatus(&self) -> windows_core::Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn SetOutputBounds(
        &self,
        _hnslowerbound: i64,
        _hnsupperbound: i64,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn ProcessEvent(
        &self,
        _dwinputstreamid: u32,
        _pevent: windows_core::Ref<'_, IMFMediaEvent>,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn ProcessMessage(
        &self,
        _emessage: MFT_MESSAGE_TYPE,
        _ulparam: usize,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn ProcessInput(
        &self,
        _: u32,
        sample: windows_core::Ref<'_, IMFSample>,
        _: u32,
    ) -> windows_core::Result<()> {
        let timestamp = unsafe { sample.as_ref().unwrap().GetSampleTime()? };
        let mut script = self.0.lock().unwrap();
        script.inputs.push(timestamp);
        if script.rejections > 0 {
            script.rejections -= 1;
            return Err(MF_E_NOTACCEPTING.into());
        }
        script.pending = Some(timestamp);
        // 收下输入即消耗掉"可接收"宣布：与真实 MFT 一样，处理完输出后
        // 才会再次宣布 NeedInput（见 GetEvent / ProcessOutput）
        script.input_announced = false;
        Ok(())
    }

    fn ProcessOutput(
        &self,
        _: u32,
        count: u32,
        outputs: *mut MFT_OUTPUT_DATA_BUFFER,
        status: *mut u32,
    ) -> windows_core::Result<()> {
        let mut script = self.0.lock().unwrap();
        let Some(timestamp) = script.pending.take() else {
            return Err(MF_E_TRANSFORM_NEED_MORE_INPUT.into());
        };
        assert_eq!(count, 1);
        script.output_announced = false;
        // 输出取走后重新开放输入：真实 MFT 此时可以再次接收样本
        script.input_announced = false;
        unsafe {
            if let Some(sample) = (*outputs).pSample.as_ref().cloned() {
                // 调用方提供样本（MFT 不自行提供时）：写入其缓冲，不替换样本
                let buffer = sample.GetBufferByIndex(0)?;
                let mut ptr = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None)?;
                *ptr = 7;
                buffer.Unlock()?;
                buffer.SetCurrentLength(1)?;
                sample.SetSampleTime(timestamp)?;
            } else {
                let sample = MFCreateSample()?;
                let buffer = MFCreateMemoryBuffer(1)?;
                let mut ptr = std::ptr::null_mut();
                buffer.Lock(&mut ptr, None, None)?;
                *ptr = 7;
                buffer.Unlock()?;
                buffer.SetCurrentLength(1)?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(timestamp)?;
                (*outputs).pSample = std::mem::ManuallyDrop::new(Some(sample));
            }
            *status = 0;
        }
        Ok(())
    }
}

#[windows_core::implement(IMFMediaEventGenerator)]
struct Events(Arc<Mutex<Script>>);

impl IMFMediaEventGenerator_Impl for Events_Impl {
    fn GetEvent(
        &self,
        _: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    ) -> windows_core::Result<IMFMediaEvent> {
        let mut script = self.0.lock().unwrap();
        script.polls += 1;
        // 无待处理输入且尚未宣布过可接收时宣布 NeedInput；有待处理输入且
        // 尚未宣布过输出时宣布 HaveOutput。ProcessInput 消耗"可接收"宣布，
        // ProcessOutput 消耗输出宣布并重新开放输入——与真实 MFT 的事件节奏
        // 一致，连续多帧编码因此可以各自拿到令牌与输出事件。
        let kind = if script.pending.is_none() && !script.input_announced {
            script.input_announced = true;
            METransformNeedInput
        } else if script.pending.is_some() && !script.output_announced {
            script.output_announced = true;
            METransformHaveOutput
        } else {
            return Err(MF_E_NO_EVENTS_AVAILABLE.into());
        };
        unsafe {
            MFCreateMediaEvent(
                kind.0 as u32,
                &GUID::zeroed(),
                windows_core::HRESULT(0),
                None,
            )
        }
    }
    fn BeginGetEvent(
        &self,
        _: windows_core::Ref<'_, IMFAsyncCallback>,
        _: windows_core::Ref<'_, windows_core::IUnknown>,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EndGetEvent(
        &self,
        _: windows_core::Ref<'_, IMFAsyncResult>,
    ) -> windows_core::Result<IMFMediaEvent> {
        Err(E_NOTIMPL.into())
    }
    fn QueueEvent(
        &self,
        _: u32,
        _: *const GUID,
        _: windows_core::HRESULT,
        _: *const windows::Win32::System::Com::StructuredStorage::PROPVARIANT,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

fn encoder(rejections: usize, asynchronous: bool) -> (MfEncoder, Arc<Mutex<Script>>) {
    com_init_on_this_thread();
    ensure_mf_started();
    let script = Arc::new(Mutex::new(Script {
        rejections,
        ..Script::default()
    }));
    let transform: IMFTransform = Transform(script.clone()).into();
    let gen = if asynchronous {
        let events: IMFMediaEventGenerator = Events(script.clone()).into();
        Some(SendPtr(events))
    } else {
        None
    };
    (
        MfEncoder {
            codec: vnt_proto::Codec::H264,
            hardware: asynchronous,
            name: "scripted MFT".into(),
            transform: SendPtr(transform),
            codec_api: None,
            gen,
            need_input_tokens: 0,
            have_output: 0,
            params: Some(EncodeParams::new(64, 48, 30, 4000)),
            mft_provides_samples: true,
            output_buffer_size: 0,
            queued: VecDeque::new(),
            progress: OutputProgress::default(),
            dropped_inputs: 0,
            force_idr: false,
            first_sample: true,
        },
        script,
    )
}

/// 与 [`encoder`] 相同，但 MFT 不自行提供输出样本：每次 ProcessOutput 都
/// 由调用方按帧创建样本（不保留样本池的那条路径）。
fn encoder_with_own_output_samples(
    rejections: usize,
    asynchronous: bool,
) -> (MfEncoder, Arc<Mutex<Script>>) {
    let (mut encoder, script) = encoder(rejections, asynchronous);
    encoder.mft_provides_samples = false;
    encoder.output_buffer_size = 16;
    (encoder, script)
}

#[test]
fn synchronous_rejection_retries_the_same_frame_after_collecting_output() {
    let (mut encoder, script) = encoder(1, false);
    let mut packets = Vec::new();
    encoder
        .encode(&vec![0; 64 * 48 * 4], 42, &mut packets)
        .unwrap();
    assert_eq!(script.lock().unwrap().inputs, [420_000, 420_000]);
    assert_eq!(encoder.dropped_inputs, 0);
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].pts_ms, 42);
}

#[test]
fn synchronous_persistent_rejection_counts_the_drop_and_returns_an_error() {
    let (mut encoder, script) = encoder(10, false);
    let mut packets = Vec::new();
    assert!(encoder
        .encode(&vec![0; 64 * 48 * 4], 42, &mut packets)
        .is_err());
    assert_eq!(script.lock().unwrap().inputs.len(), 2);
    assert_eq!(encoder.dropped_inputs, 1);
    assert!(encoder.first_sample);
    assert!(packets.is_empty());
}

#[test]
fn queued_frame_is_submitted_and_output_without_another_capture() {
    let (mut encoder, script) = encoder(0, true);
    let mut packets = Vec::new();
    encoder
        .encode(&vec![0; 64 * 48 * 4], 42, &mut packets)
        .unwrap();
    assert_eq!(script.lock().unwrap().inputs, [420_000]);
    assert!(encoder.queued.is_empty());
    assert_eq!(packets.len(), 1);
    assert_eq!(packets[0].pts_ms, 42);
}

#[test]
fn output_samples_are_created_per_call_when_the_mft_does_not_provide_them() {
    // MFT 不自行提供样本：每次 ProcessOutput 都由调用方按帧创建样本，
    // 不保留样本池；驱动写入该样本，编码字节拷出后样本即释放
    for asynchronous in [false, true] {
        let (mut encoder, _script) = encoder_with_own_output_samples(0, asynchronous);
        let mut packets = Vec::new();
        for i in 0..3u64 {
            encoder
                .encode(&vec![i as u8; 64 * 48 * 4], i * 33, &mut packets)
                .unwrap();
        }
        assert_eq!(packets.len(), 3, "asynchronous={asynchronous}");
        assert!(packets.iter().all(|p| p.data == [7]));
        assert_eq!(
            packets.iter().map(|p| p.pts_ms).collect::<Vec<_>>(),
            vec![0, 33, 66]
        );
    }
}

#[test]
fn input_samples_are_created_per_frame_and_independent() {
    // 不保留输入样本池：每帧创建独立样本，缓冲互不重叠，属性互不影响
    com_init_on_this_thread();
    ensure_mf_started();
    let (mut encoder, _script) = encoder(0, false);
    let mut first: Option<IMFSample> = None;
    let mut first_buffer: Option<IMFMediaBuffer> = None;
    for i in 0..4u64 {
        let sample = encoder
            .fill_input_sample(&vec![i as u8; 64 * 48 * 4], 64, 48, i * 33, 30)
            .unwrap();
        let buffer = unsafe { sample.GetBufferByIndex(0).unwrap() };
        let max_length = unsafe { buffer.GetMaxLength().unwrap() };
        assert_eq!(max_length, (64 * 48 * 3 / 2) as u32);
        if let Some(first_buffer) = &first_buffer {
            assert_ne!(
                buffer.as_raw() as usize,
                first_buffer.as_raw() as usize,
                "每帧必须是独立缓冲，不得复用"
            );
        } else {
            first_buffer = Some(buffer.clone());
            first = Some(sample.clone());
        }
        drop(buffer);
    }
    // 首帧样本仍可独立存活并读取（驱动侧持有场景），不被后续帧影响
    let held = first.take().unwrap();
    let held_buffer = unsafe { held.GetBufferByIndex(0).unwrap() };
    assert!(unsafe { held.GetUINT32(&MFSampleExtension_Discontinuity) }.is_ok());
    drop(held_buffer);
    drop(held);
}

#[test]
fn input_samples_set_discontinuity_only_on_the_first_frame() {
    com_init_on_this_thread();
    ensure_mf_started();
    let (mut encoder, _script) = encoder(0, false);
    let first = encoder
        .fill_input_sample(&vec![1; 64 * 48 * 4], 64, 48, 0, 30)
        .unwrap();
    assert_eq!(
        unsafe { first.GetUINT32(&MFSampleExtension_Discontinuity) }.unwrap(),
        1,
        "首帧必须标记不连续"
    );
    drop(first);
    // 首个样本提交后才会清除首帧标记（与 encode 路径一致）
    encoder.first_sample = false;
    let second = encoder
        .fill_input_sample(&vec![2; 64 * 48 * 4], 64, 48, 33, 30)
        .unwrap();
    assert_eq!(
        unsafe { second.GetUINT32(&MFSampleExtension_Discontinuity) }.unwrap(),
        0,
        "后续帧不得标记不连续"
    );
}
