use super::*;
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use davey::{DaveSession, MediaType, ProposalsOperationType};
use openmls::prelude::{tls_codec::Serialize, *};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use std::num::NonZeroU16;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

type ServerSocket = WebSocketStream<TcpStream>;
const USER: u64 = 123456789;
const PEER: u64 = 987654321;
const CHANNEL: u64 = 555555555;
const SSRC: u32 = 77;
const KEY: [u8; 32] = [29; 32];
const OPUS: &[u8] = &[0xf8, 0x11, 0x22, 0x33, 0x44];

#[test]
fn audio_backpressure_is_bounded_and_recycled_buffers_do_not_retain_payloads() {
    let (commands, mut receiver) = mpsc::channel(COMMAND_CAPACITY);
    let connection = DiscordConnection {
        commands,
        ready: AtomicBool::new(true),
        closed: AtomicBool::new(false),
        ping: AtomicI64::new(-1),
        audio_buffers: Mutex::new(Vec::with_capacity(COMMAND_CAPACITY)),
    };
    for _ in 0..COMMAND_CAPACITY {
        connection.write(OPUS).unwrap();
    }
    assert!(connection.write(OPUS).is_err());
    assert_eq!(connection.audio_buffers.lock().unwrap().len(), 1);
    let Command::Audio(packet) = receiver.try_recv().unwrap() else { panic!("expected audio") };
    assert_eq!(packet, OPUS);
    let pointer = packet.as_ptr();
    connection.recycle(packet);
    let reused = connection.packet(b"new");
    assert_eq!(reused, b"new");
    assert_eq!(reused.as_ptr(), pointer);
    connection.recycle(reused);
    for _ in 0..COMMAND_CAPACITY * 2 {
        connection.recycle(Vec::with_capacity(1500));
    }
    assert_eq!(connection.audio_buffers.lock().unwrap().len(), COMMAND_CAPACITY);
    connection.close();
    assert!(matches!(connection.write(OPUS), Err(VoiceError::NotReady)));
}

fn state() -> VoiceState {
    VoiceState {
        token: "test voice token".into(),
        endpoint: "unused.discord.media".into(),
        session_id: "test session".into(),
        channel_id: Some(CHANNEL.to_string()),
    }
}

async fn send(ws: &mut ServerSocket, value: Value) {
    ws.send(Message::Text(value.to_string().into())).await.unwrap();
}
async fn binary(ws: &mut ServerSocket, sequence: u16, opcode: u8, data: &[u8]) {
    let mut bytes = sequence.to_be_bytes().to_vec();
    bytes.push(opcode);
    bytes.extend(data);
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}
async fn next(ws: &mut ServerSocket) -> Message {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(ref text) = message {
            let value: Value = serde_json::from_str(text).unwrap();
            if value["op"] == 3 {
                assert!(value["d"]["seq_ack"].is_i64());
                send(ws, json!({"op":6,"d":{"t":value["d"]["t"]}})).await;
                continue;
            }
        }
        return message;
    }
}
async fn next_json(ws: &mut ServerSocket, opcode: u8) -> Value {
    let Message::Text(text) = next(ws).await else {
        panic!("expected JSON opcode {opcode}");
    };
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["op"], opcode);
    value
}

