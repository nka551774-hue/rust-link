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

    pub fn negotiate(offered: impl IntoIterator<Item = impl AsRef<str>>) -> Option<Self> {
        let mut has_chacha = false;
        for item in offered {
            match Self::parse(item.as_ref()) {
                Some(Self::AeadAes256GcmRtpSize) => return Some(Self::AeadAes256GcmRtpSize),
                Some(Self::AeadXChaCha20Poly1305RtpSize) => has_chacha = true,
                None => {}
            }
        }
        has_chacha.then_some(Self::AeadXChaCha20Poly1305RtpSize)
    }
}

#[inline]
pub fn header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; HEADER_LEN] {
    let mut bytes = [0u8; HEADER_LEN];
    bytes[0] = 0x80;
    bytes[1] = 0x78;
    bytes[2..4].copy_from_slice(&sequence.to_be_bytes());
    bytes[4..8].copy_from_slice(&timestamp.to_be_bytes());
    bytes[8..12].copy_from_slice(&ssrc.to_be_bytes());
    bytes
}

enum CipherInner {
    Aes(Box<Aes256Gcm>),
    // Boxed once per session so `ZeroizeOnDrop` runs on teardown rather than per packet.
    XChaCha(Box<XChaCha20Poly1305>),
}

/// Pre-initialized AEAD cipher for the 50 Hz outgoing Opus packet path.
pub(crate) struct PacketCipher {
    inner: CipherInner,
    counter: u64,
}

impl PacketCipher {
    pub(crate) fn new(mode: Mode, key: &[u8; 32]) -> Self {
        let inner = match mode {
            Mode::AeadAes256GcmRtpSize => CipherInner::Aes(Box::new(Aes256Gcm::new(key.into()))),
            Mode::AeadXChaCha20Poly1305RtpSize => {
                CipherInner::XChaCha(Box::new(XChaCha20Poly1305::new(key.into())))
            }
        };
        Self { inner, counter: 0 }
    }

    #[inline]
    fn next_counter(&mut self) -> Result<u32, VoiceError> {
        let counter = u32::try_from(self.counter)
            .map_err(|_| VoiceError::Crypto("transport nonce exhausted; reconnect voice".into()))?;
        self.counter += 1;
        Ok(counter)
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
        self.seal_detached(packet, header, counter)
    }

    #[inline]
    pub(crate) fn seal_in_place(
        &mut self,
        frame: &mut Vec<u8>,
        header: &[u8; HEADER_LEN],
    ) -> Result<(), VoiceError> {
        if frame.len() < HEADER_LEN {
            return Err(VoiceError::Crypto("transport frame missing RTP header space".into()));
        }
        let counter = self.next_counter()?;
        frame[..HEADER_LEN].copy_from_slice(header);
        frame.reserve(20);
        self.seal_detached(frame, header, counter)
    }

    #[inline]
    fn seal_detached(
        &self,
        frame: &mut Vec<u8>,
        header: &[u8; HEADER_LEN],
        counter: u32,
    ) -> Result<(), VoiceError> {
        let counter = counter.to_be_bytes();
        let tag = match &self.inner {
            CipherInner::Aes(cipher) => {
                let mut nonce = [0u8; 12];
                nonce[..4].copy_from_slice(&counter);
                cipher.encrypt_in_place_detached(
                    Nonce::from_slice(&nonce),
                    header,
                    &mut frame[HEADER_LEN..],
                )
            }
            CipherInner::XChaCha(cipher) => {
                let mut nonce = [0u8; 24];
                nonce[..4].copy_from_slice(&counter);
                cipher.encrypt_in_place_detached(
                    XNonce::from_slice(&nonce),
                    header,
                    &mut frame[HEADER_LEN..],
                )
            }
        }
        .map_err(|_| VoiceError::Crypto("transport encryption failed".into()))?;
        frame.extend_from_slice(&tag);
        frame.extend_from_slice(&counter);
        Ok(())
    }
}

