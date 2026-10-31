//! Small owned FFI surface; states never cross threads, and every allocation is freed.
use std::ffi::{c_char, c_int, c_void, CStr};
use std::ptr::NonNull;
use vnt_proto::audio::{FRAME_SAMPLES, MAX_OPUS_BYTES, SAMPLE_RATE};

unsafe extern "C" {
    fn opus_encoder_create(
        fs: c_int,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut c_void;
    fn opus_encoder_destroy(state: *mut c_void);
    fn opus_encoder_ctl(state: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode_float(
        state: *mut c_void,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max: c_int,
    ) -> c_int;
    fn opus_decoder_create(fs: c_int, channels: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_decoder_destroy(state: *mut c_void);
    fn opus_decode_float(
        state: *mut c_void,
        data: *const u8,
        len: c_int,
        pcm: *mut f32,
        frame_size: c_int,
        fec: c_int,
    ) -> c_int;
    fn opus_packet_get_nb_samples(data: *const u8, len: c_int, fs: c_int) -> c_int;
    fn opus_strerror(error: c_int) -> *const c_char;
}

fn check(code: c_int) -> Result<c_int, String> {
    if code >= 0 {
        return Ok(code);
    }
    // SAFETY: libopus returns a static, NUL-terminated string for every error code.
    Err(unsafe { CStr::from_ptr(opus_strerror(code)) }
        .to_string_lossy()
        .into_owned())
}

pub struct Encoder(NonNull<c_void>);
impl Encoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        // SAFETY: valid Opus sample rate, channel count and restricted-lowdelay application.
        let ptr = unsafe { opus_encoder_create(SAMPLE_RATE as i32, 2, 2051, &mut error) };
        check(error)?;
        let state = Self(NonNull::new(ptr).ok_or("Opus encoder allocation failed")?);
        // SAFETY: state belongs to this encoder; these requests all take one int.
        unsafe {
            check(opus_encoder_ctl(state.0.as_ptr(), 4002, 64_000i32))?; // bitrate
            check(opus_encoder_ctl(state.0.as_ptr(), 4006, 1i32))?; // VBR
            check(opus_encoder_ctl(state.0.as_ptr(), 4020, 1i32))?; // constrained VBR
            check(opus_encoder_ctl(state.0.as_ptr(), 4010, 5i32))?; // complexity
        }
        Ok(state)
    }
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, String> {
        if pcm.len() != FRAME_SAMPLES as usize * 2 || pcm.iter().any(|x| !x.is_finite()) {
            return Err("invalid 10ms stereo PCM frame".into());
        }
        let mut data = vec![0; MAX_OPUS_BYTES];
        // SAFETY: the input holds 480 stereo samples, and the output is sized to max.
        let len = check(unsafe {
            opus_encode_float(
                self.0.as_ptr(),
                pcm.as_ptr(),
                FRAME_SAMPLES as i32,
                data.as_mut_ptr(),
                data.len() as i32,
            )
        })?;
        data.truncate(len as usize);
        Ok(data)
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.0.as_ptr()) }
    }
}

pub struct Decoder(NonNull<c_void>);
impl Decoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let ptr = unsafe { opus_decoder_create(SAMPLE_RATE as i32, 2, &mut error) };
        check(error)?;
        Ok(Self(
            NonNull::new(ptr).ok_or("Opus decoder allocation failed")?,
        ))
    }
    /// None requests exactly one 10ms frame of packet-loss concealment.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> Result<Vec<[f32; 2]>, String> {
        let (ptr, len) = match packet {
            Some(data) => {
                if data.is_empty()
                    || data.len() > MAX_OPUS_BYTES
                    || unsafe {
                        opus_packet_get_nb_samples(
                            data.as_ptr(),
                            data.len() as i32,
                            SAMPLE_RATE as i32,
                        )
                    } != FRAME_SAMPLES as i32
                {
                    return Err("invalid Opus packet duration or length".into());
                }
                (data.as_ptr(), data.len() as i32)
            }
            None => (std::ptr::null(), 0),
        };
        let mut pcm = vec![0.0; FRAME_SAMPLES as usize * 2];
        // SAFETY: decoder is exclusively owned; output fits exactly 480 stereo samples.
        let frames = check(unsafe {
            opus_decode_float(
                self.0.as_ptr(),
                ptr,
                len,
                pcm.as_mut_ptr(),
                FRAME_SAMPLES as i32,
                0,
            )
        })?;
        if frames != FRAME_SAMPLES as i32 {
            return Err("unexpected Opus duration".into());
        }
        Ok(pcm.as_chunks::<2>().0.to_vec())
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { opus_decoder_destroy(self.0.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_opus_roundtrip_and_plc_are_bounded() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let mut total = 0;
        let mut energy = 0.0;
        for frame in 0..100 {
            let pcm: Vec<f32> = (0..480)
                .flat_map(|i| {
                    let x = ((frame * 480 + i) as f32 * 440.0 * std::f32::consts::TAU / 48000.0)
                        .sin()
                        * 0.25;
                    [x, -x]
                })
                .collect();
            let packet = encoder.encode(&pcm).unwrap();
            total += packet.len();
            let decoded = decoder.decode(Some(&packet)).unwrap();
            assert_eq!(decoded.len(), 480);
            energy += decoded.iter().map(|x| x[0] * x[0]).sum::<f32>();
        }
        assert!(energy > 100.0);
        assert!(
            total < 12_000,
            "constrained VBR exceeded expected bandwidth: {total}"
        );
        assert_eq!(decoder.decode(None).unwrap().len(), 480);
        assert!(decoder.decode(Some(&[255; 1276])).is_err());
        assert!(encoder.encode(&[0.0; 479]).is_err());
    }
}