#[tokio::test]
async fn v8_handshake_waits_for_dave_and_sends_interoperable_encrypted_audio() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/?v=8", listener.local_addr().unwrap());
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (prepared, waiting) = oneshot::channel();
    let (execute, can_execute) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        send(&mut ws, json!({"op":8,"d":{"heartbeat_interval":100}})).await;
        let identify = next_json(&mut ws, 0).await;
        assert_eq!(identify["d"]["max_dave_protocol_version"], 1);
        assert_eq!(identify["d"]["user_id"], USER.to_string());
        send(&mut ws,json!({"op":2,"seq":1,"d":{"ssrc":SSRC,"ip":"127.0.0.1","port":udp.local_addr().unwrap().port(),"modes":["aead_aes256_gcm_rtpsize"]}})).await;
        let suite = Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256;
        let signer = SignatureKeyPair::new(suite.signature_algorithm()).unwrap();
        let external = ExternalSender::new(
            signer.public().into(),
            BasicCredential::new(b"mock Discord".to_vec()).into(),
        )
        .tls_serialize_detached()
        .unwrap();
        binary(&mut ws, 2, 25, &external).await;
        let mut discovery = [0u8; 74];
        let (size, client) = udp.recv_from(&mut discovery).await.unwrap();
        assert_eq!(size, 74);
        assert_eq!(&discovery[..8], &transport::ip_discovery_request(SSRC)[..8]);
        // Delay discovery until a heartbeat acknowledges the binary opcode sequence.
        loop {
            let Message::Text(text) = ws.next().await.unwrap().unwrap() else {
                panic!("expected heartbeat");
            };
            let value: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(value["op"], 3);
            send(&mut ws, json!({"op":6,"d":{"t":value["d"]["t"]}})).await;
            if value["d"]["seq_ack"] == 2 {
                break;
            }
        }
        discovery[..2].copy_from_slice(&2u16.to_be_bytes());
        discovery[8..17].copy_from_slice(b"127.0.0.1");
        discovery[72..].copy_from_slice(&client.port().to_be_bytes());
        udp.send_to(&discovery, client).await.unwrap();
        let selected = next_json(&mut ws, 1).await;
        assert_eq!(selected["d"]["data"]["mode"], "aead_aes256_gcm_rtpsize");
        send(&mut ws,json!({"op":4,"seq":3,"d":{"mode":"aead_aes256_gcm_rtpsize","secret_key":KEY,"dave_protocol_version":1,"audio_codec":"opus"}})).await;
        let Message::Binary(bytes) = next(&mut ws).await else {
            panic!("expected key package");
        };
        assert_eq!(bytes[0], 26);
        let package = MlsMessageIn::tls_deserialize_exact_bytes(&bytes[1..]).unwrap();
        let MlsMessageBodyIn::KeyPackage(package) = package.extract() else {
            panic!("key package needs MLSMessage framing");
        };
        let provider = OpenMlsRustCrypto::default();
        let package = package.validate(provider.crypto(), ProtocolVersion::Mls10).unwrap();
        assert_eq!(package.leaf_node().credential().serialized_content(), &USER.to_be_bytes());
        let mut peer = DaveSession::new(NonZeroU16::new(1).unwrap(), PEER, CHANNEL, None).unwrap();
        peer.set_external_sender(&external).unwrap();
        let proposal = ExternalProposal::new_add::<OpenMlsRustCrypto>(
            package,
            GroupId::from_slice(&CHANNEL.to_be_bytes()),
            GroupEpoch::from(0),
            &signer,
            SenderExtensionIndex::new(0),
        )
        .unwrap();
        let proposals = VLBytes::new(proposal.tls_serialize_detached().unwrap())
            .tls_serialize_detached()
            .unwrap();
        let commit = peer
            .process_proposals(ProposalsOperationType::APPEND, &proposals, Some(&[USER, PEER]))
            .unwrap()
            .unwrap();
        peer.process_commit(&commit.commit).unwrap();
        send(
            &mut ws,
            json!({"op":11,"seq":4,"d":{"user_ids":[PEER.to_string(),USER.to_string()]}}),
        )
        .await;
        let mut welcome = 8u16.to_be_bytes().to_vec();
        welcome.extend(commit.welcome.unwrap());
        binary(&mut ws, 5, 30, &welcome).await;
        assert_eq!(next_json(&mut ws, 23).await["d"]["transition_id"], 8);
        prepared.send(()).unwrap();
        can_execute.await.unwrap();
        send(&mut ws, json!({"op":22,"seq":6,"d":{"transition_id":8}})).await;
        assert_eq!(next_json(&mut ws, 5).await["d"]["speaking"], 1);
        let mut packet = [0u8; 2048];
        let (size, _) = tokio::time::timeout(Duration::from_secs(5), udp.recv_from(&mut packet))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&packet[8..12], &SSRC.to_be_bytes());
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&packet[size - 4..size]);
        let frame = Aes256Gcm::new((&KEY).into())
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload { msg: &packet[12..size - 4], aad: &packet[..12] },
            )
            .unwrap();
        assert_ne!(frame, OPUS);
        assert_eq!(peer.decrypt(USER, MediaType::AUDIO, &frame).unwrap(), OPUS);
        assert_eq!(next_json(&mut ws, 5).await["d"]["speaking"], 0);
        for _ in 0..5 {
            let (size, _) =
                tokio::time::timeout(Duration::from_secs(5), udp.recv_from(&mut packet))
                    .await
                    .unwrap()
                    .unwrap();
            nonce[..4].copy_from_slice(&packet[size - 4..size]);
            let silence = Aes256Gcm::new((&KEY).into())
                .decrypt(
                    Nonce::from_slice(&nonce),
                    Payload { msg: &packet[12..size - 4], aad: &packet[..12] },
                )
                .unwrap();
            assert_eq!(peer.decrypt(USER, MediaType::AUDIO, &silence).unwrap(), [0xf8, 0xff, 0xfe]);
        }
        ws.close(None).await.unwrap();
    });
    let (closed, mut closes) = mpsc::unbounded_channel();
    let connection_task = tokio::spawn(async move {
        connect_url(
            &url,
            &USER.to_string(),
            "11111111",
            &state(),
            Box::new(move |reason| {
                closed.send(reason).unwrap();
            }),
        )
        .await
    });
    waiting.await.unwrap();
    assert!(!connection_task.is_finished(), "must wait for Execute Transition");
    execute.send(()).unwrap();
    let connection = connection_task.await.unwrap().unwrap();
    connection.speaking(true).unwrap();
    connection.write(OPUS).unwrap();
    connection.speaking(false).unwrap();
    server.await.unwrap();
    let reason =
        tokio::time::timeout(Duration::from_secs(5), closes.recv()).await.unwrap().unwrap();
    assert!(reason.by_remote);
    assert!(!connection.connected());
    assert!(connection.write(OPUS).is_err());
    assert!(closes.try_recv().is_err());
}

#[tokio::test]
async fn missing_heartbeat_ack_ends_the_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        send(&mut ws, json!({"op":8,"d":{"heartbeat_interval":20}})).await;
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
    assert!(matches!(result,Err(VoiceError::Gateway(ref s)) if s.contains("not acknowledged")));
    server.await.unwrap();
}

#[test]
fn malformed_transport_keys_are_rejected() {
    for value in [json!([1, 2]), json!(vec![256; 32]), json!(vec![Value::Null; 32])] {
        assert!(read_key(Some(&value)).is_err());
    }
    assert!(read_key(None).is_err());
    assert_eq!(*read_key(Some(&json!(KEY))).unwrap(), KEY);
}