pub(crate) fn valid_voice_udp_ip(raw_ip: IpAddr, require_public: bool) -> Option<IpAddr> {
    let ip = raw_ip.to_canonical();
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 0 || v4.is_multicast() || v4.is_broadcast() {
                return None;
            }
            if require_public
                && (v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_documentation()
                    || matches!(
                        o,
                        [100, 64..=127, ..] | [198, 18 | 19, ..] | [192, 0, 0, _] | [240..=255, ..]
                    ))
            {
                return None;
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() {
                return None;
            }
            if require_public
                && (v6.is_loopback()
                    || matches!(
                        v6.segments(),
                        [0xfc00..=0xfdff, ..]
                            | [0xfe80..=0xfebf, ..]
                            | [0x2001, 0x0db8, ..]
                            | [0x2001, 0x0002, 0, ..]
                            | [0x0100, 0, 0, 0, ..]
                            | [0x0064, 0xff9b, 0, 0, 0, 0, ..]
                            | [0x0064, 0xff9b, 1, ..]
                    ))
            {
                return None;
            }
        }
    }
    Some(ip)
}

pub fn ip_discovery_request(ssrc: u32) -> [u8; 74] {
    let mut bytes = [0u8; 74];
    bytes[..4].copy_from_slice(&[0, 1, 0, 70]);
    bytes[4..8].copy_from_slice(&ssrc.to_be_bytes());
    bytes
}

pub fn parse_ip_discovery_response(data: &[u8], ssrc: u32) -> Option<(String, u16)> {
    if ssrc == 0
        || data.len() != 74
        || data[..4] != [0, 2, 0, 70]
        || data[4..8] != ssrc.to_be_bytes()
    {
        return None;
    }
    let end = data[8..72].iter().position(|&b| b == 0)?;
    let ip = std::str::from_utf8(&data[8..8 + end]).ok()?.parse::<IpAddr>().ok()?;
    let port = u16::from_be_bytes([data[72], data[73]]);
    (port != 0).then(|| valid_voice_udp_ip(ip, false)).flatten().map(|ip| (ip.to_string(), port))
}

struct State {
    cipher: Option<PacketCipher>,
    sequence: u16,
    timestamp: u32,
    packet: Vec<u8>,
}

impl State {
    #[inline]
    fn seal_payload(&mut self, ssrc: u32, payload: &[u8]) -> Result<&[u8], VoiceError> {
        let rtp = header(self.sequence, self.timestamp, ssrc);
        let cipher = self.cipher.as_mut().ok_or(VoiceError::NotReady)?;
        let counter = cipher.next_counter()?;
        cipher.seal_into(&mut self.packet, &rtp, counter, payload)?;
        self.advance();
        Ok(&self.packet)
    }

    #[inline]
    fn seal_frame(&mut self, ssrc: u32, frame: &mut Vec<u8>) -> Result<(), VoiceError> {
        let rtp = header(self.sequence, self.timestamp, ssrc);
        self.cipher.as_mut().ok_or(VoiceError::NotReady)?.seal_in_place(frame, &rtp)?;
        self.advance();
        Ok(())
    }

    #[inline]
    fn advance(&mut self) {
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES);
    }
}

pub struct Transport {
    socket: Arc<tokio::net::UdpSocket>,
    remote: SocketAddr,
    // BSD/macOS reject sendto with an address on a connected UDP socket (EISCONN).
    connected: bool,
    ssrc: u32,
    state: Mutex<State>,
}

#[inline]
fn lock_error<T>(_: T) -> VoiceError {
    VoiceError::Crypto("transport lock poisoned".into())
}

impl Transport {
    pub fn new(socket: Arc<tokio::net::UdpSocket>, remote: SocketAddr, ssrc: u32) -> Self {
        let connected = socket.peer_addr().ok() == Some(remote);
        Self {
            socket,
            remote,
            connected,
            ssrc,
            state: Mutex::new(State {
                cipher: None,
                sequence: 0,
                timestamp: 0,
                packet: Vec::with_capacity(1500),
            }),
        }
    }

