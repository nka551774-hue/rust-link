//! Discord voice gateway v8. A single task orders speaking, DAVE transitions and media.
use std::future::{pending, Future};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use rustlink_dave::{Outbound, Session};
use rustlink_protocol::VoiceState;
use serde_json::{json, Value};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zeroize::Zeroizing;

use crate::transport::{self, Mode, Transport};
use crate::{CloseHandler, Closed, Connection, VoiceError};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_CAPACITY: usize = 32;
const MAX_RETAINED_PACKET: usize = 4096;
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Probe = Pin<Box<dyn Future<Output = Result<Discovered, VoiceError>> + Send>>;

pub struct DiscordConnection {
    commands: mpsc::Sender<Command>,
    ready: AtomicBool,
    closed: AtomicBool,
    ping: AtomicI64,
    audio_buffers: Mutex<Vec<Vec<u8>>>,
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
        self.commands.try_send(command).map_err(|error| {
            if let Command::Audio(packet) = error.into_inner() {
                self.recycle(packet);
            }
            VoiceError::Gateway("voice command queue unavailable".into())
        })
    }

    fn packet(&self, payload: &[u8]) -> Vec<u8> {
        let mut packet = self
            .audio_buffers
            .lock()
            .ok()
            .and_then(|mut buffers| buffers.pop())
            .unwrap_or_default();
        packet.reserve(payload.len() + 16);
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

impl Connection for DiscordConnection {
    fn write(&self, payload: &[u8]) -> Result<(), VoiceError> {
        if !self.connected() {
            return Err(VoiceError::NotReady);
        }
        self.command(Command::Audio(self.packet(payload)))
    }
    fn speaking(&self, on: bool) -> Result<(), VoiceError> {
        self.command(Command::Speaking(on))
    }
    fn connected(&self) -> bool {
        self.ready.load(Ordering::Acquire) && !self.closed.load(Ordering::Acquire)
    }
    fn ping(&self) -> i64 {
        if self.connected() {
            self.ping.load(Ordering::Relaxed)
        } else {
            -1
        }
    }
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.ready.store(false, Ordering::Release);
            // If the bounded queue is full, the task notices closed before its next send.
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
    stream: Socket,
    dave: Session,
    connection: Arc<DiscordConnection>,
    commands: mpsc::Receiver<Command>,
    probe: Option<Probe>,
    socket: Option<Arc<UdpSocket>>,
    transport: Option<Transport>,
    ssrc: Option<u32>,
    mode: Option<Mode>,
    interval: Duration,
    next_heartbeat: Option<Instant>,
    outstanding: Option<(i64, Instant)>,
    sequence: i64,
    identify: Option<Value>,
    speaking: bool,
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
    let url = format!("wss://{}/?v={}", state.endpoint.trim(), crate::GATEWAY_VERSION);
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
        let (stream, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false)
            .await
            .map_err(gateway_error)?;
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
        let connection = Arc::new(DiscordConnection {
            commands,
            ready: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            ping: AtomicI64::new(-1),
            audio_buffers: Mutex::new(Vec::with_capacity(COMMAND_CAPACITY)),
        });
        let mut gateway = Gateway {
            stream,
            dave,
            connection,
            commands: receiver,
            probe: None,
            socket: None,
            transport: None,
            ssrc: None,
            mode: None,
            interval: Duration::from_secs(5),
            next_heartbeat: None,
            outstanding: None,
            sequence: -1,
            speaking: false,
            identify: Some(
                json!({"op":0,"d":{"server_id":guild,"user_id":user,"session_id":state.session_id,"token":state.token,"max_dave_protocol_version":rustlink_dave::MAX_PROTOCOL_VERSION}}),
            ),
        };
        while !gateway.transport.as_ref().is_some_and(Transport::ready) || !gateway.dave.ready() {
            gateway.step().await?;
        }
        gateway.connection.ready.store(true, Ordering::Release);
        Ok::<_, VoiceError>(gateway)
    };
    let gateway = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await.map_err(|_| {
        VoiceError::Gateway("voice handshake timed out (including DAVE negotiation)".into())
    })??;
    let connection = gateway.connection.clone();
    tokio::spawn(gateway.run(on_close));
    Ok(connection)
}

