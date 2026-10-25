//! Real loopback WebSocket/UDP peers; these tests do not contact Discord.
use super::*;
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use davey::{DaveSession, MediaType, ProposalsOperationType};
use openmls::prelude::{tls_codec::Serialize, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use rustlink_audio::{opus_decode::Decoder, Encoder};
use std::num::NonZeroU16;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};

const USER: u64 = 123456789;
const PEER: u64 = 987654321;
const CHANNEL: u64 = 555555555;
const SSRC: u32 = 77;
const KEY: [u8; 32] = [29; 32];
const SILENCE: &[u8] = &[0xf8, 0xff, 0xfe];
const LIMIT: Duration = Duration::from_secs(5);
const AES_MODE: &str = "aead_aes256_gcm_rtpsize";
const XCHACHA_MODE: &str = "aead_xchacha20_poly1305_rtpsize";

fn wire_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::AeadAes256GcmRtpSize => AES_MODE,
        Mode::AeadXChaCha20Poly1305RtpSize => XCHACHA_MODE,
    }
}

fn state() -> VoiceState {
    VoiceState {
        token: "loopback-only token".into(),
        session_id: "loopback-only session".into(),
        endpoint: "unused.discord.media".into(),
        channel_id: Some(CHANNEL.to_string()),
    }
}

struct Peer {
    ws: WebSocketStream<TcpStream>,
    udp: UdpSocket,
    mode: Mode,
    key: [u8; 32],
    dave: Option<DaveSession>,
    heartbeats: usize,
    speaking: Vec<bool>,
}

impl Peer {
    async fn transport(listener: &TcpListener, mode: Mode, key: [u8; 32]) -> Self {
        let (tcp, _) = tokio::time::timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut peer = Self { ws, udp, mode, key, dave: None, heartbeats: 0, speaking: vec![] };
        peer.send(json!({"op":8,"d":{"heartbeat_interval":200}})).await;
        let identify = peer.control(0).await;
        assert_eq!(identify["d"]["user_id"], USER.to_string());
        assert_eq!(identify["d"]["max_dave_protocol_version"], 1);
        let offered_mode = wire_mode(mode);
        peer.send(json!({"op":2,"seq":1,"d":{"ssrc":SSRC,"ip":"127.0.0.1","port":peer.udp.local_addr().unwrap().port(),"modes":[offered_mode]}})).await;
        let mut discovery = [0u8; 74];
        let (size, remote) =
            tokio::time::timeout(LIMIT, peer.udp.recv_from(&mut discovery)).await.unwrap().unwrap();
        assert_eq!(size, 74);
        assert_eq!(&discovery[..8], &[0, 1, 0, 70, 0, 0, 0, 77]);
        discovery[..2].copy_from_slice(&2u16.to_be_bytes());
        discovery[8..17].copy_from_slice(b"127.0.0.1");
        discovery[72..].copy_from_slice(&remote.port().to_be_bytes());
        peer.udp.send_to(&discovery, remote).await.unwrap();
        let selected = peer.control(1).await;
        assert_eq!(selected["d"]["data"]["mode"], offered_mode);
        peer
    }

    async fn send(&mut self, value: Value) {
        self.ws.send(Message::Text(value.to_string().into())).await.unwrap();
    }

    async fn message(&mut self) -> Message {
        tokio::time::timeout(LIMIT, self.ws.next()).await.unwrap().unwrap().unwrap()
    }

    async fn handle(&mut self, message: Message) {
        match message {
            Message::Text(text) => {
                let value: Value = serde_json::from_str(&text).unwrap();
                match value["op"].as_u64().unwrap() {
                    3 => {
                        self.heartbeats += 1;
                        self.send(json!({"op":6,"d":{"t":value["d"]["t"]}})).await;
                    }
                    5 => self.speaking.push(value["d"]["speaking"] == 1),
                    opcode => panic!("unexpected client opcode {opcode}"),
                }
            }
            Message::Ping(payload) => self.ws.send(Message::Pong(payload)).await.unwrap(),
            other => panic!("unexpected client message {other:?}"),
        }
    }