    pub fn describe(&self, mode: Mode, key: [u8; 32]) -> Result<(), VoiceError> {
        let key = Zeroizing::new(key);
        let mut state = self.state.lock().map_err(lock_error)?;
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
        let mut state = self.state.lock().map_err(lock_error)?;
        let packet = state.seal_payload(self.ssrc, payload)?;
        send_udp(&self.socket, self.remote, self.connected, packet)
    }

    /// Seals `frame[HEADER_LEN..]` in place after writing the 12-byte RTP header into
    /// `frame[..HEADER_LEN]`, appends the 16-byte AEAD tag + 4-byte nonce suffix, and sends `&frame`.
    pub fn write_frame_in_place(&self, frame: &mut Vec<u8>) -> Result<(), VoiceError> {
        let mut state = self.state.lock().map_err(lock_error)?;
        state.seal_frame(self.ssrc, frame)?;
        send_udp(&self.socket, self.remote, self.connected, frame)
    }

    pub fn write_frame_in_place_mut(&mut self, frame: &mut Vec<u8>) -> Result<(), VoiceError> {
        self.state.get_mut().map_err(lock_error)?.seal_frame(self.ssrc, frame)?;
        send_udp(&self.socket, self.remote, self.connected, frame)
    }
}

#[inline]
fn try_send_once(
    socket: &tokio::net::UdpSocket,
    remote: SocketAddr,
    connected: bool,
    packet: &[u8],
) -> std::io::Result<usize> {
    if connected {
        socket.try_send(packet)
    } else {
        socket.try_send_to(packet, remote)
    }
}

#[inline]
fn send_udp(
    socket: &tokio::net::UdpSocket,
    remote: SocketAddr,
    connected: bool,
    packet: &[u8],
) -> Result<(), VoiceError> {
    let sent = match try_send_once(socket, remote, connected, packet) {
        Err(err) if retryable_udp_send_error(&err) => {
            try_send_once(socket, remote, connected, packet)
        }
        other => other,
    };
    match sent {
        Err(err) if transient_udp_error(&err) => Ok(()),
        result => result.map(drop).map_err(VoiceError::from),
    }
}

pub(crate) fn retryable_udp_send_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(
        error.kind(),
        ConnectionRefused | ConnectionReset | HostUnreachable | NetworkUnreachable | Interrupted
    ) || (cfg!(windows) && matches!(error.raw_os_error(), Some(10052 | 10054)))
}

