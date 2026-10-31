//! WASAPI shared-mode loopback capture and event-driven native rendering.
//! All interfaces live on a dedicated MTA thread; callbacks only set an atomic flag.
use crate::{format::PcmFormat, playback::Player};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use windows::core::{implement, w, Interface, Result as WinResult, PCWSTR, PWSTR};
use windows::Win32::{
    Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
    Foundation::{CloseHandle, HANDLE, PROPERTYKEY, WAIT_FAILED},
    Media::Audio::*,
    System::{
        Com::{
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc, PROPVARIANT},
            *,
        },
        Threading::{
            AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
            WaitForSingleObject,
        },
    },
};

struct Com;
impl Com {
    fn new() -> Result<Self, String> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .map_err(|e| e.to_string())?;
        Ok(Self)
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}
struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
struct MixFormat(*mut WAVEFORMATEX);
impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) }
    }
}

struct MultimediaTask(HANDLE);
impl Drop for MultimediaTask {
    fn drop(&mut self) {
        let _ = unsafe { AvRevertMmThreadCharacteristics(self.0) };
    }
}

struct ComString(PWSTR);
impl ComString {
    fn into_string(self) -> Result<String, String> {
        unsafe { self.0.to_string() }.map_err(|e| e.to_string())
    }
}
impl Drop for ComString {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.as_ptr().cast())) }
    }
}
struct Property(PROPVARIANT);
impl Drop for Property {
    fn drop(&mut self) {
        let _ = unsafe { PropVariantClear(&mut self.0) };
    }
}

pub fn list_playback_devices() -> Result<Vec<crate::PlaybackDevice>, String> {
    let _com = Com::new()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let default_id = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .and_then(|device| device.GetId())
            .ok()
            .and_then(|id| ComString(id).into_string().ok());
        let devices = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| e.to_string())?;
        let mut result = Vec::new();
        for i in 0..devices.GetCount().map_err(|e| e.to_string())? {
            // Endpoints may disappear between enumeration and property access.
            let Ok(device) = devices.Item(i) else {
                continue;
            };
            let Ok(id) = device
                .GetId()
                .map_err(|e| e.to_string())
                .and_then(|id| ComString(id).into_string())
            else {
                continue;
            };
            let name = (|| -> WinResult<String> {
                let store = device.OpenPropertyStore(STGM_READ)?;
                let property = Property(store.GetValue(&PKEY_Device_FriendlyName)?);
                let value = ComString(PropVariantToStringAlloc(&property.0)?);
                Ok(value.0.to_string()?)
            })()
            .ok()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| id.clone());
            result.push(crate::PlaybackDevice {
                is_default: default_id.as_ref() == Some(&id),
                id,
                name,
            });
        }
        result.sort_by(|a, b| {
            b.is_default
                .cmp(&a.is_default)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(result)
    }
}