    async fn control(&mut self, opcode: u64) -> Value {
        loop {
            let message = self.message().await;
            if let Message::Text(text) = &message {
                let value: Value = serde_json::from_str(text).unwrap();
                if value["op"] == opcode {
                    return value;
                }
            }
            self.handle(message).await;
        }
    }

    async fn describe(&mut self, version: u16) {
        self.send(json!({"op":4,"seq":3,"d":{"mode":wire_mode(self.mode),"secret_key":self.key,"audio_codec":"opus","dave_protocol_version":version}})).await;
    }

    async fn enable_dave(&mut self) {
        let suite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
        let signer = SignatureKeyPair::new(suite.signature_algorithm()).unwrap();
        let external = ExternalSender::new(
            signer.public().into(),
            BasicCredential::new(b"loopback Discord peer".to_vec()).into(),
        )
        .tls_serialize_detached()
        .unwrap();
        let mut message = vec![0, 2, 25];
        message.extend(&external);
        self.ws.send(Message::Binary(message.into())).await.unwrap();
        self.describe(1).await;
        let package = loop {
            let message = self.message().await;
            if let Message::Binary(bytes) = &message {
                assert_eq!(bytes[0], 26);
                let message = MlsMessageIn::tls_deserialize_exact_bytes(&bytes[1..]).unwrap();
                let MlsMessageBodyIn::KeyPackage(package) = message.extract() else {
                    panic!("expected key package")
                };
                break package;
            }
            self.handle(message).await;
        };
        let provider = OpenMlsRustCrypto::default();
        let package = package.validate(provider.crypto(), ProtocolVersion::Mls10).unwrap();
        let mut dave = DaveSession::new(NonZeroU16::new(1).unwrap(), PEER, CHANNEL, None).unwrap();
        dave.set_external_sender(&external).unwrap();
        let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            package,
            GroupId::from_slice(&CHANNEL.to_be_bytes()),
            GroupEpoch::from(0),
            &signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        let proposal = VLBytes::new(proposal.tls_serialize_detached().unwrap())
            .tls_serialize_detached()
            .unwrap();
        let commit = dave
            .process_proposals(ProposalsOperationType::APPEND, &proposal, Some(&[USER, PEER]))
            .unwrap()
            .unwrap();
        dave.process_commit(&commit.commit).unwrap();
        self.send(json!({"op":11,"seq":4,"d":{"user_ids":[USER.to_string(),PEER.to_string()]}}))
            .await;
        let mut welcome = vec![0, 5, 30, 0, 8];
        welcome.extend(commit.welcome.unwrap());
        self.ws.send(Message::Binary(welcome.into())).await.unwrap();
        assert_eq!(self.control(23).await["d"]["transition_id"], 8);
        self.send(json!({"op":22,"seq":6,"d":{"transition_id":8}})).await;
        self.dave = Some(dave);
    }

    async fn packet(&mut self, sequence: u16) -> Vec<u8> {
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + LIMIT;
        let bytes = loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => panic!("timed out receiving UDP frame {sequence}"),
                packet = self.udp.recv_from(&mut buffer) => {
                    let (size, _) = packet.unwrap();
                    break buffer[..size].to_vec();
                }
                message = self.ws.next() => self.handle(message.unwrap().unwrap()).await,
            }
        };
        assert_eq!(&bytes[..2], &[0x80, 0x78]);
        assert_eq!(u16::from_be_bytes(bytes[2..4].try_into().unwrap()), sequence);
        assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), u32::from(sequence) * 960);
        assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), SSRC);
        let size = bytes.len();
        let counter = &bytes[size - 4..];
        assert_eq!(u32::from_be_bytes(counter.try_into().unwrap()), u32::from(sequence));
        let payload = Payload { msg: &bytes[12..size - 4], aad: &bytes[..12] };
        let clear = match self.mode {
            Mode::AeadAes256GcmRtpSize => {
                let mut nonce = [0; 12];
                nonce[..4].copy_from_slice(counter);
                Aes256Gcm::new((&self.key).into())
                    .decrypt(Nonce::from_slice(&nonce), payload)
                    .unwrap()
            }
            Mode::AeadXChaCha20Poly1305RtpSize => {
                let mut nonce = [0; 24];
                nonce[..4].copy_from_slice(counter);
                XChaCha20Poly1305::new((&self.key).into())
                    .decrypt(XNonce::from_slice(&nonce), payload)
                    .unwrap()
            }
        };
        match self.dave.as_mut() {
            Some(dave) => dave.decrypt(USER, MediaType::AUDIO, &clear).unwrap().to_vec(),
            None => clear,
        }
    }

    async fn remote_close(&mut self) {
        self.ws
            .close(Some(CloseFrame {
                code: CloseCode::from(4014),
                reason: "simulated voice move".into(),
            }))
            .await
            .unwrap();
    }
}

