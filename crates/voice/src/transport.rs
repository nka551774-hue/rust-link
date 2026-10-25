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
            "aead_xchacha20_poly1305_rtpsize" => Some(Self::AeadXChaCha20Poly1305RtpSize),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::AeadAes256GcmRtpSize => "aead_aes256_gcm_rtpsize",
            Self::AeadXChaCha20Poly1305RtpSize => "aead_xchacha20_poly1305_rtpsize",
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

/// Expand the AES key at Session Description, outside the 50 Hz packet path.
pub(crate) enum PacketCipher {
    Aes(Box<Aes256Gcm>),
    // XChaCha has no reusable expanded key schedule. Keep its key zeroized
    // and construct the small per-packet cipher where it is used.
    XChaCha(Zeroizing<[u8; 32]>),
}

impl PacketCipher {
    pub(crate) fn new(mode: Mode, key: &[u8; 32]) -> Self {
        match mode {
            Mode::AeadAes256GcmRtpSize => Self::Aes(Box::new(Aes256Gcm::new(key.into()))),
            Mode::AeadXChaCha20Poly1305RtpSize => Self::XChaCha(Zeroizing::new(*key)),
        }
    }

    #[inline]
    pub(crate) fn seal_into(
        &self,
        packet: &mut Vec<u8>,
        header: &[u8; HEADER_LEN],
        counter: u32,
        payload: &[u8],
    ) -> Result<(), VoiceError> {
        packet.clear();
        packet.reserve(HEADER_LEN + payload.len() + 20);
        packet.extend_from_slice(header);
        packet.extend_from_slice(payload);
        // RTP-size modes append the counter bytes; receivers copy these into the nonce.
        let counter = counter.to_be_bytes();
        let tag = match self {
            Self::Aes(cipher) => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&counter);
                cipher.encrypt_in_place_detached(
                    Nonce::from_slice(&nonce),
                    header,
                    &mut packet[HEADER_LEN..],
                )
            }
            Self::XChaCha(key) => {
                let cipher = XChaCha20Poly1305::new((&**key).into());
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
}

pub fn ip_discovery_request(ssrc: u32) -> Vec<u8> {
    let mut bytes = vec![0; 74];
    bytes[..2].copy_from_slice(&1u16.to_be_bytes());
    bytes[2..4].copy_from_slice(&70u16.to_be_bytes());
    bytes[4..8].copy_from_slice(&ssrc.to_be_bytes());
    bytes
}

pub fn parse_ip_discovery_response(data: &[u8], ssrc: u32) -> Option<(String, u16)> {
    if data.len() != 74
        || data[..2] != 2u16.to_be_bytes()
        || data[2..4] != 70u16.to_be_bytes()
        || data[4..8] != ssrc.to_be_bytes()
    {
        return None;
    }
    let address = &data[8..72];
    let end = address.iter().position(|byte| *byte == 0)?;
    let address = std::str::from_utf8(&address[..end]).ok()?.parse::<IpAddr>().ok()?;
    let port = u16::from_be_bytes([data[72], data[73]]);
    if port == 0 {
        return None;
    }
    Some((address.to_string(), port))
}

struct State {
    cipher: Option<PacketCipher>,
    sequence: u16,
    timestamp: u32,
    counter: u64,
    packet: Vec<u8>,
}

pub struct Transport {
    socket: Arc<tokio::net::UdpSocket>,
    remote: SocketAddr,
    ssrc: u32,
    state: Mutex<State>,
}

impl Transport {
    pub fn new(socket: Arc<tokio::net::UdpSocket>, remote: SocketAddr, ssrc: u32) -> Self {
        Self {
            socket,
            remote,
            ssrc,
            state: Mutex::new(State {
                cipher: None,
                sequence: 0,
                timestamp: 0,
                counter: 0,
                packet: Vec::with_capacity(1500),
            }),
        }
    }

    pub fn describe(&self, mode: Mode, key: [u8; 32]) -> Result<(), VoiceError> {
        let key = Zeroizing::new(key);
        let mut state =
            self.state.lock().map_err(|_| VoiceError::Crypto("transport lock poisoned".into()))?;
        if state.cipher.is_some() {
            return Err(VoiceError::Crypto("transport key already installed".into()));
        }
        state.cipher = Some(PacketCipher::new(mode, &key));
        Ok(())
    }

    pub fn ready(&self) -> bool {
        self.state.lock().is_ok_and(|s| s.cipher.is_some())
    }

