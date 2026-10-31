use crate::{playback::Player, PlaybackDevice};
pub fn list_playback_devices() -> Result<Vec<PlaybackDevice>, String> {
    serde_json::from_value(vnt_macos::query(7)?).map_err(|e| e.to_string())
}
pub struct Capture {
    stream: vnt_macos::Stream,
    format: crate::format::PcmFormat,
}
impl Capture {
    pub fn open() -> Result<Self, String> {
        let format = crate::format::PcmFormat {
            rate: 48000,
            channels: 2,
            bits: 32,
            float: true,
            mask: 3,
        };
        format.validate()?;
        Ok(Self {
            stream: vnt_macos::Stream::open(0, true)?,
            format,
        })
    }
    pub fn rate(&self) -> u32 {
        self.format.rate
    }
    pub fn read(&mut self) -> Result<Vec<[f32; 2]>, String> {
        let mut bytes = vec![0; 48000 * 8 / 5];
        let (n, _, _) = self.stream.read(&mut bytes)?;
        Ok(self.format.decode(&bytes[..n]))
    }
}
pub struct Render {
    native: vnt_macos::Render,
    fallback: bool,
    device: String,
    default: bool,
    next_check: std::time::Instant,
}
impl Render {
    pub fn open_on_device(id: Option<&str>) -> Result<Self, String> {
        let devices = list_playback_devices()?;
        let chosen = id.and_then(|id| devices.iter().find(|d| d.id == id));
        let fallback = id.is_some() && chosen.is_none();
        let device = chosen
            .or_else(|| devices.iter().find(|d| d.is_default))
            .ok_or("暂无播放设备")?
            .id
            .clone();
        Ok(Self {
            native: vnt_macos::Render::open(Some(&device))?,
            fallback,
            device,
            default: id.is_none() || fallback,
            next_check: std::time::Instant::now(),
        })
    }
    pub fn is_fallback(&self) -> bool {
        self.fallback
    }
    pub fn rate(&self) -> u32 {
        48000
    }
    pub fn write(&mut self, player: &mut Player) -> Result<(), String> {
        if self.next_check <= std::time::Instant::now() {
            let devices = list_playback_devices()?;
            if !devices
                .iter()
                .any(|d| d.id == self.device && (!self.default || d.is_default))
            {
                return Err("音频播放设备已变化".into());
            }
            self.next_check = std::time::Instant::now() + std::time::Duration::from_secs(1);
        }
        self.native.write(&player.read(480)?)?;
        std::thread::park_timeout(std::time::Duration::from_millis(10));
        Ok(())
    }
}