fn audio_packets() -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new(192_000, 10).unwrap();
    (0..50)
        .map(|frame| {
            let mut pcm = Vec::with_capacity(1920);
            for sample in 0..960 {
                let time = (frame * 960 + sample) as f64 / 48_000.0;
                pcm.push((14_000.0 * (time * 220.0 * std::f64::consts::TAU).sin()) as i16);
                pcm.push((9_000.0 * (time * 880.0 * std::f64::consts::TAU).sin()) as i16);
            }
            encoder.encode(&pcm).unwrap().to_vec()
        })
        .collect()
}

#[tokio::test]
async fn both_transports_preserve_stereo_opus_through_dave_pause_and_resume() {
    for mode in [Mode::AeadAes256GcmRtpSize, Mode::AeadXChaCha20Poly1305RtpSize] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let packets = audio_packets();
        let mut expected = packets[..25].to_vec();
        expected.extend(vec![SILENCE.to_vec(); 5]);
        expected.extend_from_slice(&packets[25..]);
        expected.extend(vec![SILENCE.to_vec(); 5]);
        let server = tokio::spawn(async move {
            let mut peer = Peer::transport(&listener, mode, KEY).await;
            peer.enable_dave().await;
            let mut actual_decoder = Decoder::new().unwrap();
            let mut reference_decoder = Decoder::new().unwrap();
            let mut actual = Vec::new();
            let mut reference = Vec::new();
            let mut samples = 0;
            let mut energy = [0.0f64; 2];
            for (index, expected) in expected.iter().enumerate() {
                let actual_packet = peer.packet(index as u16).await;
                assert_eq!(&actual_packet, expected, "transport changed Opus packet {index}");
                samples += actual_decoder.decode(&actual_packet, &mut actual).unwrap();
                reference_decoder.decode(expected, &mut reference).unwrap();
                assert_eq!(actual, reference, "decoded channel data changed at packet {index}");
                for pair in actual.as_chunks::<2>().0 {
                    for channel in 0..2 {
                        energy[channel] += f64::from(pair[channel]).powi(2);
                    }
                }
            }
            while peer.speaking.len() < 4 {
                let message = peer.message().await;
                peer.handle(message).await;
            }
            assert_eq!(peer.speaking, [true, false, true, false]);
            assert!(peer.heartbeats >= 2, "audio must coexist with acknowledged heartbeats");
            assert_eq!(samples, 57_600);
            assert!(energy.iter().all(|value| (*value / samples as f64).sqrt() > 1000.0));
            eprintln!("loopback mode={} packets=60 samples_per_channel={} heartbeat_acks={} left_rms={:.2} right_rms={:.2}", mode.name(), samples, peer.heartbeats, (energy[0] / samples as f64).sqrt(), (energy[1] / samples as f64).sqrt());
            peer.remote_close().await;
        });
        let (close, mut closed) = mpsc::unbounded_channel();
        let connection = connect_url(
            &url,
            &USER.to_string(),
            "1111",
            &state(),
            Box::new(move |event| {
                close.send(event).unwrap();
            }),
        )
        .await
        .unwrap();
        connection.speaking(true).unwrap();
        for (index, packet) in packets.iter().enumerate() {
            if index == 25 {
                connection.speaking(false).unwrap();
                tokio::time::sleep(Duration::from_millis(80)).await;
                connection.speaking(true).unwrap();
            }
            connection.write(packet).unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        connection.speaking(false).unwrap();
        server.await.unwrap();
        let closed_event = tokio::time::timeout(LIMIT, closed.recv()).await.unwrap().unwrap();
        assert_eq!(closed_event.code, 4014);
        assert_eq!(closed_event.reason, "simulated voice move");
        assert!(closed_event.by_remote);
        assert!(!connection.connected());
        assert!(connection.write(SILENCE).is_err());
        assert!(closed.try_recv().is_err());
    }
}

