//! Discord voice gateway v8. A single task orders speaking, DAVE transitions and media.
use std::collections::VecDeque;
use std::future::{pending, Future};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rustlink_dave::{Outbound, Session};
use rustlink_protocol::VoiceState;
use serde_json::{json, Value};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Notify};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, protocol::WebSocketConfig, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zeroize::Zeroizing;

use crate::transport::{self, Mode, Transport};
use crate::{CloseHandler, Closed, Connection, VoiceError};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a DAVE session waits for a welcome before deciding it is alone.
const ALONE_GRACE: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_CAPACITY: usize = 32;
/// Slots of the command queue that audio frames never take. Speaking and Close
/// always find room there, however far behind the gateway task is.
const CONTROL_RESERVE: usize = 4;
const MAX_RETAINED_PACKET: usize = 4096;
const MAX_PENDING_OUTBOUND: usize = 64;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Probe = Pin<Box<dyn Future<Output = Result<Discovered, VoiceError>> + Send>>;

struct SharedState {
    ready: AtomicBool,
    closed: AtomicBool,
    closed_notify: Notify,
    ping: AtomicI64,
    audio_buffers: Mutex<Vec<Vec<u8>>>,
    /// Audio frames dropped because the gateway task was behind.
    dropped_frames: AtomicU64,
}

impl SharedState {
    fn packet(&self, payload: &[u8]) -> Vec<u8> {
        let mut packet = self
            .audio_buffers
            .lock()
            .ok()
            .and_then(|mut buffers| buffers.pop())
            .unwrap_or_default();
        let needed = (transport::HEADER_LEN + payload.len() + 16 + 20).max(1500);
        if packet.capacity() < needed {
            packet.reserve(needed - packet.len());
        }
        packet.extend_from_slice(&[0u8; transport::HEADER_LEN]);
        packet.extend_from_slice(payload);
        packet
    }

    fn recycle(&self, mut packet: Vec<u8>) {
        if packet.capacity() <= MAX_RETAINED_PACKET {
            packet.clear();
            if let Ok(mut buffers) = self.audio_buffers.lock() {
                if buffers.len() < COMMAND_CAPACITY {
                    buffers.push(packet);
                }
            }
        }
    }
}

pub struct DiscordConnection {
    commands: mpsc::Sender<Command>,
    shared: Arc<SharedState>,
}

enum Command {
    Audio(Vec<u8>),
    Speaking(bool),
    Close,
}

impl DiscordConnection {
    fn command(&self, command: Command) -> Result<(), VoiceError> {
        if !self.connected() {
            return Err(VoiceError::NotReady);
        }
        // Speaking and Close take the reserved slots, so they only fail when
        // the gateway task is gone.
        self.commands
            .try_send(command)
            .map_err(|_| VoiceError::Gateway("voice command queue unavailable".into()))
    }

    fn record_dropped_frame(&self) {
        let dropped = self.shared.dropped_frames.fetch_add(1, Ordering::Relaxed) + 1;
        if dropped == 1 || dropped.is_multiple_of(500) {
            log::warn!("voice gateway is behind; audio frames dropped={dropped}");
        }
    }

    fn drop_frame(&self, packet: Vec<u8>) {
        self.recycle(packet);
        self.record_dropped_frame();
    }

    #[cfg(test)]
    fn packet(&self, payload: &[u8]) -> Vec<u8> {
        self.shared.packet(payload)
    }

    fn recycle(&self, packet: Vec<u8>) {
        self.shared.recycle(packet);
    }
}

impl Drop for DiscordConnection {
    fn drop(&mut self) {
        self.close();
    }
}

