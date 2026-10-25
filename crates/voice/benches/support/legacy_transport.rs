//! Discord RTP-size transport encryption. DAVE frames are encrypted before this layer.
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use aes_gcm::{Aes256Gcm, Nonce};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::VoiceError;

pub const HEADER_LEN: usize = 12;
pub const FRAME_SAMPLES: u32 = 960;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    AeadAes256GcmRtpSize,
    AeadXChaCha20Poly1305RtpSize,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "aead_aes256_gcm_rtpsize" => Some(Self::AeadAes256GcmRtpSize),
            "aead_xchacha20poly1305_rtpsize" => Some(Self::AeadXChaCha20Poly1305RtpSize),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::AeadAes256GcmRtpSize => "aead_aes256_gcm_rtpsize",
            Self::AeadXChaCha20Poly1305RtpSize => "aead_xchacha20poly1305_rtpsize",
        }
    }

    pub fn negotiate(offered: &[String]) -> Option<Self> {
        [Self::AeadAes256GcmRtpSize, Self::AeadXChaCha20Poly1305RtpSize]
            .into_iter()
            .find(|m| offered.iter().any(|s| Self::parse(s) == Some(*m)))
    }
}

pub fn header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; HEADER_LEN] {
    let mut bytes = [0u8; HEADER_LEN];
    bytes[0] = 0x80;
    bytes[1] = 0x78;
    bytes[2..4].copy_from_slice(&sequence.to_be_bytes());
    bytes[4..8].copy_from_slice(&timestamp.to_be_bytes());
    bytes[8..12].copy_from_slice(&ssrc.to_be_bytes());
    bytes
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
    // RTP-size modes append the counter bytes; receivers copy these into the nonce.
    let counter = counter.to_be_bytes();
    let tag = match mode {
        Mode::AeadAes256GcmRtpSize => {
            let cipher = Aes256Gcm::new(key.into());
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&counter);
            cipher.encrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                header,
                &mut packet[HEADER_LEN..],
            )
        }
        Mode::AeadXChaCha20Poly1305RtpSize => {
            let cipher = XChaCha20Poly1305::new(key.into());
            let mut nonce = [0u8; 24];
            nonce[..4].copy_from_slice(&counter);
            cipher.encrypt_in_place_detached(
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