#[tokio::test]
async fn duplicate_description_and_wrong_heartbeat_ack_close_established_connections() {
    for duplicate in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut peer = Peer::transport(&listener, Mode::AeadAes256GcmRtpSize, KEY).await;
            peer.describe(0).await;
            if duplicate {
                peer.describe(0).await;
            }
            loop {
                match peer.message().await {
                    Message::Close(_) => break,
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).unwrap();
                        if value["op"] == 3 {
                            let wrong = value["d"]["t"].as_i64().unwrap() + 1;
                            peer.send(json!({"op":6,"d":{"t":wrong}})).await;
                        }
                    }
                    other => panic!("unexpected close-test message {other:?}"),
                }
            }
        });
        let (close, mut closed) = mpsc::unbounded_channel();
        let connection = connect_url(
            &url,
            &USER.to_string(),
            "1111",
            &state(),
            Box::new(move |event| {
                close.send(event).unwrap();
            }),
        )
        .await
        .unwrap();
        let event = tokio::time::timeout(LIMIT, closed.recv()).await.unwrap().unwrap();
        assert_eq!(event.code, 1006);
        assert!(event.reason.contains(if duplicate {
            "already installed"
        } else {
            "not acknowledged"
        }));
        assert!(!connection.connected());
        assert_eq!(connection.ping(), -1);
        server.await.unwrap();
        assert!(closed.try_recv().is_err());
    }
}

#[tokio::test]
async fn local_close_is_idempotent_and_fresh_connect_uses_a_new_transport_key() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let (received, mut deliveries) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for round in 0..2 {
            let mut peer =
                Peer::transport(&listener, Mode::AeadAes256GcmRtpSize, [29 + round; 32]).await;
            peer.describe(0).await;
            assert_eq!(peer.packet(0).await, SILENCE);
            received.send(()).unwrap();
            loop {
                let message = peer.message().await;
                if matches!(message, Message::Close(_)) {
                    break;
                }
                peer.handle(message).await;
            }
        }
    });
    for _ in 0..2 {
        let (close, mut closed) = mpsc::unbounded_channel();
        let connection = connect_url(
            &url,
            &USER.to_string(),
            "1111",
            &state(),
            Box::new(move |event| {
                close.send(event).unwrap();
            }),
        )
        .await
        .unwrap();
        connection.speaking(true).unwrap();
        connection.write(SILENCE).unwrap();
        tokio::time::timeout(LIMIT, deliveries.recv()).await.unwrap().unwrap();
        connection.close();
        connection.close();
        let event = tokio::time::timeout(LIMIT, closed.recv()).await.unwrap().unwrap();
        assert_eq!(event.code, 1000);
        assert!(!event.by_remote);
        assert!(!connection.connected());
        assert!(closed.try_recv().is_err());
    }
    server.await.unwrap();
}

#[tokio::test]
async fn premature_dave_downgrade_fails_the_loopback_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.send(Message::Text(
            json!({"op":21,"d":{"protocol_version":0,"transition_id":0}}).to_string().into(),
        ))
        .await
        .unwrap();
        while let Some(Ok(_)) = ws.next().await {}
    });
    let result = connect_url(
        &url,
        &USER.to_string(),
        "1111",
        &state(),
        Box::new(|_| panic!("failed handshake is not an established connection")),
    )
    .await;
    assert!(
        matches!(result, Err(VoiceError::Crypto(reason)) if reason.contains("before session description"))
    );
    server.await.unwrap();
}