impl Connection for DiscordConnection {
    fn write(&self, payload: &[u8]) -> Result<(), VoiceError> {
        if !self.connected() {
            return Err(VoiceError::NotReady);
        }
        if self.commands.is_closed() {
            return Err(VoiceError::Gateway("voice command queue unavailable".into()));
        }
        if self.commands.capacity() <= CONTROL_RESERVE {
            self.record_dropped_frame();
            return Ok(());
        }
        let packet = self.shared.packet(payload);
        match self.commands.try_reserve() {
            Ok(permit) if self.commands.capacity() >= CONTROL_RESERVE => {
                permit.send(Command::Audio(packet));
                Ok(())
            }
            Ok(_) | Err(mpsc::error::TrySendError::Full(())) => {
                self.drop_frame(packet);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                self.recycle(packet);
                Err(VoiceError::Gateway("voice command queue unavailable".into()))
            }
        }
    }
    fn speaking(&self, on: bool) -> Result<(), VoiceError> {
        self.command(Command::Speaking(on))
    }
    fn connected(&self) -> bool {
        self.shared.ready.load(Ordering::Acquire) && !self.shared.closed.load(Ordering::Acquire)
    }
    fn ping(&self) -> i64 {
        if self.connected() {
            self.shared.ping.load(Ordering::Relaxed)
        } else {
            -1
        }
    }
    fn close(&self) {
        if !self.shared.closed.swap(true, Ordering::AcqRel) {
            self.shared.ready.store(false, Ordering::Release);
            self.shared.closed_notify.notify_waiters();
            // If the bounded queue is full, the task notices closed_notify / closed immediately.
            let _ = self.commands.try_send(Command::Close);
        }
    }
}

struct Discovered {
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
    address: String,
    port: u16,
}

struct Gateway {
    url: String,
    resume_credentials: (String, String, Zeroizing<String>),
    stream: Socket,
    dave: Session,
    shared: Arc<SharedState>,
    commands: mpsc::Receiver<Command>,
    probe: Option<Probe>,
    socket: Option<Arc<UdpSocket>>,
    transport: Option<Transport>,
    ssrc: Option<u32>,
    mode: Option<Mode>,
    interval: Duration,
    heartbeat_sleep: Pin<Box<tokio::time::Sleep>>,
    has_heartbeat: bool,
    last_nonce: i64,
    outstanding: Option<(i64, Instant)>,
    sequence: i64,
    identify: Option<Value>,
    pending_outbound: VecDeque<Outbound>,
    resuming: bool,
    transient_fault: bool,
    speaking: bool,
    /// When to check whether the DAVE session is the call's only member.
    alone_at: Option<Instant>,
}

pub async fn connect(
    user: &str,
    guild: &str,
    state: &VoiceState,
    on_close: CloseHandler,
) -> Result<Arc<DiscordConnection>, VoiceError> {
    crate::validate::validate(state)?;
    if !crate::valid_id(user) || !crate::valid_id(guild) {
        return Err(VoiceError::Invalid("invalid voice user or guild ID".into()));
    }
    let url = format!("wss://{}/?v={}", state.endpoint, crate::GATEWAY_VERSION);
    connect_url(&url, user, guild, state, on_close).await
}

// Kept private: only production connect may choose an endpoint, after validation.
async fn connect_url(
    url: &str,
    user: &str,
    guild: &str,
    state: &VoiceState,
    on_close: CloseHandler,
) -> Result<Arc<DiscordConnection>, VoiceError> {
    let handshake = async {
        let config = WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        log::debug!("voice gateway connecting to {url}");
        let (stream, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), true)
            .await
            .map_err(gateway_error)?;
        log::debug!("voice gateway connected");
        let user_id =
            user.trim().parse().map_err(|_| VoiceError::Invalid("invalid user ID".into()))?;
        let channel = state
            .channel_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .parse()
            .map_err(|_| VoiceError::Invalid("invalid channel ID".into()))?;
        let dave = Session::new(user_id, channel).map_err(dave_error)?;
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let shared = Arc::new(SharedState {
            ready: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closed_notify: Notify::new(),
            ping: AtomicI64::new(-1),
            audio_buffers: Mutex::new(Vec::with_capacity(COMMAND_CAPACITY)),
            dropped_frames: AtomicU64::new(0),
        });
        let connection = Arc::new(DiscordConnection { commands, shared: shared.clone() });
        let mut gateway = Gateway {
            url: url.to_owned(),
            resume_credentials: (
                guild.to_owned(),
                state.session_id.clone(),
                Zeroizing::new(state.token.clone()),
            ),
            stream,
            dave,
            shared,
            commands: receiver,
            probe: None,
            socket: None,
            transport: None,
            ssrc: None,
            mode: None,
            interval: Duration::from_secs(5),
            heartbeat_sleep: Box::pin(tokio::time::sleep(HANDSHAKE_TIMEOUT)),
            has_heartbeat: false,
            last_nonce: 0,
            outstanding: None,
            sequence: -1,
            pending_outbound: VecDeque::new(),
            resuming: false,
            transient_fault: false,
            speaking: false,
            alone_at: None,
            identify: Some(
                json!({"op":0,"d":{"server_id":guild,"user_id":user,"session_id":state.session_id,"token":state.token,"max_dave_protocol_version":rustlink_dave::MAX_PROTOCOL_VERSION}}),
            ),
        };
        while !gateway.transport.as_ref().is_some_and(Transport::ready) || !gateway.dave.ready() {
            gateway.step().await?;
        }
        gateway.shared.ready.store(true, Ordering::Release);
        Ok::<_, VoiceError>((gateway, connection))
    };
    let (gateway, connection) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await.map_err(|_| {
            VoiceError::Gateway("voice handshake timed out (including DAVE negotiation)".into())
        })??;
    log::debug!("voice handshake finished (DAVE version {:?})", gateway.dave.active_version());
    tokio::spawn(gateway.run(on_close));
    Ok(connection)
}

