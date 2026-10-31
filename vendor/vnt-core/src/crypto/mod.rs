use crate::crypto::chacha20_poly1305::TAG_LEN;
use crate::protocol::ip_packet_protocol::NetPacket;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

mod chacha20_poly1305;

use crate::protocol::transmission::{ExtendEnd, ShrinkEnd};

#[derive(Clone)]
pub(crate) struct PacketCrypto {
    crypto: Option<Arc<chacha20_poly1305::PacketCrypto>>,
    plaintext_seq: Arc<AtomicU32>,
}
impl PacketCrypto {
    pub(crate) fn key_sign(s: &str) -> String {
        chacha20_poly1305::PacketCrypto::key_sign(s)
    }

    pub(crate) fn new_from_str(s: Option<&str>) -> io::Result<Self> {
        let crypto = s
            .map(chacha20_poly1305::PacketCrypto::new_from_str)
            .transpose()?
            .map(Arc::new);
        Ok(Self {
            crypto,
            plaintext_seq: Arc::new(AtomicU32::new(rand::random())),
        })
    }
    pub(crate) fn encrypt_reserve(&self) -> usize {
        if self.crypto.is_some() { TAG_LEN } else { 0 }
    }

    pub(crate) fn encrypt_in_place<B: AsRef<[u8]> + AsMut<[u8]> + ExtendEnd>(
        &self,
        pkt: &mut NetPacket<B>,
    ) -> io::Result<()> {
        if let Some(crypto) = self.crypto.as_ref() {
            pkt.source_buf_mut().extend_end(TAG_LEN);
            return crypto.encrypt_in_place(pkt);
        }
        pkt.set_seq(self.plaintext_seq.fetch_add(1, Ordering::Relaxed));
        Ok(())
    }
    pub(crate) fn decrypt_in_place<B: AsRef<[u8]> + AsMut<[u8]> + ShrinkEnd>(
        &self,
        pkt: &mut NetPacket<B>,
    ) -> io::Result<()> {
        if let Some(crypto) = self.crypto.as_ref() {
            let _ = crypto.decrypt_in_place(pkt)?;
            pkt.source_buf_mut().shrink_end(TAG_LEN);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ip_packet_protocol::HEAD_LENGTH;
    use crate::protocol::transmission::TransmissionBytes;

    #[test]
    fn plaintext_packets_still_receive_unique_sequences() {
        let crypto = PacketCrypto::new_from_str(None).unwrap();
        let mut first = NetPacket::new(TransmissionBytes::zeroed(HEAD_LENGTH)).unwrap();
        let mut second = NetPacket::new(TransmissionBytes::zeroed(HEAD_LENGTH)).unwrap();
        crypto.encrypt_in_place(&mut first).unwrap();
        crypto.encrypt_in_place(&mut second).unwrap();
        assert_ne!(first.seq(), second.seq());
    }
}
