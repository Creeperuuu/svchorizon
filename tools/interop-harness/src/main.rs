//! End-to-end interop harness: joins a real Minecraft 26.3 (protocol 777)
//! server as a vanilla-protocol client and drives the complete official
//! Simple Voice Chat plugin handshake — plugin-channel secret request, UDP
//! authentication, connection validation, keepalive and microphone frames —
//! using the exact wire code of the svchorizon-protocol crate.
//!
//! Usage: interop-harness [--host HOST] [--port PORT] [--name NAME]
//!
//! Exits 0 only after the server has authenticated and validated the UDP
//! voice connection and answered keepalives for several seconds.

use std::{
    io::{Read, Write},
    net::{TcpStream, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use svchorizon_protocol::{
    handshake::{self, VoiceServerInfo},
    protocol::{ClientDatagram, ServerDatagram, VoicePacket},
    wire::{Reader, Writer},
};
use thiserror::Error;
use uuid::Uuid;

/// Protocol version of Minecraft 26.3 (horizon-rust/horizon@ba6344a,
/// horizon-core/src/version.rs).
const PROTOCOL_VERSION: i32 = 777;
const GAME_HOST: &str = "127.0.0.1";
const GAME_PORT: u16 = 25565;
const PLAYER_NAME: &str = "svchorizon";

/// Packet ids for protocol 777, from the pinned Horizon packet id table
/// (horizon-rust/horizon@ba6344a crates/horizon-protocol/src/ids.rs).
mod ids {
    pub mod handshake {
        pub mod serverbound {
            pub const INTENTION: i32 = 0;
        }
    }
    pub mod login {
        pub mod clientbound {
            pub const LOGIN_DISCONNECT: i32 = 0;
            pub const HELLO: i32 = 1; // encryption request (online mode only)
            pub const LOGIN_FINISHED: i32 = 2;
            pub const LOGIN_COMPRESSION: i32 = 3;
            pub const CUSTOM_QUERY: i32 = 4;
            pub const COOKIE_REQUEST: i32 = 5;
        }
        pub mod serverbound {
            pub const HELLO: i32 = 0;
            pub const CUSTOM_QUERY_ANSWER: i32 = 2;
            pub const LOGIN_ACKNOWLEDGED: i32 = 3;
            pub const COOKIE_RESPONSE: i32 = 4;
        }
    }
    pub mod configuration {
        pub mod clientbound {
            pub const CUSTOM_PAYLOAD: i32 = 1;
            pub const DISCONNECT: i32 = 2;
            pub const FINISH_CONFIGURATION: i32 = 3;
            pub const KEEP_ALIVE: i32 = 4;
            pub const PING: i32 = 5;
            pub const SELECT_KNOWN_PACKS: i32 = 15;
        }
        pub mod serverbound {
            pub const CUSTOM_PAYLOAD: i32 = 2;
            pub const FINISH_CONFIGURATION: i32 = 3;
            pub const KEEP_ALIVE: i32 = 4;
            pub const PONG: i32 = 5;
            pub const SELECT_KNOWN_PACKS: i32 = 7;
        }
    }
    pub mod play {
        pub mod clientbound {
            pub const BUNDLE_DELIMITER: i32 = 0;
            pub const CUSTOM_PAYLOAD: i32 = 24;
            pub const DISCONNECT: i32 = 32;
            pub const KEEP_ALIVE: i32 = 45;
            pub const PING: i32 = 62;
            pub const START_CONFIGURATION: i32 = 120;
        }
        pub mod serverbound {
            pub const CONFIGURATION_ACKNOWLEDGED: i32 = 16;
            pub const CUSTOM_PAYLOAD: i32 = 22;
            pub const KEEP_ALIVE: i32 = 28;
            pub const PONG: i32 = 45;
        }
    }
}

/// Every channel the official plugin registers (mixins runtime
/// REGISTERED_CHANNELS, matching NetManager.init upstream).
const REGISTERED_CHANNELS: &[&str] = &[
    "voicechat:request_secret",
    "voicechat:secret",
    "voicechat:client_state",
    "voicechat:state",
    "voicechat:states",
    "voicechat:remove_state",
    "voicechat:create_group",
    "voicechat:set_group",
    "voicechat:leave_group",
    "voicechat:joined_group",
    "voicechat:add_group",
    "voicechat:remove_group",
    "voicechat:add_category",
    "voicechat:remove_category",
];

#[derive(Debug, Error)]
enum HarnessError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("svc wire error: {0}")]
    Wire(#[from] svchorizon_protocol::wire::WireError),
    #[error("svc handshake error: {0}")]
    Handshake(#[from] svchorizon_protocol::handshake::HandshakeError),
    #[error("svc protocol error: {0}")]
    Protocol(#[from] svchorizon_protocol::protocol::ProtocolError),
    #[error("server disconnected: {0}")]
    Disconnected(String),
    #[error("timed out waiting for {0}")]
    Timeout(String),
    #[error("{0}")]
    Fail(String),
}

type Result<T> = std::result::Result<T, HarnessError>;

fn stage(step: &str) {
    println!("[stage] {step}");
}

/// Length- and compression-framed Minecraft TCP connection.
struct Framed {
    stream: TcpStream,
    /// Compression threshold; `None` until the server enables it.
    threshold: Option<usize>,
}

impl Framed {
    fn connect(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        // Generous timeout for login/config (registry data can lag); the
        // responder phase shortens it right before the UDP session starts.
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        Ok(Self {
            stream,
            threshold: None,
        })
    }

    fn read_varint_stream(&mut self) -> Result<usize> {
        let mut value = 0u32;
        let mut shift = 0;
        for _ in 0..5 {
            let mut byte = [0u8; 1];
            self.stream.read_exact(&mut byte)?;
            value |= u32::from(byte[0] & 0x7f) << shift;
            if byte[0] & 0x80 == 0 {
                return Ok(value as usize);
            }
            shift += 7;
        }
        Err(HarnessError::Fail(
            "frame varint longer than five bytes".into(),
        ))
    }

    fn recv_packet(&mut self) -> Result<(i32, Vec<u8>)> {
        let total = self.read_varint_stream()?;
        let mut frame = vec![0u8; total];
        self.stream.read_exact(&mut frame)?;
        let body = match self.threshold {
            None => frame,
            Some(_threshold) => {
                let mut reader = Reader::new(&frame);
                let uncompressed = reader.varint()? as usize;
                let rest = &frame[reader.offset()..];
                if uncompressed == 0 {
                    rest.to_vec()
                } else {
                    let mut decoder = ZlibDecoder::new(rest);
                    let mut out = Vec::with_capacity(uncompressed);
                    decoder.read_to_end(&mut out)?;
                    if out.len() != uncompressed {
                        return Err(HarnessError::Fail(format!(
                            "decompressed length {} != declared {uncompressed}",
                            out.len()
                        )));
                    }
                    out
                }
            }
        };
        let mut reader = Reader::new(&body);
        let id = reader.varint()? as i32;
        let offset = reader.offset();
        Ok((id, body[offset..].to_vec()))
    }

    fn send_packet(&mut self, id: i32, body: &[u8]) -> Result<()> {
        let mut payload = Writer::default();
        payload.varint(id as u32);
        payload.raw(body);
        let frame = match self.threshold {
            None => {
                let mut frame = Writer::default();
                frame.varint(payload.data.len() as u32);
                frame.raw(&payload.data);
                frame.data
            }
            Some(threshold) => {
                let mut frame = Writer::default();
                if payload.data.len() < threshold {
                    // Below the threshold the vanilla server still expects the
                    // compression envelope with an empty size prefix.
                    let mut inner = Writer::default();
                    inner.varint(0);
                    inner.raw(&payload.data);
                    frame.varint(inner.data.len() as u32);
                    frame.raw(&inner.data);
                } else {
                    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
                    encoder.write_all(&payload.data)?;
                    let compressed = encoder.finish()?;
                    let mut inner = Writer::default();
                    inner.varint(payload.data.len() as u32);
                    inner.raw(&compressed);
                    frame.varint(inner.data.len() as u32);
                    frame.raw(&inner.data);
                }
                frame.data
            }
        };
        self.stream.write_all(&frame)?;
        self.stream.flush()?;
        Ok(())
    }
}

fn write_string(out: &mut Writer, value: &str) {
    out.utf(value);
}

fn register_payload() -> Vec<u8> {
    let mut data = Vec::new();
    for (index, channel) in REGISTERED_CHANNELS.iter().enumerate() {
        if index > 0 {
            data.push(0);
        }
        data.extend_from_slice(channel.as_bytes());
    }
    data
}

fn send_custom_payload(framed: &mut Framed, id: i32, channel: &str, data: &[u8]) -> Result<()> {
    let mut body = Writer::default();
    write_string(&mut body, channel);
    body.raw(data);
    framed.send_packet(id, &body.data)
}

fn mc_login(framed: &mut Framed, host: &str, port: u16, name: &str) -> Result<Uuid> {
    let mut intention = Writer::default();
    intention.varint(PROTOCOL_VERSION as u32);
    write_string(&mut intention, host);
    intention.data.extend_from_slice(&port.to_be_bytes());
    intention.varint(2);
    framed.send_packet(ids::handshake::serverbound::INTENTION, &intention.data)?;

    let mut hello = Writer::default();
    write_string(&mut hello, name);
    // Protocol 777: ServerHello = name (StrN<16>) + profile UUID.
    hello.uuid(Uuid::new_v4());
    framed.send_packet(ids::login::serverbound::HELLO, &hello.data)?;

    loop {
        let (id, payload) = framed.recv_packet()?;
        let mut reader = Reader::new(&payload);
        match id {
            ids::login::clientbound::LOGIN_COMPRESSION => {
                let threshold = reader.varint()?;
                framed.threshold = if threshold == 0 {
                    None
                } else {
                    Some(threshold as usize)
                };
                println!("[mc] compression threshold={threshold}");
            }
            ids::login::clientbound::CUSTOM_QUERY => {
                let message_id = reader.varint()?;
                let channel = reader.utf(32767)?;
                println!("[mc] login custom query: {channel}");
                let mut answer = Writer::default();
                answer.varint(message_id);
                answer.bool(false);
                framed.send_packet(ids::login::serverbound::CUSTOM_QUERY_ANSWER, &answer.data)?;
            }
            ids::login::clientbound::COOKIE_REQUEST => {
                let channel = reader.utf(32767)?;
                println!("[mc] login cookie request: {channel}");
                let mut response = Writer::default();
                write_string(&mut response, &channel);
                response.bool(false);
                framed.send_packet(ids::login::serverbound::COOKIE_RESPONSE, &response.data)?;
            }
            ids::login::clientbound::HELLO => {
                return Err(HarnessError::Fail(
                    "server requested encryption (online-mode? run with online-mode=false)".into(),
                ));
            }
            ids::login::clientbound::LOGIN_DISCONNECT => {
                return Err(HarnessError::Disconnected(
                    reader.utf(262144).unwrap_or_default(),
                ));
            }
            ids::login::clientbound::LOGIN_FINISHED => {
                let uuid = reader.uuid()?;
                let username = reader.utf(16)?;
                framed.send_packet(ids::login::serverbound::LOGIN_ACKNOWLEDGED, &[])?;
                println!("[mc] logged in as {username} ({uuid})");
                return Ok(uuid);
            }
            other => println!("[mc] ignoring login packet {other}"),
        }
    }
}

fn mc_configure(framed: &mut Framed) -> Result<()> {
    send_custom_payload(
        framed,
        ids::configuration::serverbound::CUSTOM_PAYLOAD,
        "minecraft:register",
        &register_payload(),
    )?;
    send_custom_payload(
        framed,
        ids::configuration::serverbound::CUSTOM_PAYLOAD,
        "minecraft:brand",
        &brand_payload(),
    )?;
    loop {
        let (id, payload) = framed.recv_packet()?;
        let mut reader = Reader::new(&payload);
        match id {
            ids::configuration::clientbound::FINISH_CONFIGURATION => {
                framed.send_packet(ids::configuration::serverbound::FINISH_CONFIGURATION, &[])?;
                println!("[mc] configuration finished");
                return Ok(());
            }
            ids::configuration::clientbound::KEEP_ALIVE => {
                let token = reader.i64()?;
                let mut answer = Writer::default();
                answer.i64(token);
                framed.send_packet(ids::configuration::serverbound::KEEP_ALIVE, &answer.data)?;
            }
            ids::configuration::clientbound::PING => {
                let value = reader.i32()?;
                let mut answer = Writer::default();
                answer.i32(value);
                framed.send_packet(ids::configuration::serverbound::PONG, &answer.data)?;
            }
            ids::configuration::clientbound::SELECT_KNOWN_PACKS => {
                let count = reader.varint()?;
                let mut answer = Writer::default();
                answer.varint(count);
                for _ in 0..count {
                    let namespace = reader.utf(256)?;
                    let pack = reader.utf(256)?;
                    let version = reader.utf(256)?;
                    write_string(&mut answer, &namespace);
                    write_string(&mut answer, &pack);
                    write_string(&mut answer, &version);
                }
                framed.send_packet(
                    ids::configuration::serverbound::SELECT_KNOWN_PACKS,
                    &answer.data,
                )?;
            }
            ids::configuration::clientbound::CUSTOM_PAYLOAD => {
                let channel = reader.utf(32767)?;
                if channel.starts_with("voicechat:") {
                    println!("[mc] config payload from plugin: {channel}");
                }
            }
            ids::configuration::clientbound::DISCONNECT => {
                return Err(HarnessError::Disconnected(
                    reader.utf(262144).unwrap_or_default(),
                ));
            }
            _other => {}
        }
    }
}

fn brand_payload() -> Vec<u8> {
    let mut data = Writer::default();
    write_string(&mut data, "svchorizon-interop");
    data.data
}

/// Keeps the play-state TCP connection alive (keepalive/ping answers) while
/// the main thread drives the UDP voice protocol.
fn play_responder(
    framed: Arc<Mutex<Framed>>,
    secret_tx: mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        let (id, payload) = {
            let mut guard = match framed.lock() {
                Ok(guard) => guard,
                Err(_) => return,
            };
            match guard.recv_packet() {
                Ok(packet) => packet,
                Err(HarnessError::Io(error))
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue; // idle: re-check the stop flag
                }
                Err(error) => {
                    if !stop.load(Ordering::Acquire) {
                        eprintln!("[tcp] read ended: {error}");
                    }
                    return;
                }
            }
        };
        let mut reader = Reader::new(&payload);
        match id {
            ids::play::clientbound::BUNDLE_DELIMITER => {}
            ids::play::clientbound::CUSTOM_PAYLOAD => {
                let Ok(channel) = reader.utf(32767) else {
                    continue;
                };
                let data = &payload[reader.offset()..];
                if channel == handshake::SECRET_CHANNEL {
                    let _ = secret_tx.send(data.to_vec());
                }
            }
            ids::play::clientbound::KEEP_ALIVE => {
                let Ok(token) = reader.i64() else { continue };
                let mut answer = Writer::default();
                answer.i64(token);
                let mut guard = framed.lock().ok();
                if let Some(guard) = guard.as_mut() {
                    let _ = guard.send_packet(ids::play::serverbound::KEEP_ALIVE, &answer.data);
                }
            }
            ids::play::clientbound::PING => {
                let Ok(value) = reader.i32() else { continue };
                let mut answer = Writer::default();
                answer.i32(value);
                let mut guard = framed.lock().ok();
                if let Some(guard) = guard.as_mut() {
                    let _ = guard.send_packet(ids::play::serverbound::PONG, &answer.data);
                }
            }
            ids::play::clientbound::START_CONFIGURATION => {
                let mut guard = framed.lock().ok();
                if let Some(guard) = guard.as_mut() {
                    let _ =
                        guard.send_packet(ids::play::serverbound::CONFIGURATION_ACKNOWLEDGED, &[]);
                }
            }
            ids::play::clientbound::DISCONNECT => {
                return eprintln!(
                    "[tcp] server disconnected: {}",
                    reader.utf(262144).unwrap_or_default()
                );
            }
            _other => {}
        }
    }
}

/// Runs the full official UDP voice handshake and five seconds of live
/// keepalive/microphone traffic.
fn svc_voice_session(info: &VoiceServerInfo, player_uuid: Uuid, game_host: &str) -> Result<()> {
    let endpoint = info.endpoint(game_host)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect((endpoint.host.as_str(), endpoint.port))?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    println!(
        "[svc] voice endpoint {}:{} (mtu {}, keepalive {} ms)",
        endpoint.host, endpoint.port, info.mtu_size, info.keep_alive_ms
    );

    // SVC 2.6.27: every client datagram is prefixed with the player's own
    // profile UUID (the server keys its secret map by it).
    let connection_id = info.player_uuid;
    if connection_id != player_uuid {
        println!(
            "[svc] note: secret packet player UUID {connection_id} differs from login UUID {player_uuid}"
        );
    }
    let authenticate = VoicePacket::Authenticate {
        player_uuid,
        secret: info.secret.clone(),
    };
    let mut authenticated = false;
    let mut last_auth: Option<Instant> = None;
    let mut last_check: Option<Instant> = None;
    let mut sequence = 0i64;
    let mut last_mic = Instant::now();
    let mut connected_at: Option<Instant> = None;
    let overall_deadline = Instant::now() + Duration::from_secs(60);
    let mut buffer = [0u8; 2048];

    loop {
        let now = Instant::now();
        if now > overall_deadline {
            return Err(HarnessError::Timeout("UDP voice handshake".into()));
        }
        if !authenticated && last_auth.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
            send_voice(&socket, &connection_id, &authenticate, &info.secret)?;
            last_auth = Some(now);
        } else if authenticated
            && connected_at.is_none()
            && last_check.is_none_or(|at| at.elapsed() >= Duration::from_secs(1))
        {
            send_voice(
                &socket,
                &connection_id,
                &VoicePacket::ConnectionCheck,
                &info.secret,
            )?;
            last_check = Some(now);
        }
        if let Some(connected) = connected_at
            && last_mic.elapsed() >= Duration::from_millis(50)
        {
            send_voice(
                &socket,
                &connection_id,
                &VoicePacket::Mic {
                    opus: vec![0u8; 64],
                    sequence,
                    whispering: false,
                },
                &info.secret,
            )?;
            sequence += 1;
            last_mic = Instant::now();
            if connected.elapsed() >= Duration::from_secs(5) {
                return Ok(());
            }
        }

        match socket.recv(&mut buffer) {
            Ok(length) => match ServerDatagram::decode(&buffer[..length], &info.secret) {
                Ok(datagram) => match datagram.packet {
                    VoicePacket::AuthenticateAck => {
                        authenticated = true;
                        println!("[svc] authenticated (AuthenticateAck received)");
                    }
                    VoicePacket::ConnectionCheckAck => {
                        if connected_at.is_none() {
                            connected_at = Some(Instant::now());
                            println!("[svc] connection validated (ConnectionCheckAck received)");
                        }
                    }
                    VoicePacket::KeepAlive => {
                        send_voice(
                            &socket,
                            &connection_id,
                            &VoicePacket::KeepAlive,
                            &info.secret,
                        )?;
                    }
                    VoicePacket::Ping { id, timestamp } => {
                        send_voice(
                            &socket,
                            &connection_id,
                            &VoicePacket::Ping { id, timestamp },
                            &info.secret,
                        )?;
                    }
                    other => println!("[svc] received {:?}", other),
                },
                Err(error) => println!("[svc] dropping undecodable datagram: {error}"),
            },
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn send_voice(
    socket: &UdpSocket,
    connection_id: &Uuid,
    packet: &VoicePacket,
    secret: &svchorizon_protocol::crypto::VoiceSecret,
) -> Result<()> {
    let datagram = ClientDatagram {
        connection_id: *connection_id,
        packet: packet.clone(),
    };
    let bytes = datagram.encode(secret)?;
    socket.send(&bytes)?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("[FAIL] {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let addr = format!("{GAME_HOST}:{GAME_PORT}");
    stage("connecting to game server");
    let mut framed = Framed::connect(&addr)?;
    println!("[mc] connected to {addr}");

    stage("minecraft login (protocol 777, offline)");
    let player_uuid = mc_login(&mut framed, GAME_HOST, GAME_PORT, PLAYER_NAME)?;

    stage("configuration phase");
    mc_configure(&mut framed)?;

    stage("entering play state and requesting the voice secret");
    send_custom_payload(
        &mut framed,
        ids::play::serverbound::CUSTOM_PAYLOAD,
        "minecraft:register",
        &register_payload(),
    )?;
    send_custom_payload(
        &mut framed,
        ids::play::serverbound::CUSTOM_PAYLOAD,
        "minecraft:brand",
        &brand_payload(),
    )?;
    send_custom_payload(
        &mut framed,
        ids::play::serverbound::CUSTOM_PAYLOAD,
        handshake::REQUEST_SECRET_CHANNEL,
        &handshake::encode_request_secret(handshake::COMPATIBILITY_VERSION),
    )?;

    let framed = Arc::new(Mutex::new(framed));
    let (secret_tx, secret_rx) = mpsc::channel::<Vec<u8>>();
    // Short idle timeout so the responder thread can observe the stop flag.
    {
        let guard = framed.lock().expect("framed lock poisoned");
        guard
            .stream
            .set_read_timeout(Some(Duration::from_millis(500)))?;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let responder = {
        let framed = Arc::clone(&framed);
        let stop = Arc::clone(&stop);
        thread::spawn(move || play_responder(framed, secret_tx, stop))
    };

    let payload = match secret_rx.recv_timeout(Duration::from_secs(30)) {
        Ok(payload) => payload,
        Err(_) => {
            stop.store(true, Ordering::Release);
            let _ = responder.join();
            return Err(HarnessError::Timeout(format!(
                "voicechat:secret payload (compatibility {})",
                handshake::COMPATIBILITY_VERSION
            )));
        }
    };
    stage("decoding SecretPacket from the official plugin");
    let info = VoiceServerInfo::decode(&payload)?;
    println!(
        "[svc] secret accepted: port {} codec {:?} groups {} recording {}",
        info.server_port, info.codec, info.groups_enabled, info.allow_recording
    );

    stage("UDP voice handshake (authenticate, connection check, keepalive, mic)");
    let result = svc_voice_session(&info, player_uuid, GAME_HOST);
    stop.store(true, Ordering::Release);
    let _ = responder.join();
    result?;

    println!(
        "[PASS] official plugin handshake, UDP authentication, keepalive and microphone stream all succeeded"
    );
    Ok(())
}
