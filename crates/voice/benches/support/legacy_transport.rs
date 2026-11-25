//! 0.4.0 per-packet cipher initialization baseline for RTP-size transport benchmarks.
use aes_gcm::{Aes256Gcm, Nonce};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::VoiceError;

pub const HEADER_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    AeadAes256GcmRtpSize,
    AeadXChaCha20Poly1305RtpSize,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "aead_aes256_gcm_rtpsize" => Some(Self::AeadAes256GcmRtpSize),
            "aead_xchacha20_poly1305_rtpsize" => Some(Self::AeadXChaCha20Poly1305RtpSize),
            _ => None,
        }
    }
}

pub fn seal_into(
    packet: &mut Vec<u8>,
    mode: Mode,
    key: &[u8; 32],
    header: &[u8; HEADER_LEN],
    counter: u32,
    payload: &[u8],
) -> Result<(), VoiceError> {
    packet.clear();
    packet.extend_from_slice(header);
    packet.extend_from_slice(payload);
    let counter = counter.to_be_bytes();
    let tag = match mode {
        Mode::AeadAes256GcmRtpSize => {
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&counter);
            Aes256Gcm::new(key.into()).encrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                header,
                &mut packet[HEADER_LEN..],
            )
        }
        Mode::AeadXChaCha20Poly1305RtpSize => {
            let mut nonce = [0u8; 24];
            nonce[..4].copy_from_slice(&counter);
            XChaCha20Poly1305::new(key.into()).encrypt_in_place_detached(
                XNonce::from_slice(&nonce),
                header,
                &mut packet[HEADER_LEN..],
            )
        }
    }
    .map_err(|_| VoiceError::Crypto("transport encryption failed".into()))?;
    packet.extend_from_slice(&tag);
    packet.extend_from_slice(&counter);
    Ok(())
}