    pub fn write(&self, payload: &[u8]) -> Result<(), VoiceError> {
        let mut state =
            self.state.lock().map_err(|_| VoiceError::Crypto("transport lock poisoned".into()))?;
        let counter = u32::try_from(state.counter)
            .map_err(|_| VoiceError::Crypto("transport nonce exhausted; reconnect voice".into()))?;
        let rtp = header(state.sequence, state.timestamp, self.ssrc);
        let State { cipher, packet, .. } = &mut *state;
        cipher.as_ref().ok_or(VoiceError::NotReady)?.seal_into(packet, &rtp, counter, payload)?;
        state.counter += 1;
        state.sequence = state.sequence.wrapping_add(1);
        state.timestamp = state.timestamp.wrapping_add(FRAME_SAMPLES);
        self.socket.try_send_to(&state.packet, self.remote)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::Aead;

    #[test]
    fn both_modes_authenticate_headers_and_roundtrip_payloads() {
        let key = [73; 32];
        let rtp = header(7, 960, 12345);
        for mode in [Mode::AeadAes256GcmRtpSize, Mode::AeadXChaCha20Poly1305RtpSize] {
            let mut packet = vec![];
            PacketCipher::new(mode, &key)
                .seal_into(&mut packet, &rtp, 0x12345678, b"encrypted DAVE frame")
                .unwrap();
            assert_eq!(&packet[packet.len() - 4..], &0x12345678u32.to_be_bytes());
            let decrypt = |aad: &[u8]| {
                let payload =
                    chacha20poly1305::aead::Payload { msg: &packet[12..packet.len() - 4], aad };
                match mode {
                    Mode::AeadAes256GcmRtpSize => {
                        let mut nonce = [0u8; 12];
                        nonce[..4].copy_from_slice(&packet[packet.len() - 4..]);
                        Aes256Gcm::new((&key).into()).decrypt(Nonce::from_slice(&nonce), payload)
                    }
                    Mode::AeadXChaCha20Poly1305RtpSize => {
                        let mut nonce = [0u8; 24];
                        nonce[..4].copy_from_slice(&packet[packet.len() - 4..]);
                        XChaCha20Poly1305::new((&key).into())
                            .decrypt(XNonce::from_slice(&nonce), payload)
                    }
                }
            };
            assert_eq!(decrypt(&rtp).unwrap(), b"encrypted DAVE frame");
            assert!(decrypt(&header(8, 960, 12345)).is_err());
        }
    }

    #[test]
    fn discovery_rejects_wrong_type_stream_length_or_address() {
        let mut packet = ip_discovery_request(123);
        assert!(parse_ip_discovery_response(&packet, 123).is_none());
        packet[..2].copy_from_slice(&2u16.to_be_bytes());
        packet[8..17].copy_from_slice(b"127.0.0.1");
        packet[72..].copy_from_slice(&3456u16.to_be_bytes());
        assert_eq!(parse_ip_discovery_response(&packet, 123), Some(("127.0.0.1".into(), 3456)));
        assert!(parse_ip_discovery_response(&packet, 124).is_none());
        assert!(parse_ip_discovery_response(&packet[..73], 123).is_none());
        packet[2] = 1;
        assert!(parse_ip_discovery_response(&packet, 123).is_none());
    }

    #[test]
    fn negotiation_requires_supported_authenticated_modes() {
        assert_eq!(Mode::negotiate(&["plain".into()]), None);
        assert_eq!(
            Mode::negotiate(&["aead_aes256_gcm_rtpsize".into()]),
            Some(Mode::AeadAes256GcmRtpSize)
        );
        assert_eq!(
            Mode::negotiate(&["aead_xchacha20_poly1305_rtpsize".into()]),
            Some(Mode::AeadXChaCha20Poly1305RtpSize)
        );
        assert_eq!(Mode::parse("aead_xchacha20poly1305_rtpsize"), None);
        assert_eq!(Mode::AeadXChaCha20Poly1305RtpSize.name(), "aead_xchacha20_poly1305_rtpsize");
    }

    #[test]
    fn cached_cipher_reuses_packet_storage_and_advances_nonce() {
        for mode in [Mode::AeadAes256GcmRtpSize, Mode::AeadXChaCha20Poly1305RtpSize] {
            let cipher = PacketCipher::new(mode, &[73; 32]);
            let mut packet = Vec::with_capacity(1500);
            let pointer = packet.as_ptr();
            let rtp = header(1, 960, 77);
            cipher.seal_into(&mut packet, &rtp, 1, b"opus").unwrap();
            let first = packet.clone();
            cipher.seal_into(&mut packet, &rtp, 2, b"opus").unwrap();
            assert_eq!(packet.as_ptr(), pointer);
            assert_ne!(&first[HEADER_LEN..HEADER_LEN + 4], &packet[HEADER_LEN..HEADER_LEN + 4]);
            assert_eq!(&packet[packet.len() - 4..], &2u32.to_be_bytes());
        }
    }

    #[tokio::test]
    async fn exhausted_nonce_and_reinstallation_are_rejected() {
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let transport = Transport::new(socket.clone(), socket.local_addr().unwrap(), 1);
        let mode = Mode::AeadAes256GcmRtpSize;
        transport.describe(mode, [1; 32]).unwrap();
        assert!(transport.describe(mode, [1; 32]).is_err());
        transport.state.lock().unwrap().counter = u32::MAX as u64 + 1;
        assert!(matches!(transport.write(b"opus"), Err(VoiceError::Crypto(_))));
    }
}