impl Gateway {
    fn audio(&mut self, mut frame: Vec<u8>) -> Result<(), VoiceError> {
        let result = self
            .dave
            .encrypt_opus_in_place_from(&mut frame, transport::HEADER_LEN)
            .map_err(dave_error)
            .and_then(|()| {
                self.transport
                    .as_mut()
                    .ok_or(VoiceError::NotReady)?
                    .write_frame_in_place_mut(&mut frame)
            });
        self.shared.recycle(frame);
        result
    }

    fn ws_error(&mut self, error: tungstenite::Error) -> VoiceError {
        if is_transient_tungstenite_error(&error) {
            self.transient_fault = true;
        }
        VoiceError::Gateway(error.to_string())
    }

    fn transient_gateway_error(&mut self, msg: &'static str) -> VoiceError {
        self.transient_fault = true;
        VoiceError::Gateway(msg.into())
    }

    fn is_resumable(&self, error: &VoiceError) -> bool {
        match error {
            VoiceError::Closed { code, by_remote: true, .. } => is_resumable_close_code(*code),
            VoiceError::Io(_) => true,
            VoiceError::Gateway(_) => self.transient_fault,
            _ => false,
        }
    }

    async fn try_resume(&mut self) -> Result<(), VoiceError> {
        let _ = tokio::time::timeout(Duration::from_millis(250), self.stream.close(None)).await;
        let config = WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let connected =
            tokio_tungstenite::connect_async_with_config(&self.url, Some(config), true).await;
        let (stream, _) = connected.map_err(|err| {
            self.transient_fault = true;
            gateway_error(err)
        })?;
        self.stream = stream;
        self.outstanding = None;
        self.has_heartbeat = false;
        self.resuming = true;
        let (ref server_id, ref session_id, ref token) = self.resume_credentials;
        self.identify = Some(json!({
            "op": 7,
            "d": {
                "server_id": server_id,
                "session_id": session_id,
                "token": token.as_str(),
                "seq_ack": self.sequence,
            }
        }));
        while self.resuming {
            self.step().await?;
        }
        Ok(())
    }