/// Errors a voice UDP socket reports for one lost datagram rather than a dead socket.
pub(crate) fn transient_udp_error(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    retryable_udp_send_error(error)
        || matches!(error.kind(), WouldBlock | NetworkDown)
        || (cfg!(windows) && matches!(error.raw_os_error(), Some(10040 | 10050 | 10055 | 10064)))
        || (cfg!(target_os = "linux") && matches!(error.raw_os_error(), Some(105 | 112)))
        || (cfg!(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd"
        )) && matches!(error.raw_os_error(), Some(55 | 64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::Aead;

    #[test]
    fn both_modes_authenticate_headers_and_roundtrip_payloads() {
        let (key, rtp) = ([73; 32], header(7, 960, 12345));
        for mode in [Mode::AeadAes256GcmRtpSize, Mode::AeadXChaCha20Poly1305RtpSize] {
            let mut packet = vec![];
            PacketCipher::new(mode, &key)
                .seal_into(&mut packet, &rtp, 0x12345678, b"encrypted DAVE frame")
                .unwrap();
            let suffix = &packet[packet.len() - 4..];
            assert_eq!(suffix, &0x12345678u32.to_be_bytes());
            let decrypt = |aad: &[u8]| {
                let payload =
                    chacha20poly1305::aead::Payload { msg: &packet[12..packet.len() - 4], aad };
                match mode {
                    Mode::AeadAes256GcmRtpSize => {
                        let mut nonce = [0u8; 12];
                        nonce[..4].copy_from_slice(suffix);
                        Aes256Gcm::new((&key).into()).decrypt(Nonce::from_slice(&nonce), payload)
                    }
                    Mode::AeadXChaCha20Poly1305RtpSize => {
                        let mut nonce = [0u8; 24];
                        nonce[..4].copy_from_slice(suffix);
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
        assert_eq!(Mode::negotiate(["plain"]), None);
        assert_eq!(Mode::negotiate(["aead_aes256_gcm_rtpsize"]), Some(Mode::AeadAes256GcmRtpSize));
        assert_eq!(
            Mode::negotiate(["aead_xchacha20_poly1305_rtpsize"]),
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
            let ptr = packet.as_ptr();
            let rtp = header(1, 960, 77);
            cipher.seal_into(&mut packet, &rtp, 1, b"opus").unwrap();
            let first = packet.clone();
            cipher.seal_into(&mut packet, &rtp, 2, b"opus").unwrap();
            assert_eq!(packet.as_ptr(), ptr);
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
        transport.state.lock().unwrap().cipher.as_mut().unwrap().counter = u32::MAX as u64 + 1;
        assert!(matches!(transport.write(b"opus"), Err(VoiceError::Crypto(_))));
        let mut frame = vec![0u8; HEADER_LEN + 4];
        assert!(matches!(transport.write_frame_in_place(&mut frame), Err(VoiceError::Crypto(_))));
    }

    #[test]
    fn seal_in_place_matches_seal_into_without_reallocating() {
        let (key, payload) = ([55; 32], b"dave-encrypted-opus-payload");
        for mode in [Mode::AeadAes256GcmRtpSize, Mode::AeadXChaCha20Poly1305RtpSize] {
            let ref_cipher = PacketCipher::new(mode, &key);
            let mut inplace_cipher = PacketCipher::new(mode, &key);
            let (mut expected, mut frame) = (Vec::with_capacity(1500), Vec::with_capacity(1500));
            let ptr = frame.as_ptr();
            assert!(inplace_cipher
                .seal_in_place(&mut vec![0u8; HEADER_LEN - 1], &header(0, 0, 1))
                .is_err());
            for seq in 0u16..5 {
                let rtp = header(seq, u32::from(seq) * FRAME_SAMPLES, 999);
                ref_cipher.seal_into(&mut expected, &rtp, u32::from(seq), payload).unwrap();
                frame.clear();
                frame.extend_from_slice(&[0u8; HEADER_LEN]);
                frame.extend_from_slice(payload);
                inplace_cipher.seal_in_place(&mut frame, &rtp).unwrap();
                assert_eq!(frame, expected);
                assert_eq!(frame.as_ptr(), ptr);
            }
        }
    }

    #[tokio::test]
    async fn write_frame_in_place_delivers_identical_wire_packets() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket1 = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let socket2 = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let remote = receiver.local_addr().unwrap();
        for s in [&socket1, &socket2] {
            s.connect(remote).await.unwrap();
            s.writable().await.unwrap();
        }

        let legacy = Transport::new(socket1, remote, 42);
        let zero_copy = Transport::new(socket2, remote, 42);
        legacy.describe(Mode::AeadXChaCha20Poly1305RtpSize, [7; 32]).unwrap();
        zero_copy.describe(Mode::AeadXChaCha20Poly1305RtpSize, [7; 32]).unwrap();

        legacy.write(b"opus-frame").unwrap();
        let mut buf1 = [0u8; 128];
        let len1 = receiver.recv(&mut buf1).await.unwrap();

        let mut frame = Vec::with_capacity(1500);
        let ptr = frame.as_ptr();
        frame.extend_from_slice(&[0u8; HEADER_LEN]);
        frame.extend_from_slice(b"opus-frame");
        zero_copy.write_frame_in_place(&mut frame).unwrap();
        assert_eq!(frame.as_ptr(), ptr);
        let mut buf2 = [0u8; 128];
        let len2 = receiver.recv(&mut buf2).await.unwrap();

        assert_eq!(&buf1[..len1], &buf2[..len2]);
    }

    #[test]
    fn canonical_ip_validation_blocks_zero_network_and_ssrf_ranges() {
        for always_bad in [
            "0.0.0.0",
            "0.1.2.3",
            "::ffff:0.0.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::ffff:255.255.255.255",
            "::",
            "ff02::1",
        ] {
            let ip: IpAddr = always_bad.parse().unwrap();
            assert_eq!(valid_voice_udp_ip(ip, false), None, "{always_bad}");
            assert_eq!(valid_voice_udp_ip(ip, true), None, "{always_bad}");
        }

        assert_eq!(
            valid_voice_udp_ip("::ffff:127.0.0.1".parse().unwrap(), false),
            Some("127.0.0.1".parse().unwrap())
        );
        for non_public in [
            "127.0.0.1",
            "::ffff:127.0.0.1",
            "10.0.0.1",
            "::ffff:10.0.0.1",
            "172.16.5.4",
            "192.168.1.1",
            "169.254.169.254",
            "::ffff:169.254.169.254",
            "100.64.0.1",
            "100.127.255.254",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "198.18.0.1",
            "198.19.255.255",
            "192.0.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "2001:db8::1",
            "2001:2::1",
            "100::1",
            "64:ff9b::1",
        ] {
            assert_eq!(valid_voice_udp_ip(non_public.parse().unwrap(), true), None, "{non_public}");
        }

        for public_ip in ["162.159.130.232", "2606:4700:4700::1111"] {
            let ip: IpAddr = public_ip.parse().unwrap();
            assert_eq!(valid_voice_udp_ip(ip, true), Some(ip));
        }

        let mut discovery = ip_discovery_request(55);
        discovery[..2].copy_from_slice(&2u16.to_be_bytes());
        discovery[8..22].copy_from_slice(b"::ffff:0.1.2.3");
        discovery[72..].copy_from_slice(&5000u16.to_be_bytes());
        assert!(parse_ip_discovery_response(&discovery, 55).is_none());
    }

    #[test]
    fn transient_and_retryable_udp_errors_cover_network_down_and_icmp() {
        assert!(transient_udp_error(&std::io::ErrorKind::NetworkDown.into()));
        assert!(retryable_udp_send_error(&std::io::ErrorKind::ConnectionRefused.into()));
        assert!(retryable_udp_send_error(&std::io::ErrorKind::Interrupted.into()));
        assert!(!retryable_udp_send_error(&std::io::ErrorKind::WouldBlock.into()));
        #[cfg(windows)]
        for code in [10050, 10055, 10064] {
            assert!(transient_udp_error(&std::io::Error::from_raw_os_error(code)));
        }
        #[cfg(target_os = "linux")]
        for code in [105, 112] {
            assert!(transient_udp_error(&std::io::Error::from_raw_os_error(code)));
        }
    }

    #[tokio::test]
    async fn connected_transport_reaches_its_peer_and_survives_an_unreachable_one() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let remote = receiver.local_addr().unwrap();
        socket.connect(remote).await.unwrap();
        socket.writable().await.unwrap();
        let transport = Transport::new(socket, remote, 9);
        assert!(transport.connected);
        transport.describe(Mode::AeadAes256GcmRtpSize, [1; 32]).unwrap();
        transport.write(b"opus").unwrap();
        let mut bytes = [0u8; 64];
        let size =
            tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv(&mut bytes))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(&bytes[8..12], &9u32.to_be_bytes());
        assert_eq!(size, HEADER_LEN + 4 + 16 + 4);
        drop(receiver);
        for _ in 0..5 {
            transport.write(b"opus").unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(transient_udp_error(&std::io::ErrorKind::WouldBlock.into()));
    }
}