#[implement(IMMNotificationClient)]
struct Notifications {
    changed: Arc<AtomicBool>,
    follow_default: bool,
    device_id: String,
}
#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for Notifications_Impl {
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> WinResult<()> {
        if self.follow_default && flow == eRender && role == eConsole {
            self.changed.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn OnDeviceStateChanged(&self, id: &PCWSTR, state: DEVICE_STATE) -> WinResult<()> {
        if state != DEVICE_STATE_ACTIVE
            && unsafe { id.to_string() }.is_ok_and(|id| id == self.device_id)
        {
            self.changed.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> WinResult<()> {
        if unsafe { id.to_string() }.is_ok_and(|id| id == self.device_id) {
            self.changed.store(true, Ordering::Release);
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> WinResult<()> {
        Ok(())
    }
}
struct NotificationRegistration {
    enumerator: IMMDeviceEnumerator,
    callback: IMMNotificationClient,
}
impl Drop for NotificationRegistration {
    fn drop(&mut self) {
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.callback)
        };
    }
}

struct Stream {
    client: IAudioClient,
    device_id: String,
    format: PcmFormat,
    event: Event,
    changed: Arc<AtomicBool>,
    _notifications: NotificationRegistration,
    _scheduling: Option<MultimediaTask>,
    _com: Com,
}
impl Stream {
    fn open(loopback: bool, device_id: Option<&str>) -> Result<Self, String> {
        unsafe {
            let com = Com::new()?;
            let mut task_index = 0;
            let scheduling = AvSetMmThreadCharacteristicsW(w!("Audio"), &mut task_index)
                .ok()
                .map(MultimediaTask);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| e.to_string())?;
            let changed = Arc::new(AtomicBool::new(false));
            let device = if let Some(id) = device_id {
                let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                let device = enumerator
                    .GetDevice(PCWSTR(wide.as_ptr()))
                    .map_err(|e| e.to_string())?;
                if device
                    .cast::<IMMEndpoint>()
                    .and_then(|endpoint| endpoint.GetDataFlow())
                    .map_err(|e| e.to_string())?
                    != eRender
                {
                    return Err("所选设备不是音频播放设备".into());
                }
                if device.GetState().map_err(|e| e.to_string())? != DEVICE_STATE_ACTIVE {
                    return Err("所选播放设备不可用".into());
                }
                device
            } else {
                enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .map_err(|e| e.to_string())?
            };
            let actual_id = ComString(device.GetId().map_err(|e| e.to_string())?).into_string()?;
            let callback: IMMNotificationClient = Notifications {
                changed: changed.clone(),
                follow_default: device_id.is_none(),
                device_id: actual_id.clone(),
            }
            .into();
            enumerator
                .RegisterEndpointNotificationCallback(&callback)
                .map_err(|e| e.to_string())?;
            let notifications = NotificationRegistration {
                enumerator,
                callback,
            };
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| e.to_string())?;
            let mix = MixFormat(client.GetMixFormat().map_err(|e| e.to_string())?);
            let base = std::ptr::read_unaligned(mix.0);
            let (tag, mask) = if base.wFormatTag == 0xfffe && base.cbSize >= 22 {
                let ext = std::ptr::read_unaligned(mix.0.cast::<WAVEFORMATEXTENSIBLE>());
                let sub = ext.SubFormat;
                if sub.data2 != 0
                    || sub.data3 != 0x10
                    || sub.data4 != [0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]
                {
                    return Err("unsupported audio device subformat".into());
                }
                (sub.data1 as u16, ext.dwChannelMask)
            } else {
                (base.wFormatTag, 0)
            };
            if tag != 1 && tag != 3 {
                return Err("audio device does not provide PCM".into());
            }
            let format = PcmFormat {
                rate: base.nSamplesPerSec,
                channels: base.nChannels as usize,
                bits: base.wBitsPerSample,
                float: tag == 3,
                mask,
            };
            format.validate()?;
            if base.nBlockAlign as usize != format.frame_bytes() {
                return Err("invalid PCM block alignment".into());
            }
            let event = Event(CreateEventW(None, false, false, None).map_err(|e| e.to_string())?);
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | if loopback {
                    AUDCLNT_STREAMFLAGS_LOOPBACK
                } else {
                    0
                };
            // IAudioClient3 supports smaller shared-mode render periods on Windows 10.
            // Loopback uses the regular shared-mode engine period for driver compatibility.
            let low_latency = if !loopback {
                client.cast::<IAudioClient3>().ok().is_some_and(|client3| {
                    let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
                    client3
                        .GetSharedModeEnginePeriod(
                            mix.0,
                            &mut default,
                            &mut fundamental,
                            &mut min,
                            &mut max,
                        )
                        .is_ok()
                        && client3
                            .InitializeSharedAudioStream(flags, min, mix.0, None)
                            .is_ok()
                })
            } else {
                false
            };
            if !low_latency {
                client
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, mix.0, None)
                    .map_err(|e| e.to_string())?;
            }
            client.SetEventHandle(event.0).map_err(|e| e.to_string())?;
            Ok(Self {
                client,
                device_id: actual_id,
                format,
                event,
                changed,
                _notifications: notifications,
                _scheduling: scheduling,
                _com: com,
            })
        }
    }
    fn wait(&self) -> Result<(), String> {
        if self.changed.load(Ordering::Acquire) {
            return Err("播放设备已变化，正在重新连接".into());
        }
        if unsafe { WaitForSingleObject(self.event.0, 20) } == WAIT_FAILED {
            return Err("audio event wait failed".into());
        }
        Ok(())
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

pub struct Capture {
    capture: IAudioCaptureClient,
    stream: Stream,
}
impl Capture {
    pub fn open() -> Result<Self, String> {
        let stream = Stream::open(true, None)?;
        let capture = unsafe { stream.client.GetService() }.map_err(|e| e.to_string())?;
        unsafe { stream.client.Start() }.map_err(|e| e.to_string())?;
        Ok(Self { capture, stream })
    }
    pub fn rate(&self) -> u32 {
        self.stream.format.rate
    }
    pub fn read(&mut self) -> Result<Vec<[f32; 2]>, String> {
        unsafe {
            self.stream.wait()?;
            let mut samples = Vec::new();
            let mut buffers = 0;
            while self
                .capture
                .GetNextPacketSize()
                .map_err(|e| e.to_string())?
                > 0
            {
                buffers += 1;
                let (mut ptr, mut frames, mut flags) = (std::ptr::null_mut(), 0, 0);
                self.capture
                    .GetBuffer(&mut ptr, &mut frames, &mut flags, None, None)
                    .map_err(|e| e.to_string())?;
                // A driver may return a large buffer after scheduling stalls. Keep
                // at most the newest 60ms; decoding a stale backlog defeats low latency.
                let keep = frames.min(self.rate() * 60 / 1000) as usize;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || ptr.is_null() {
                    samples.extend(std::iter::repeat_n([0.0; 2], keep));
                } else {
                    let offset = (frames as usize - keep) * self.stream.format.frame_bytes();
                    let bytes = std::slice::from_raw_parts(
                        ptr.add(offset),
                        keep * self.stream.format.frame_bytes(),
                    );
                    samples.extend(self.stream.format.decode(bytes));
                }
                self.capture
                    .ReleaseBuffer(frames)
                    .map_err(|e| e.to_string())?;
                if samples.len() > self.rate() as usize * 60 / 1000 {
                    samples.drain(..samples.len() - self.rate() as usize * 60 / 1000);
                }
                // Bound each drain so even a busy/slow device returns to the
                // owner loop, where stop and default-device changes are observed.
                if buffers >= 16 {
                    break;
                }
            }
            Ok(samples)
        }
    }
}

pub struct Render {
    render: IAudioRenderClient,
    stream: Stream,
    size: u32,
    fallback: bool,
}
impl Render {
    pub fn open_on_device(device_id: Option<&str>) -> Result<Self, String> {
        let stream = Stream::open(false, device_id).or_else(|error| {
            if device_id.is_some() {
                Stream::open(false, None)
            } else {
                Err(error)
            }
        })?;
        let fallback = device_id.is_some_and(|id| id != stream.device_id);
        let render = unsafe { stream.client.GetService() }.map_err(|e| e.to_string())?;
        let size = unsafe { stream.client.GetBufferSize() }.map_err(|e| e.to_string())?;
        unsafe { stream.client.Start() }.map_err(|e| e.to_string())?;
        Ok(Self {
            render,
            stream,
            size,
            fallback,
        })
    }
    pub fn is_fallback(&self) -> bool {
        self.fallback
    }
    pub fn rate(&self) -> u32 {
        self.stream.format.rate
    }
    pub fn write(&mut self, player: &mut Player) -> Result<(), String> {
        unsafe {
            self.stream.wait()?;
            let padding = self
                .stream
                .client
                .GetCurrentPadding()
                .map_err(|e| e.to_string())?;
            // Some drivers allocate more than requested. Filling their entire
            // buffer would add device latency outside our bounded jitter queue.
            let target_padding = self.size.min(self.rate() * 20 / 1000);
            let frames = target_padding.saturating_sub(padding);
            if frames == 0 {
                return Ok(());
            }
            let pcm = player.read(frames as usize)?;
            let ptr = self.render.GetBuffer(frames).map_err(|e| e.to_string())?;
            self.stream.format.encode(
                &pcm,
                std::slice::from_raw_parts_mut(
                    ptr,
                    frames as usize * self.stream.format.frame_bytes(),
                ),
            );
            self.render
                .ReleaseBuffer(frames, 0)
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_endpoint_ignores_unrelated_default_changes_but_detects_removal() {
        let changed = Arc::new(AtomicBool::new(false));
        let callback: IMMNotificationClient = Notifications {
            changed: changed.clone(),
            follow_default: false,
            device_id: "selected-device".into(),
        }
        .into();
        unsafe {
            callback
                .OnDefaultDeviceChanged(eRender, eConsole, PCWSTR::null())
                .unwrap();
            assert!(!changed.load(Ordering::Acquire));
            callback
                .OnDeviceStateChanged(w!("other-device"), DEVICE_STATE_DISABLED)
                .unwrap();
            assert!(!changed.load(Ordering::Acquire));
            callback
                .OnDeviceStateChanged(w!("selected-device"), DEVICE_STATE_DISABLED)
                .unwrap();
            assert!(changed.swap(false, Ordering::AcqRel));
            callback.OnDeviceRemoved(w!("selected-device")).unwrap();
            assert!(changed.swap(false, Ordering::AcqRel));
        }
        let callback: IMMNotificationClient = Notifications {
            changed: changed.clone(),
            follow_default: true,
            device_id: "default-device".into(),
        }
        .into();
        unsafe {
            callback
                .OnDefaultDeviceChanged(eRender, eConsole, PCWSTR::null())
                .unwrap();
        }
        assert!(changed.load(Ordering::Acquire));
    }

    #[test]
    #[ignore = "requires a Windows default playback endpoint; opens devices without playing sound"]
    fn playback_device_enumeration_and_explicit_routing_probe() {
        let devices = list_playback_devices().expect("enumerate active playback endpoints");
        let default = devices
            .iter()
            .find(|device| device.is_default)
            .expect("default playback endpoint");
        assert!(devices
            .iter()
            .all(|device| !device.id.is_empty() && !device.name.is_empty()));
        let mut ids: Vec<_> = devices.iter().map(|device| &device.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), devices.len());
        let render = Render::open_on_device(Some(&default.id)).expect("open selected endpoint");
        assert!(!render.is_fallback());
        assert_eq!(render.stream.device_id, default.id);
        drop(render);
        let fallback = Render::open_on_device(Some("vnt-remote-missing-playback-device"))
            .expect("fallback to system default");
        assert!(fallback.is_fallback());
        assert_eq!(fallback.stream.device_id, default.id);
    }
}