    async fn run(mut self, on_close: CloseHandler) {
        let mut resume_attempts = 0u8;
        let error = loop {
            let step_result = self.step().await;
            match step_result {
                Ok(()) => {
                    resume_attempts = 0;
                }
                Err(mut error) => {
                    if !self.shared.closed.load(Ordering::Acquire) && self.is_resumable(&error) {
                        let mut resumed_ok = false;
                        while resume_attempts < 2 && !self.shared.closed.load(Ordering::Acquire) {
                            resume_attempts += 1;
                            let resume_result =
                                tokio::time::timeout(Duration::from_secs(10), self.try_resume())
                                    .await;
                            match resume_result {
                                Ok(Ok(())) => {
                                    log::debug!("voice gateway v8 session resumed");
                                    resumed_ok = true;
                                    break;
                                }
                                Ok(Err(resume_error)) => {
                                    self.resuming = false;
                                    self.identify = None;
                                    if self.shared.closed.load(Ordering::Acquire) {
                                        error = local_close();
                                        break;
                                    }
                                    if !self.is_resumable(&resume_error) {
                                        error = resume_error;
                                        break;
                                    }
                                    if matches!(resume_error, VoiceError::Closed { .. }) {
                                        error = resume_error;
                                    }
                                }
                                Err(_) => {
                                    self.resuming = false;
                                    self.identify = None;
                                }
                            }
                        }
                        if resumed_ok {
                            continue;
                        }
                    }
                    break error;
                }
            }
        };
        self.shared.ready.store(false, Ordering::Release);
        self.shared.closed.store(true, Ordering::Release);
        let closed = match error {
            VoiceError::Closed { code, reason, by_remote } => Closed { code, reason, by_remote },
            VoiceError::Io(_) | VoiceError::Invalid(_) | VoiceError::NotReady => {
                Closed { code: 1006, reason: error.to_string(), by_remote: false }
            }
            error => Closed { code: 1006, reason: error.to_string(), by_remote: true },
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), self.stream.close(None)).await;
        on_close(closed);
    }

    async fn send(&mut self, value: &impl serde::Serialize) -> Result<(), VoiceError> {
        let text = serde_json::to_string(value).map_err(gateway_error)?;
        self.send_text(text).await
    }

    async fn send_text(&mut self, text: String) -> Result<(), VoiceError> {
        self.send_message(Message::Text(text.into())).await
    }

    async fn send_message(&mut self, message: Message) -> Result<(), VoiceError> {
        let sent = tokio::time::timeout(Duration::from_secs(5), self.stream.send(message)).await;
        match sent {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(self.ws_error(err)),
            Err(_) => Err(self.transient_gateway_error("voice gateway write timed out")),
        }
    }

    fn enqueue_outbound(&mut self, messages: Vec<Outbound>) -> Result<(), VoiceError> {
        if self.pending_outbound.len().saturating_add(messages.len()) > MAX_PENDING_OUTBOUND {
            return Err(VoiceError::Gateway("excessive pending outbound gateway frames".into()));
        }
        self.pending_outbound.extend(messages);
        Ok(())
    }

    async fn flush_outbound(&mut self) -> Result<(), VoiceError> {
        loop {
            let ws_message = match self.pending_outbound.front() {
                Some(Outbound::Json(value)) => {
                    log::debug!("voice gateway sent op {}", value["op"]);
                    Message::Text(serde_json::to_string(value).map_err(gateway_error)?.into())
                }
                Some(Outbound::Binary(bytes)) => {
                    log::debug!(
                        "voice gateway sent binary op {} ({} bytes)",
                        bytes.first().copied().unwrap_or_default(),
                        bytes.len()
                    );
                    Message::Binary(bytes.clone().into())
                }
                None => break,
            };
            self.send_message(ws_message).await?;
            self.pending_outbound.pop_front();
        }
        Ok(())
    }

    async fn step(&mut self) -> Result<(), VoiceError> {
        self.transient_fault = false;
        let shared = Arc::clone(&self.shared);
        let closed = shared.closed_notify.notified();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if shared.closed.load(Ordering::Acquire) {
            return Err(local_close());
        }
        tokio::select! {
            biased;
            _ = &mut closed => return Err(local_close()),
            message = self.stream.next() => {
                let raw = match message {
                    Some(Ok(msg)) => msg,
                    Some(Err(err)) => return Err(self.ws_error(err)),
                    None => return Err(self.transient_gateway_error("voice gateway hung up")),
                };
                match raw {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).map_err(gateway_error)?;
                        let next_seq = match value.get("seq").filter(|v| !v.is_null()) {
                            Some(sequence) => Some(
                                sequence
                                    .as_u64()
                                    .filter(|n| *n <= u16::MAX as u64)
                                    .ok_or_else(|| VoiceError::Gateway("invalid gateway sequence".into()))?
                                    as i64,
                            ),
                            None => None,
                        };
                        let op = value.get("op").and_then(Value::as_u64).and_then(|n| u8::try_from(n).ok())
                            .ok_or_else(|| VoiceError::Gateway("missing gateway opcode".into()))?;
                        // Op 2 and 4 carry the address and transport key; log only their opcode.
                        if matches!(op, 2 | 4) {
                            log::debug!("voice gateway received op {op} dave={}", value["d"]["dave_protocol_version"]);
                        } else {
                            log::debug!("voice gateway received op {op} {}", value["d"]);
                        }
                        let identify_to_send = self.json(op, &value["d"])?;
                        if let Some(seq) = next_seq {
                            self.sequence = seq;
                        }
                        if let Some(identify) = identify_to_send {
                            self.send(&identify).await?;
                        }
                        if !self.resuming && self.identify.is_none() {
                            self.flush_outbound().await?;
                        }
                    }
                    Message::Binary(bytes) => {
                        if self.identify.is_some() {
                            return Err(VoiceError::Gateway(
                                "expected voice hello before binary opcodes".into(),
                            ));
                        }
                        let &[s0, s1, op, ref payload @ ..] = bytes.as_ref() else {
                            return Err(VoiceError::Gateway("truncated binary gateway message".into()));
                        };
                        let next_seq = i64::from(u16::from_be_bytes([s0, s1]));
                        log::debug!("voice gateway received binary op {op} ({} bytes)", bytes.len());
                        let outgoing = self.dave.handle_binary(op, payload).map_err(dave_error)?;
                        self.sequence = next_seq;
                        if self.dave.active_version() == Some(1) {
                            self.alone_at = None;
                        } else if matches!(op, 25 | 29 | 30) {
                            self.alone_at = Some(Instant::now() + ALONE_GRACE);
                        }
                        self.enqueue_outbound(outgoing)?;
                        if !self.resuming && self.identify.is_none() {
                            self.flush_outbound().await?;
                        }
                    }
                    Message::Ping(_) => {
                        // tungstenite queues a Pong reply on Ping; flush it with a bounded timeout.
                        let flushed =
                            tokio::time::timeout(Duration::from_secs(5), self.stream.flush()).await;
                        match flushed {
                            Ok(Ok(())) => {}
                            Ok(Err(err)) => return Err(self.ws_error(err)),
                            Err(_) => {
                                return Err(self
                                    .transient_gateway_error("voice gateway write timed out"))
                            }
                        }
                    }
                    Message::Close(frame) => {
                        let (code, reason) = frame.map(|f| (u16::from(f.code), f.reason.to_string())).unwrap_or((1000, String::new()));
                        return Err(VoiceError::Closed { code, reason, by_remote: true });
                    }
                    _ => {},
                }
            }
            _ = self.heartbeat_sleep.as_mut(), if self.has_heartbeat => {
                if self.outstanding.is_some() {
                    return Err(self.transient_gateway_error("voice heartbeat was not acknowledged"));
                }
                let nonce = now_millis().max(self.last_nonce.saturating_add(1));
                self.last_nonce = nonce;
                let start = Instant::now();
                self.send_text(format!(r#"{{"op":3,"d":{{"t":{nonce},"seq_ack":{}}}}}"#, self.sequence)).await?;
                self.outstanding = Some((nonce, start));
                self.heartbeat_sleep.as_mut().reset(heartbeat_deadline(self.interval)?);
            }
            _ = async {
                match self.alone_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => pending().await,
                }
            } => {
                self.alone_at = None;
                if self.dave.start_alone().map_err(dave_error)? {
                    log::debug!("voice DAVE: no other member joined; sending with the local group");
                }
            }
            result = async { match &mut self.probe { Some(probe) => probe.await, None => pending().await } } => {
                self.probe = None;
                let discovered = result?;
                log::debug!("voice IP discovery finished");
                let mode = self.mode.ok_or_else(|| VoiceError::Gateway("no selected transport mode".into()))?;
                let ssrc = self.ssrc.ok_or_else(|| VoiceError::Gateway("missing voice SSRC".into()))?;
                self.transport = Some(Transport::new(discovered.socket.clone(), discovered.remote, ssrc));
                self.socket = Some(discovered.socket);
                #[derive(serde::Serialize)]
                struct SelectData<'a> {
                    address: &'a str,
                    port: u16,
                    mode: &'a str,
                }
                #[derive(serde::Serialize)]
                struct SelectProtocol<'a> {
                    protocol: &'a str,
                    data: SelectData<'a>,
                }
                #[derive(serde::Serialize)]
                struct Op1<'a> {
                    op: u8,
                    d: SelectProtocol<'a>,
                }
                self.send(&Op1 {
                    op: 1,
                    d: SelectProtocol {
                        protocol: "udp",
                        data: SelectData {
                            address: &discovered.address,
                            port: discovered.port,
                            mode: mode.name(),
                        },
                    },
                }).await?;
            }
            command = self.commands.recv(), if !self.resuming && self.identify.is_none() => {
                match command {
                    Some(Command::Audio(opus)) => {
                        self.audio(opus)?;
                    }
                    Some(Command::Speaking(on)) => {
                        if self.speaking == on {
                            return Ok(());
                        }
                        let ssrc = self.ssrc.ok_or(VoiceError::NotReady)?;
                        if !on {
                            // End the stream with five silence frames to stop Opus interpolation.
                            for _ in 0..5 {
                                self.audio(self.shared.packet(&[0xf8, 0xff, 0xfe]))?;
                            }
                        }
                        self.speaking = on;
                        self.enqueue_outbound(vec![Outbound::Json(json!({
                            "op": 5,
                            "d": { "speaking": u8::from(on), "delay": 0, "ssrc": ssrc }
                        }))])?;
                        self.flush_outbound().await?;
                    }
                    Some(Command::Close) | None => return Err(local_close()),
                }
            }
            result = async {
                match &self.socket {
                    Some(socket) => {
                        let mut discard = [0u8; 1];
                        socket.recv(&mut discard).await
                    }
                    None => pending().await,
                }
            } => {
                // Connected UDP reports ICMP unreachable (WSAECONNRESET on Windows) on recv; one
                // lost datagram must not end the voice session.
                if let Err(error) = result {
                    if !transport::transient_udp_error(&error) { return Err(error.into()); }
                }
                if let Some(socket) = &self.socket {
                    let mut discard = [0u8; 1];
                    for _ in 0..15 {
                        match socket.try_recv(&mut discard) {
                            Ok(_) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(error) if transport::transient_udp_error(&error) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn json(&mut self, opcode: u8, data: &Value) -> Result<Option<Value>, VoiceError> {
        if self.identify.is_some() && opcode != 8 {
            return Err(VoiceError::Gateway("expected voice hello before other opcodes".into()));
        }
        match opcode {
            8 => {
                let identify = self
                    .identify
                    .take()
                    .ok_or_else(|| VoiceError::Gateway("duplicate voice hello".into()))?;
                self.interval = heartbeat_interval(data.get("heartbeat_interval"))?;
                // Follow Hello's supplied cadence so the connection does not add control traffic
                // ahead of the server-selected schedule.
                self.heartbeat_sleep.as_mut().reset(heartbeat_deadline(self.interval)?);
                self.has_heartbeat = true;
                return Ok(Some(identify));
            }
            9 => {
                if !self.resuming {
                    return Err(VoiceError::Gateway("unexpected voice resumed".into()));
                }
                self.resuming = false;
            }
            2 => {
                if self.identify.is_some() || self.ssrc.is_some() {
                    return Err(VoiceError::Gateway("unexpected voice ready".into()));
                }
                let ssrc: u32 = integer(data, "ssrc")?;
                if ssrc == 0 {
                    return Err(VoiceError::Gateway("invalid voice SSRC".into()));
                }
                let raw_ip: IpAddr = data
                    .get("ip")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VoiceError::Gateway("missing voice IP".into()))?
                    .parse()
                    .map_err(gateway_error)?;
                let port: u16 = integer(data, "port")?;
                let require_public = self.url.starts_with("wss://");
                let Some(ip) = (port != 0)
                    .then(|| transport::valid_voice_udp_ip(raw_ip, require_public))
                    .flatten()
                else {
                    return Err(VoiceError::Gateway("invalid voice UDP endpoint".into()));
                };
                let modes = data
                    .get("modes")
                    .and_then(Value::as_array)
                    .ok_or_else(|| VoiceError::Gateway("missing transport modes".into()))?;
                let mode =
                    Mode::negotiate(modes.iter().filter_map(Value::as_str)).ok_or_else(|| {
                        VoiceError::Gateway("no supported RTP-size transport mode".into())
                    })?;
                self.ssrc = Some(ssrc);
                self.mode = Some(mode);
                self.probe = Some(Box::pin(discover(SocketAddr::new(ip, port), ssrc)));
            }
            4 => {
                let mode = data
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(Mode::parse)
                    .filter(|m| Some(*m) == self.mode)
                    .ok_or_else(|| VoiceError::Gateway("transport mode mismatch".into()))?;
                if data.get("audio_codec").and_then(Value::as_str).is_some_and(|c| c != "opus") {
                    return Err(VoiceError::Gateway("voice codec is not Opus".into()));
                }
                let key = read_key(data.get("secret_key"))?;
                let version: u16 = integer(data, "dave_protocol_version")?;
                self.transport
                    .as_ref()
                    .ok_or_else(|| {
                        VoiceError::Gateway("session description before IP discovery".into())
                    })?
                    .describe(mode, *key)?;
                let outgoing = self.dave.negotiate(version).map_err(dave_error)?;
                self.enqueue_outbound(outgoing)?;
                // The external sender may already be here (op 25 before op 4).
                if version == 1 && self.dave.active_version() != Some(1) {
                    self.alone_at = Some(Instant::now() + ALONE_GRACE);
                } else {
                    self.alone_at = None;
                }
            }
            6 => {
                let nonce = data.get("t").and_then(Value::as_i64);
                if let Some((sent, start)) = self.outstanding {
                    if nonce == Some(sent) {
                        self.shared.ping.store(
                            start.elapsed().as_millis().min(i64::MAX as u128) as i64,
                            Ordering::Relaxed,
                        );
                        self.outstanding = None;
                    }
                }
            }
            11 | 13 | 21 | 22 | 24 => {
                let outgoing = self.dave.handle_json(opcode, data).map_err(dave_error)?;
                self.enqueue_outbound(outgoing)?;
                if self.dave.active_version() == Some(1) {
                    self.alone_at = None;
                } else if opcode == 13
                    || (opcode == 24
                        && data.get("protocol_version").and_then(Value::as_u64) == Some(1))
                {
                    self.alone_at = Some(Instant::now() + ALONE_GRACE);
                } else if opcode == 24 {
                    self.alone_at = None;
                }
            }
            _ => {}
        }
        Ok(None)
    }
}

fn is_resumable_close_code(code: u16) -> bool {
    code < 4000 || code == 4015
}

fn is_transient_tungstenite_error(error: &tungstenite::Error) -> bool {
    matches!(
        error,
        tungstenite::Error::ConnectionClosed
            | tungstenite::Error::AlreadyClosed
            | tungstenite::Error::Io(_)
            | tungstenite::Error::Tls(_)
            | tungstenite::Error::Protocol(
                tungstenite::error::ProtocolError::ResetWithoutClosingHandshake
                    | tungstenite::error::ProtocolError::SendAfterClosing
                    | tungstenite::error::ProtocolError::ReceivedAfterClosing
            )
    )
}

#[cfg(windows)]
fn disable_udp_connreset(socket: &UdpSocket) -> std::io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawSocket, RawSocket};

    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAIoctl(
            s: RawSocket,
            dwIoControlCode: u32,
            lpvInBuffer: *const c_void,
            cbInBuffer: u32,
            lpvOutBuffer: *mut c_void,
            cbOutBuffer: u32,
            lpcbBytesReturned: *mut u32,
            lpOverlapped: *mut c_void,
            lpCompletionRoutine: *const c_void,
        ) -> i32;
    }

    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    let enable: i32 = 0;
    let mut bytes_returned: u32 = 0;
    let rc = unsafe {
        WSAIoctl(
            socket.as_raw_socket(),
            SIO_UDP_CONNRESET,
            std::ptr::from_ref(&enable).cast::<c_void>(),
            std::mem::size_of_val(&enable) as u32,
            std::ptr::null_mut(),
            0,
            std::ptr::from_mut(&mut bytes_returned),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
fn disable_udp_connreset(_socket: &UdpSocket) -> std::io::Result<()> {
    Ok(())
}

async fn discover(remote: SocketAddr, ssrc: u32) -> Result<Discovered, VoiceError> {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        let bind = if remote.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let raw_socket = UdpSocket::bind(bind).await?;
        let _ = disable_udp_connreset(&raw_socket);
        let socket = Arc::new(raw_socket);
        // Connected UDP accepts discovery answers and subsequent traffic only from this peer.
        socket.connect(remote).await?;
        let request = transport::ip_discovery_request(ssrc);
        let mut bytes = [0u8; 75];
        loop {
            let sent = match socket.send(&request).await {
                Err(error) if transport::retryable_udp_send_error(&error) => {
                    socket.send(&request).await
                }
                other => other,
            };
            match sent {
                Ok(_) => {}
                Err(error) if transport::transient_udp_error(&error) => {}
                Err(error) => return Err(error.into()),
            }
            let retry_at = Instant::now() + Duration::from_secs(1);
            loop {
                match tokio::time::timeout_at(retry_at, socket.recv(&mut bytes)).await {
                    Ok(Ok(count)) => {
                        if let Some((address, port)) =
                            transport::parse_ip_discovery_response(&bytes[..count], ssrc)
                        {
                            return Ok(Discovered { socket, remote, address, port });
                        }
                    }
                    Ok(Err(error)) if transport::transient_udp_error(&error) => {}
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => break,
                }
            }
        }
    })
    .await
    .map_err(|_| VoiceError::Gateway("voice IP discovery timed out".into()))?
}