impl Gateway {
    fn audio(&mut self, mut frame: Vec<u8>) -> Result<(), VoiceError> {
        let result = self
            .dave
            .encrypt_opus_in_place(&mut frame)
            .map_err(dave_error)
            .and_then(|()| self.transport.as_ref().ok_or(VoiceError::NotReady)?.write(&frame));
        self.connection.recycle(frame);
        result
    }

    async fn run(mut self, on_close: CloseHandler) {
        let error = loop {
            if let Err(error) = self.step().await {
                break error;
            }
        };
        self.connection.ready.store(false, Ordering::Release);
        self.connection.closed.store(true, Ordering::Release);
        let closed = match error {
            VoiceError::Closed { code, reason, by_remote } => Closed { code, reason, by_remote },
            error => Closed { code: 1006, reason: error.to_string(), by_remote: true },
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), self.stream.close(None)).await;
        on_close(closed);
    }

    async fn send(&mut self, value: Value) -> Result<(), VoiceError> {
        self.send_message(Message::Text(value.to_string().into())).await
    }

    async fn send_message(&mut self, message: Message) -> Result<(), VoiceError> {
        tokio::time::timeout(Duration::from_secs(5), self.stream.send(message))
            .await
            .map_err(|_| VoiceError::Gateway("voice gateway write timed out".into()))?
            .map_err(gateway_error)
    }

    async fn outbound(&mut self, messages: Vec<Outbound>) -> Result<(), VoiceError> {
        for message in messages {
            match message {
                Outbound::Json(value) => self.send(value).await?,
                Outbound::Binary(bytes) => self.send_message(Message::Binary(bytes.into())).await?,
            }
        }
        Ok(())
    }

    async fn step(&mut self) -> Result<(), VoiceError> {
        if self.connection.closed.load(Ordering::Acquire) {
            return Err(local_close());
        }
        let deadline = self.next_heartbeat.unwrap_or_else(|| Instant::now() + HANDSHAKE_TIMEOUT);
        let mut udp_buffer = [0u8; 2048];
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline), if self.next_heartbeat.is_some() => {
                if self.outstanding.is_some() { return Err(VoiceError::Gateway("voice heartbeat was not acknowledged".into())); }
                let nonce = now_millis();
                self.send(json!({"op":3,"d":{"t":nonce,"seq_ack":self.sequence}})).await?;
                self.outstanding = Some((nonce,Instant::now()));
                self.next_heartbeat = Some(heartbeat_deadline(self.interval)?);
            }
            message = self.stream.next() => {
                match message.ok_or_else(|| VoiceError::Gateway("voice gateway hung up".into()))?.map_err(gateway_error)? {
                    Message::Text(text) => {
                        let value: Value = serde_json::from_str(&text).map_err(gateway_error)?;
                        if let Some(sequence) = value.get("seq") {
                            self.sequence = sequence.as_u64().filter(|n| *n <= u16::MAX as u64)
                                .ok_or_else(|| VoiceError::Gateway("invalid gateway sequence".into()))? as i64;
                        }
                        let op = value.get("op").and_then(Value::as_u64).and_then(|n| u8::try_from(n).ok())
                            .ok_or_else(|| VoiceError::Gateway("missing gateway opcode".into()))?;
                        self.json(op,&value["d"]).await?;
                    }
                    Message::Binary(bytes) => {
                        if bytes.len() < 3 { return Err(VoiceError::Gateway("truncated binary gateway message".into())); }
                        self.sequence = u16::from_be_bytes([bytes[0],bytes[1]]) as i64;
                        let outgoing = self.dave.handle_binary(bytes[2],&bytes[3..]).map_err(dave_error)?;
                        self.outbound(outgoing).await?;
                    }
                    Message::Ping(bytes) => self.send_message(Message::Pong(bytes)).await?,
                    Message::Close(frame) => {
                        let (code,reason) = frame.map(|f| (u16::from(f.code),f.reason.to_string())).unwrap_or((1000,String::new()));
                        return Err(VoiceError::Closed {code,reason,by_remote:true});
                    }
                    _ => {},
                }
            }
            result = async { match &mut self.probe { Some(probe) => probe.await, None => pending().await } } => {
                self.probe = None;
                let discovered = result?;
                let mode = self.mode.ok_or_else(|| VoiceError::Gateway("no selected transport mode".into()))?;
                self.transport = Some(Transport::new(discovered.socket.clone(),discovered.remote,self.ssrc.unwrap()));
                self.socket = Some(discovered.socket);
                self.send(json!({"op":1,"d":{"protocol":"udp","data":{"address":discovered.address,"port":discovered.port,"mode":mode.name()}}})).await?;
            }
            command = self.commands.recv() => {
                match command {
                    Some(Command::Audio(opus)) => {
                        self.audio(opus)?;
                    }
                    Some(Command::Speaking(on)) => {
                        if !on && self.speaking {
                            // End the stream with five silence frames to stop Opus interpolation.
                            for _ in 0..5 {
                                self.audio(self.connection.packet(&[0xf8,0xff,0xfe]))?;
                            }
                        }
                        self.send(json!({"op":5,"d":{"speaking":u8::from(on),"delay":0,"ssrc":self.ssrc}})).await?;
                        self.speaking = on;
                    }
                    Some(Command::Close) | None => return Err(local_close()),
                }
            }
            result = async { match &self.socket { Some(socket) => socket.recv(&mut udp_buffer).await, None => pending().await } } => { result?; }
        }
        Ok(())
    }

    async fn json(&mut self, opcode: u8, data: &Value) -> Result<(), VoiceError> {
        match opcode {
            8 => {
                let identify = self
                    .identify
                    .take()
                    .ok_or_else(|| VoiceError::Gateway("duplicate voice hello".into()))?;
                self.interval = heartbeat_interval(data.get("heartbeat_interval"))?;
                // Follow Hello's supplied cadence so the connection does not add control traffic
                // ahead of the server-selected schedule.
                self.next_heartbeat = Some(heartbeat_deadline(self.interval)?);
                self.send(identify).await?;
            }
            2 => {
                if self.identify.is_some() || self.ssrc.is_some() {
                    return Err(VoiceError::Gateway("unexpected voice ready".into()));
                }
                let ssrc = integer(data, "ssrc")?;
                let ip: IpAddr = data
                    .get("ip")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VoiceError::Gateway("missing voice IP".into()))?
                    .parse()
                    .map_err(gateway_error)?;
                let port: u16 = integer(data, "port")?;
                if port == 0 {
                    return Err(VoiceError::Gateway("invalid voice port".into()));
                }
                let offered: Vec<String> = data
                    .get("modes")
                    .and_then(Value::as_array)
                    .ok_or_else(|| VoiceError::Gateway("missing transport modes".into()))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                let mode = Mode::negotiate(&offered).ok_or_else(|| {
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
                self.outbound(outgoing).await?;
            }
            6 => {
                let nonce = data.get("t").and_then(Value::as_i64);
                if let Some((sent, start)) = self.outstanding {
                    if nonce == Some(sent) {
                        self.connection.ping.store(
                            start.elapsed().as_millis().min(i64::MAX as u128) as i64,
                            Ordering::Relaxed,
                        );
                        self.outstanding = None;
                    }
                }
            }
            11 | 13 | 21 | 22 | 24 => {
                let outgoing = self.dave.handle_json(opcode, data).map_err(dave_error)?;
                self.outbound(outgoing).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

async fn discover(remote: SocketAddr, ssrc: u32) -> Result<Discovered, VoiceError> {
    tokio::time::timeout(PROBE_TIMEOUT, async {
        let bind = if remote.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        // Connected UDP accepts discovery answers and subsequent traffic only from this peer.
        socket.connect(remote).await?;
        socket.send(&transport::ip_discovery_request(ssrc)).await?;
        let mut bytes = [0u8; 2048];
        let count = socket.recv(&mut bytes).await?;
        let (address, port) = transport::parse_ip_discovery_response(&bytes[..count], ssrc)
            .ok_or_else(|| VoiceError::Gateway("invalid IP discovery response".into()))?;
        Ok(Discovered { socket, remote, address, port })
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
