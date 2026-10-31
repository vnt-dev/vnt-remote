//! Balance Windows timer-resolution requests for the lifetime of active video work.
pub struct TimerResolution(#[cfg(windows)] bool);

impl TimerResolution {
    pub fn acquire() -> Self {
        #[cfg(windows)]
        {
            let ok = unsafe { timeBeginPeriod(1) } == 0;
            if !ok {
                log::warn!("Could not request 1 ms timer resolution");
            }
            Self(ok)
        }
        #[cfg(not(windows))]
        Self()
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.0 {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}