fn integer<T: TryFrom<u64>>(data: &Value, field: &str) -> Result<T, VoiceError> {
    data.get(field)
        .and_then(Value::as_u64)
        .and_then(|n| T::try_from(n).ok())
        .ok_or_else(|| VoiceError::Gateway(format!("invalid voice field: {field}")))
}

fn read_key(value: Option<&Value>) -> Result<Zeroizing<[u8; 32]>, VoiceError> {
    let values = value
        .and_then(Value::as_array)
        .filter(|v| v.len() == 32)
        .ok_or_else(|| VoiceError::Gateway("invalid transport key length".into()))?;
    let mut key = Zeroizing::new([0u8; 32]);
    for (dst, src) in key.iter_mut().zip(values) {
        *dst = src
            .as_u64()
            .and_then(|n| n.try_into().ok())
            .ok_or_else(|| VoiceError::Gateway("invalid transport key byte".into()))?;
    }
    Ok(key)
}

/// Discord normally serializes this as an integer number of milliseconds, but
/// accepting a finite fractional JSON number costs nothing and avoids rejecting
/// otherwise usable Hello payloads. The deadline check rejects values the local
/// monotonic clock cannot represent instead of silently changing the interval.
fn heartbeat_interval(value: Option<&Value>) -> Result<Duration, VoiceError> {
    let millis = value
        .and_then(Value::as_f64)
        .filter(|millis| millis.is_finite() && *millis > 0.0)
        .ok_or_else(|| VoiceError::Gateway("invalid heartbeat interval".into()))?;
    let interval = Duration::try_from_secs_f64(millis / 1000.0)
        .map_err(|_| VoiceError::Gateway("invalid heartbeat interval".into()))?;
    if interval.is_zero() {
        return Err(VoiceError::Gateway("invalid heartbeat interval".into()));
    }
    Ok(interval)
}

fn heartbeat_deadline(interval: Duration) -> Result<Instant, VoiceError> {
    Instant::now()
        .checked_add(interval)
        .ok_or_else(|| VoiceError::Gateway("heartbeat interval is too large".into()))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
fn gateway_error(error: impl std::fmt::Display) -> VoiceError {
    VoiceError::Gateway(error.to_string())
}
fn dave_error(error: rustlink_dave::Error) -> VoiceError {
    VoiceError::Crypto(error.to_string())
}
fn local_close() -> VoiceError {
    VoiceError::Closed { code: 1000, reason: String::new(), by_remote: false }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod simulation;
