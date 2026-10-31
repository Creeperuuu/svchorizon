//! Simple Voice Chat UDP client, AES-GCM envelope and Opus frame pipeline.
//! UDP and audio workers are independent of Horizon's game thread. CPAL
//! captures mono PCM and plays decoded Opus frames; callers control PTT.

use crate::{Application, OpusDecoder, OpusEncoder};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    error::Error,
    fmt,
    net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAGIC: u8 = 0xff;
const MAX_DATAGRAM: usize = 2048;
const MAX_OPUS: usize = 1275;
const FRAME_SAMPLES: usize = 960;
const SAMPLE_RATE: i32 = 48_000;
const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

const MIC: u8 = 0x01;
const PLAYER_SOUND: u8 = 0x02;
const GROUP_SOUND: u8 = 0x03;
const LOCATION_SOUND: u8 = 0x04;
const AUTHENTICATE: u8 = 0x05;
const AUTHENTICATE_ACK: u8 = 0x06;
const PING: u8 = 0x07;
const KEEP_ALIVE: u8 = 0x08;
const CONNECTION_CHECK: u8 = 0x09;
const CONNECTION_CHECK_ACK: u8 = 0x0a;

#[derive(Clone, Debug)]
pub struct VoiceServerInfo {
    pub secret: [u8; 16],
    pub port: u16,
    pub player: [u8; 16],
    /// 0 = VOIP, 1 = AUDIO, 2 = RESTRICTED_LOWDELAY.
    pub codec: u8,
    pub mtu: i32,
    pub distance: f64,
    /// Simple Voice Chat's configured keepalive interval, in milliseconds.
    pub keep_alive_ms: i32,
    pub groups: bool,
    pub host: String,
    pub allow_recording: bool,
}

#[derive(Debug)]
pub enum VoiceEvent {
    Connected,
    Disconnected(String),
    AudioUnavailable(String),
}

#[derive(Clone, Copy, Debug, Default)]
struct ListenerPose {
    x: f64,
    y: f64,
    z: f64,
    yaw: f64,
    active: bool,
}

#[derive(Clone, Copy)]
struct PlayerAudioSettings {
    volume: f32,
    muted: bool,
}

impl Default for PlayerAudioSettings {
    fn default() -> Self { Self { volume: 1.0, muted: false } }
}

struct PendingAudio {
    sender: [u8; 16],
    sequence: i64,
    opus: Vec<u8>,
    position: Option<[f64; 3]>,
    max_distance: Option<f32>,
}

struct SenderJitter {
    sender: [u8; 16],
    expected: Option<i64>,
    pending: BTreeMap<i64, PendingAudio>,
    gap_since: Option<Instant>,
    last_seen: Instant,
    last_position: Option<[f64; 3]>,
    last_distance: Option<f32>,
}

#[derive(Default)]
struct AudioJitterBuffer {
    senders: HashMap<[u8; 16], SenderJitter>,
}

impl AudioJitterBuffer {
    const MAX_SENDERS: usize = 128;
    const MAX_PENDING_PER_SENDER: usize = 8;
    const REORDER_DELAY: Duration = Duration::from_millis(40);
    const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

    fn push(&mut self, packet: PendingAudio, now: Instant) -> Vec<PendingAudio> {
        self.senders.retain(|_, state| now.duration_since(state.last_seen) < Self::IDLE_TIMEOUT);
        if !self.senders.contains_key(&packet.sender) && self.senders.len() >= Self::MAX_SENDERS {
            return Vec::new();
        }

        let state = self.senders.entry(packet.sender).or_insert_with(|| SenderJitter {
            sender: packet.sender,
            expected: Some(packet.sequence),
            pending: BTreeMap::new(),
            gap_since: None,
            last_seen: now,
            last_position: packet.position,
            last_distance: packet.max_distance,
        });
        state.last_seen = now;
        state.last_position = packet.position;
        state.last_distance = packet.max_distance;
        let expected = state.expected.unwrap_or(packet.sequence);
        if packet.sequence < expected {
            return Vec::new();
        }
        if packet.sequence != expected
            && !state.pending.contains_key(&packet.sequence)
            && state.pending.len() >= Self::MAX_PENDING_PER_SENDER
        {
            return Vec::new();
        }
        state.pending.entry(packet.sequence).or_insert(packet);
        Self::drain_ready(state, now)
    }

    fn poll(&mut self, now: Instant) -> Vec<PendingAudio> {
        self.senders.retain(|_, state| {
            now.duration_since(state.last_seen) < Self::IDLE_TIMEOUT || !state.pending.is_empty()
        });
        let mut ready = Vec::new();
        for state in self.senders.values_mut() {
            ready.extend(Self::drain_ready(state, now));
        }
        ready
    }

    fn drain_ready(state: &mut SenderJitter, now: Instant) -> Vec<PendingAudio> {
        let mut ready = Vec::new();
        loop {
            let Some(expected) = state.expected else { break; };
            if let Some(packet) = state.pending.remove(&expected) {
                state.expected = Some(expected.wrapping_add(1));
                state.gap_since = None;
                ready.push(packet);
                continue;
            }
            let Some(next_sequence) = state.pending.keys().next().copied() else {
                state.gap_since = None;
                break;
            };
            let gap_since = *state.gap_since.get_or_insert(now);
            if now.duration_since(gap_since) < Self::REORDER_DELAY {
                break;
            }
            // A sequence gap lasted beyond the reorder window. Ask Opus
            // to conceal a short loss burst, then skip any larger outage.
            let missing = next_sequence.saturating_sub(expected).clamp(0, 3) as usize;
            for _ in 0..missing {
                ready.push(PendingAudio {
                    sender: state.sender,
                    sequence: state.expected.unwrap_or(expected),
                    opus: Vec::new(),
                    position: state.last_position,
                    max_distance: state.last_distance,
                });
                state.expected = Some(state.expected.unwrap_or(expected).wrapping_add(1));
            }
            if state.expected.is_none_or(|value| value < next_sequence) {
                state.expected = Some(next_sequence);
            }
            state.gap_since = None;
        }
        ready
    }
}

struct AudioFrame {
    sender: [u8; 16],
    pcm: [i16; FRAME_SAMPLES],
    position: Option<[f64; 3]>,
    max_distance: Option<f32>,
}

#[derive(Debug)]
pub enum VoiceError {
    Io(std::io::Error),
    Resolve(String),
    InvalidServerInfo(&'static str),
    UnsupportedCodec(u8),
    WorkerStopped,
}

impl fmt::Display for VoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "voice UDP I/O error: {e}"),
            Self::Resolve(s) => write!(f, "could not resolve voice server: {s}"),
            Self::InvalidServerInfo(s) => write!(f, "invalid voice server metadata: {s}"),
            Self::UnsupportedCodec(c) => write!(f, "unsupported Simple Voice Chat codec ordinal {c}"),
            Self::WorkerStopped => write!(f, "voice worker has stopped"),
        }
    }
}
impl Error for VoiceError {}
impl From<std::io::Error> for VoiceError {
    fn from(value: std::io::Error) -> Self { Self::Io(value) }
}

enum Command {
    Pcm([i16; FRAME_SAMPLES]),
    Shutdown,
}

pub struct VoiceClient {
    command_tx: SyncSender<Command>,
    event_rx: Receiver<VoiceEvent>,
    running: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    whispering: Arc<AtomicBool>,
    voice_activation: Arc<AtomicBool>,
    voice_activation_threshold: Arc<AtomicU32>,
    microphone_gain: Arc<AtomicU32>,
    connected: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    audio_worker: Option<JoinHandle<()>>,
    listener: Arc<Mutex<ListenerPose>>,
    player_positions: Arc<RwLock<HashMap<[u8; 16], [f64; 3]>>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
}

impl VoiceClient {
    pub fn connect(info: VoiceServerInfo, fallback_host: &str) -> Result<Self, VoiceError> {
        if info.port == 0 { return Err(VoiceError::InvalidServerInfo("UDP port is zero")); }
        if info.keep_alive_ms <= 0 { return Err(VoiceError::InvalidServerInfo("keepalive interval is not positive")); }
        let application = match info.codec {
            0 => Application::Voip,
            1 => Application::Audio,
            2 => Application::RestrictedLowDelay,
            other => return Err(VoiceError::UnsupportedCodec(other)),
        };
        // Fail early if the Opus encoder cannot be constructed for the selected
        // upstream codec application.
        OpusEncoder::new(SAMPLE_RATE, 1, application)
            .map_err(|_| VoiceError::InvalidServerInfo("Opus encoder initialization failed"))?;

        // The upstream voice_host setting accepts an empty value, a port
        // override by itself, a hostname, or hostname:port. In particular,
        // treating "24454" as a hostname breaks servers that configure only
        // a custom UDP port.
        let address = resolve_voice_address(&info.host, fallback_host, info.port)?;
        let bind_address = if address.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socket = UdpSocket::bind(bind_address)?;
        socket.connect(address)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;

        let (command_tx, command_rx) = mpsc::sync_channel(8);
        let (event_tx, event_rx) = mpsc::sync_channel(32);
        let (audio_tx, audio_rx) = mpsc::sync_channel(8);
        let listener = Arc::new(Mutex::new(ListenerPose::default()));
        let player_positions = Arc::new(RwLock::new(HashMap::new()));
        let player_audio_settings = Arc::new(RwLock::new(HashMap::new()));
        let running = Arc::new(AtomicBool::new(true));
        let transmitting = Arc::new(AtomicBool::new(false));
        let whispering = Arc::new(AtomicBool::new(false));
        let voice_activation = Arc::new(AtomicBool::new(false));
        let voice_activation_threshold = Arc::new(AtomicU32::new(0.025f32.to_bits()));
        let microphone_gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let connected = Arc::new(AtomicBool::new(false));

        let audio_running = Arc::clone(&running);
        let audio_transmitting = Arc::clone(&transmitting);
        let audio_command_tx = command_tx.clone();
        let audio_events = event_tx.clone();
        let audio_listener = Arc::clone(&listener);
        let audio_settings = Arc::clone(&player_audio_settings);
        let audio_voice_activation = Arc::clone(&voice_activation);
        let audio_voice_threshold = Arc::clone(&voice_activation_threshold);
        let audio_microphone_gain = Arc::clone(&microphone_gain);
        let audio_worker = thread::Builder::new()
            .name("svchorizon-audio".into())
            .spawn(move || run_audio_io(
                audio_command_tx, audio_rx, audio_running, audio_transmitting, audio_events, audio_listener,
                audio_settings, audio_voice_activation, audio_voice_threshold, audio_microphone_gain,
            ))
            .map_err(VoiceError::Io)?;

        let worker_running = Arc::clone(&running);
        let worker_transmitting = Arc::clone(&transmitting);
        let worker_whispering = Arc::clone(&whispering);
        let worker_voice_activation = Arc::clone(&voice_activation);
        let worker_connected = Arc::clone(&connected);
        let worker_positions = Arc::clone(&player_positions);
        let worker = match thread::Builder::new()
            .name("svchorizon-udp".into())
            .spawn(move || {
                run_worker(
                    socket, info, application, command_rx, event_tx, audio_tx,
                    worker_running, worker_transmitting, worker_whispering, worker_voice_activation,
                    worker_connected, worker_positions,
                );
            }) {
            Ok(worker) => worker,
            Err(error) => {
                running.store(false, Ordering::Release);
                if let Some(audio) = audio_worker { let _ = audio.join(); }
                return Err(VoiceError::Io(error));
            }
        };

        Ok(Self {
            command_tx, event_rx, running, transmitting, whispering,
            voice_activation, voice_activation_threshold, microphone_gain, connected,
            worker: Some(worker), audio_worker, listener, player_positions, player_audio_settings,
        })
    }

    /// Enable only while push-to-talk is held or voice activation is active.
    pub fn set_transmitting(&self, enabled: bool) {
        self.transmitting.store(enabled, Ordering::Relaxed);
    }

    /// Enable the official Simple Voice Chat whispering flag on outgoing frames.
    pub fn set_whispering(&self, enabled: bool) {
        self.whispering.store(enabled, Ordering::Relaxed);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn is_transmitting(&self) -> bool {
        self.transmitting.load(Ordering::Relaxed)
    }

    pub fn is_whispering(&self) -> bool {
        self.whispering.load(Ordering::Relaxed)
    }

    /// Enable voice activation and set its normalized RMS threshold (0.0..=1.0).
    pub fn set_voice_activation(&self, enabled: bool, threshold: f32) {
        let threshold = if threshold.is_finite() { threshold.clamp(0.0, 1.0) } else { 0.025 };
        self.voice_activation_threshold.store(threshold.to_bits(), Ordering::Relaxed);
        self.voice_activation.store(enabled, Ordering::Relaxed);
    }

    pub fn is_voice_activation_enabled(&self) -> bool {
        self.voice_activation.load(Ordering::Relaxed)
    }

    /// Set microphone amplification from 0.0 (silent) through 4.0.
    pub fn set_microphone_gain(&self, gain: f32) {
        let gain = if gain.is_finite() { gain.clamp(0.0, 4.0) } else { 1.0 };
        self.microphone_gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Set a speaker's playback level. Values are clamped to 0.0..=2.0.
    pub fn set_player_volume(&self, player: [u8; 16], volume: f32) {
        if let Ok(mut settings) = self.player_audio_settings.write() {
            if !settings.contains_key(&player) && settings.len() >= 128 { return; }
            let remove = {
                let entry = settings.entry(player).or_default();
                entry.volume = if volume.is_finite() { volume.clamp(0.0, 2.0) } else { 1.0 };
                entry.volume == 1.0 && !entry.muted
            };
            if remove { settings.remove(&player); }
        }
    }

    /// Mute or unmute one remote speaker without affecting other players.
    pub fn set_player_muted(&self, player: [u8; 16], muted: bool) {
        if let Ok(mut settings) = self.player_audio_settings.write() {
            if !settings.contains_key(&player) && settings.len() >= 128 { return; }
            let remove = {
                let entry = settings.entry(player).or_default();
                entry.muted = muted;
                entry.volume == 1.0 && !entry.muted
            };
            if remove { settings.remove(&player); }
        }
    }

    pub fn set_listener(&self, x: f64, y: f64, z: f64, yaw_degrees: f32) {
        if let Ok(mut pose) = self.listener.lock() {
            *pose = ListenerPose { x, y, z, yaw: f64::from(yaw_degrees).to_radians(), active: true };
        }
    }

    pub fn clear_listener(&self) {
        if let Ok(mut pose) = self.listener.lock() { pose.active = false; }
    }

    pub fn set_player_positions(&self, positions: Vec<([u8; 16], [f64; 3])>) {
        if let Ok(mut current) = self.player_positions.write() {
            *current = positions.into_iter().collect();
        }
    }

    /// Queue exactly one 20 ms mono 48 kHz PCM frame. Returns false when muted
    /// or when the bounded worker queue is full; the caller must not block.
    pub fn send_pcm(&self, frame: [i16; FRAME_SAMPLES]) -> bool {
        if !self.transmitting.load(Ordering::Relaxed) || !self.running.load(Ordering::Acquire) {
            return false;
        }
        match self.command_tx.try_send(Command::Pcm(frame)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub fn poll_event(&self) -> Option<VoiceEvent> {
        match self.event_rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.command_tx.try_send(Command::Shutdown);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
        if let Some(worker) = self.audio_worker.take() { let _ = worker.join(); }
    }
}

impl Drop for VoiceClient {
    fn drop(&mut self) { self.shutdown(); }
}

fn resolve_voice_address(
    voice_host: &str,
    game_server_host: &str,
    default_port: u16,
) -> Result<SocketAddr, VoiceError> {
    let fallback_host = split_host_port(game_server_host.trim())?.0;
    let (host, port) = if voice_host.trim().is_empty() {
        (fallback_host, default_port)
    } else if let Ok(override_port) = voice_host.trim().parse::<u16>() {
        if override_port == 0 {
            return Err(VoiceError::Resolve("voice_host port is zero".into()));
        }
        (fallback_host, override_port)
    } else {
        let (host, override_port) = split_host_port(voice_host.trim())?;
        (host, override_port.unwrap_or(default_port))
    };

    if port == 0 {
        return Err(VoiceError::Resolve("voice UDP port is zero".into()));
    }
    let host = normalize_host(host)?;
    if host == "rtc-local" || host == "rtc-remote" {
        return Ok(SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    (host.as_str(), port)
        .to_socket_addrs()
        .map_err(VoiceError::Io)?
        .next()
        .ok_or_else(|| VoiceError::Resolve(format!("{host}:{port}")))
}

/// Split a Minecraft-style host authority without accidentally treating the
/// port of the game server as the voice UDP port.
fn split_host_port(value: &str) -> Result<(&str, Option<u16>), VoiceError> {
    if let Some(rest) = value.strip_prefix('[') {
        let close = rest.find(']')
            .ok_or_else(|| VoiceError::Resolve(format!("invalid IPv6 authority: {value}")))?;
        let host = &rest[..close];
        let suffix = &rest[close + 1..];
        if suffix.is_empty() {
            return Ok((host, None));
        }
        let port_text = suffix.strip_prefix(':')
            .ok_or_else(|| VoiceError::Resolve(format!("invalid host authority: {value}")))?;
        let port = port_text.parse::<u16>()
            .ok().filter(|port| *port != 0)
            .ok_or_else(|| VoiceError::Resolve(format!("invalid port in authority: {value}")))?;
        return Ok((host, Some(port)));
    }

    if value.matches(':').count() == 1 {
        let (host, port_text) = value.split_once(':').expect("one colon was counted");
        let port = port_text.parse::<u16>()
            .ok().filter(|port| *port != 0)
            .ok_or_else(|| VoiceError::Resolve(format!("invalid port in authority: {value}")))?;
        return Ok((host, Some(port)));
    }

    // An unbracketed IPv6 literal has multiple colons and no port suffix.
    Ok((value, None))
}

fn normalize_host(value: &str) -> Result<String, VoiceError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(VoiceError::Resolve("empty voice server host".into()));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(VoiceError::Resolve("voice server host contains whitespace".into()));
    }
    Ok(value.to_owned())
}

fn run_worker(
    socket: UdpSocket,
    info: VoiceServerInfo,
    application: Application,
    commands: Receiver<Command>,
    events: SyncSender<VoiceEvent>,
    audio: SyncSender<AudioFrame>,
    running: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    whispering: Arc<AtomicBool>,
    voice_activation: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    player_positions: Arc<RwLock<HashMap<[u8; 16], [f64; 3]>>>,
) {
    let mut encoder = match OpusEncoder::new(SAMPLE_RATE, 1, application) {
        Ok(value) => value,
        Err(_) => {
            let _ = events.try_send(VoiceEvent::Disconnected("Opus encoder initialization failed".into()));
            return;
        }
    };
    encoder.use_inband_fec = true;
    encoder.packet_loss_perc = 5;
    let payload_limit = (info.mtu as usize).clamp(2, MAX_OPUS);
    let mut decoders: HashMap<[u8; 16], OpusDecoder> = HashMap::new();
    let mut jitter = AudioJitterBuffer::default();
    let mut authenticated = false;
    let mut validated = false;
    let mut sequence = 0i64;
    let mut last_auth: Option<Instant> = None;
    let mut last_check: Option<Instant> = None;
    let mut last_keepalive = Instant::now();
    let keepalive_timeout = Duration::from_millis((info.keep_alive_ms as u64).saturating_mul(10));
    let mut receive = [0u8; MAX_DATAGRAM];

    while running.load(Ordering::Acquire) {
        loop {
            match commands.try_recv() {
                Ok(Command::Pcm(pcm)) => {
                    if validated && (
                        transmitting.load(Ordering::Relaxed)
                            || voice_activation.load(Ordering::Relaxed)
                    ) {
                        let mut opus = [0u8; 1276];
                        if let Ok(length) = encoder.encode_i16(&pcm, FRAME_SAMPLES, &mut opus[..payload_limit]) {
                            let packet = Packet::Mic {
                                opus: opus[..length].to_vec(),
                                sequence,
                                whispering: whispering.load(Ordering::Relaxed),
                            };
                            sequence = sequence.wrapping_add(1);
                            let _ = send_packet(&socket, &info, &packet);
                        }
                    }
                }
                Ok(Command::Shutdown) => {
                    running.store(false, Ordering::Release);
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running.store(false, Ordering::Release);
                    break;
                }
            }
        }
        if !running.load(Ordering::Acquire) { break; }

        if !authenticated && last_auth.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)) {
            let packet = Packet::Authenticate { player: info.player, secret: info.secret };
            let _ = send_packet(&socket, &info, &packet);
            last_auth = Some(Instant::now());
        } else if authenticated && !validated
            && last_check.is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            let _ = send_packet(&socket, &info, &Packet::ConnectionCheck);
            last_check = Some(Instant::now());
        }

        if validated && last_keepalive.elapsed() > keepalive_timeout {
            connected.store(false, Ordering::Release);
            let _ = events.try_send(VoiceEvent::Disconnected("voice server keepalive timed out".into()));
            break;
        }

        match socket.recv(&mut receive) {
            Ok(length) => {
                let Some(packet) = decode_server_datagram(&receive[..length], &info.secret) else {
                    continue;
                };
                match packet {
                    Packet::AuthenticateAck => {
                        authenticated = true;
                        last_check = None;
                    }
                    Packet::ConnectionCheckAck => {
                        if !validated {
                            validated = true;
                            connected.store(true, Ordering::Release);
                            last_keepalive = Instant::now();
                            let _ = events.try_send(VoiceEvent::Connected);
                        }
                    }
                    Packet::KeepAlive => {
                        last_keepalive = Instant::now();
                        let _ = send_packet(&socket, &info, &Packet::KeepAlive);
                    }
                    Packet::Ping { id, timestamp } => {
                        let _ = send_packet(&socket, &info, &Packet::Ping { id, timestamp });
                    }
                    Packet::PlayerSound { sender, opus, sequence, distance, .. } => {
                        if validated {
                            let position = player_positions.read().ok()
                                .and_then(|positions| positions.get(&sender).copied());
                            let ready = jitter.push(PendingAudio {
                                sender, sequence, opus, position, max_distance: Some(distance),
                            }, Instant::now());
                            for frame in ready {
                                emit_audio(&mut decoders, &audio, frame);
                            }
                        }
                    }
                    Packet::GroupSound { sender, opus, sequence, .. } => {
                        if validated {
                            let ready = jitter.push(PendingAudio {
                                sender, sequence, opus, position: None, max_distance: None,
                            }, Instant::now());
                            for frame in ready {
                                emit_audio(&mut decoders, &audio, frame);
                            }
                        }
                    }
                    Packet::LocationSound { sender, location, opus, sequence, distance, .. } => {
                        if validated {
                            let ready = jitter.push(PendingAudio {
                                sender, sequence, opus, position: Some(location),
                                max_distance: Some(distance),
                            }, Instant::now());
                            for frame in ready {
                                emit_audio(&mut decoders, &audio, frame);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Err(error) if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
            ) => {}
            Err(error) => {
                connected.store(false, Ordering::Release);
                let _ = events.try_send(VoiceEvent::Disconnected(format!("UDP socket failed: {error}")));
                break;
            }
        }
        for frame in jitter.poll(Instant::now()) {
            emit_audio(&mut decoders, &audio, frame);
        }
        decoders.retain(|sender, _| jitter.senders.contains_key(sender));
    }
    connected.store(false, Ordering::Release);
    running.store(false, Ordering::Release);
}

fn emit_audio(
    decoders: &mut HashMap<[u8; 16], OpusDecoder>,
    audio: &SyncSender<AudioFrame>,
    pending: PendingAudio,
) {
    if pending.opus.len() > MAX_OPUS { return; }
    if !decoders.contains_key(&pending.sender) {
        let Ok(decoder) = OpusDecoder::new(SAMPLE_RATE, 1) else { return; };
        decoders.insert(pending.sender, decoder);
    }
    let Some(decoder) = decoders.get_mut(&pending.sender) else { return; };
    let mut float_pcm = [0.0f32; FRAME_SAMPLES];
    let Ok(samples) = decoder.decode(&pending.opus, FRAME_SAMPLES, &mut float_pcm) else { return; };
    let mut pcm = [0i16; FRAME_SAMPLES];
    for (out, input) in pcm.iter_mut().zip(float_pcm.iter()).take(samples.min(FRAME_SAMPLES)) {
        *out = (input.clamp(-1.0, 1.0) * 32767.0) as i16;
    }
    let _ = audio.try_send(AudioFrame {
        sender: pending.sender,
        pcm,
        position: pending.position,
        max_distance: pending.max_distance,
    });
}

fn run_audio_io(
    command_tx: SyncSender<Command>,
    audio_rx: Receiver<AudioFrame>,
    running: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    events: SyncSender<VoiceEvent>,
    listener: Arc<Mutex<ListenerPose>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
    voice_activation: Arc<AtomicBool>,
    voice_activation_threshold: Arc<AtomicU32>,
    microphone_gain: Arc<AtomicU32>,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let mut streams = Vec::new();

    match host.default_input_device() {
        Some(device) => match build_input_stream(
            &device, command_tx, transmitting, voice_activation, voice_activation_threshold,
            microphone_gain, events.clone(),
        ) {
            Ok(stream) => if let Err(error) = stream.play() {
                let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("could not start microphone: {error}")));
            } else { streams.push(stream); },
            Err(error) => { let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("microphone unavailable: {error}"))); }
        },
        None => { let _ = events.try_send(VoiceEvent::AudioUnavailable("no default microphone device".into())); }
    }

    match host.default_output_device() {
        Some(device) => match build_output_stream(&device, audio_rx, events.clone(), listener, player_audio_settings) {
            Ok(stream) => if let Err(error) = stream.play() {
                let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("could not start speaker: {error}")));
            } else { streams.push(stream); },
            Err(error) => { let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("speaker unavailable: {error}"))); }
        },
        None => { let _ = events.try_send(VoiceEvent::AudioUnavailable("no default speaker device".into())); }
    }

    while running.load(Ordering::Acquire) { thread::sleep(Duration::from_millis(50)); }
    drop(streams);
}

struct InputAssembler {
    input_rate: u32,
    phase: u64,
    previous: f32,
    has_previous: bool,
    frame: [i16; FRAME_SAMPLES],
    frame_pos: usize,
    tx: SyncSender<Command>,
    transmitting: Arc<AtomicBool>,
    voice_activation: Arc<AtomicBool>,
    voice_activation_threshold: Arc<AtomicU32>,
    microphone_gain: Arc<AtomicU32>,
}
impl InputAssembler {
    fn new(
        input_rate: u32,
        tx: SyncSender<Command>,
        transmitting: Arc<AtomicBool>,
        voice_activation: Arc<AtomicBool>,
        voice_activation_threshold: Arc<AtomicU32>,
        microphone_gain: Arc<AtomicU32>,
    ) -> Self {
        Self {
            input_rate: input_rate.max(1),
            phase: 0,
            previous: 0.0,
            has_previous: false,
            frame: [0; FRAME_SAMPLES],
            frame_pos: 0,
            tx,
            transmitting,
            voice_activation,
            voice_activation_threshold,
            microphone_gain,
        }
    }
    fn push(&mut self, sample: f32) {
        let gain = f32::from_bits(self.microphone_gain.load(Ordering::Relaxed));
        let sample = (sample * gain).clamp(-1.0, 1.0);
        if !self.has_previous { self.previous = sample; self.has_previous = true; return; }
        self.phase += SAMPLE_RATE as u64;
        while self.phase >= u64::from(self.input_rate) {
            self.phase -= u64::from(self.input_rate);
            let fraction = 1.0 - self.phase as f32 / SAMPLE_RATE as f32;
            let value = self.previous + (sample - self.previous) * fraction;
            self.frame[self.frame_pos] = (value * 32767.0) as i16;
            self.frame_pos += 1;
            if self.frame_pos == FRAME_SAMPLES {
                let ptt = self.transmitting.load(Ordering::Relaxed);
                let voice_activation = self.voice_activation.load(Ordering::Relaxed);
                let should_send = if ptt {
                    true
                } else if voice_activation {
                    let energy = self.frame.iter()
                        .map(|sample| {
                            let normalized = *sample as f32 / 32768.0;
                            normalized * normalized
                        })
                        .sum::<f32>() / FRAME_SAMPLES as f32;
                    let rms = energy.sqrt();
                    let threshold = f32::from_bits(self.voice_activation_threshold.load(Ordering::Relaxed));
                    rms >= threshold
                } else {
                    false
                };
                if should_send {
                    let _ = self.tx.try_send(Command::Pcm(self.frame));
                }
                self.frame = [0; FRAME_SAMPLES];
                self.frame_pos = 0;
            }
        }
        self.previous = sample;
    }
}

fn build_input_stream(
    device: &cpal::Device,
    tx: SyncSender<Command>,
    transmitting: Arc<AtomicBool>,
    voice_activation: Arc<AtomicBool>,
    voice_activation_threshold: Arc<AtomicU32>,
    microphone_gain: Arc<AtomicU32>,
    events: SyncSender<VoiceEvent>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = usize::from(config.channels).max(1);
    let rate = config.sample_rate.0;
    match format {
        cpal::SampleFormat::F32 => {
            let mut input = InputAssembler::new(
                rate, tx, transmitting, Arc::clone(&voice_activation), Arc::clone(&voice_activation_threshold),
                Arc::clone(&microphone_gain),
            );
            let errors = events;
            device.build_input_stream(&config, move |data: &[f32], _| {
                for frame in data.chunks(channels) {
                    input.push(frame.iter().copied().sum::<f32>() / frame.len().max(1) as f32);
                }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("microphone stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        cpal::SampleFormat::I16 => {
            let mut input = InputAssembler::new(
                rate, tx, transmitting, Arc::clone(&voice_activation), Arc::clone(&voice_activation_threshold),
                Arc::clone(&microphone_gain),
            );
            let errors = events;
            device.build_input_stream(&config, move |data: &[i16], _| {
                for frame in data.chunks(channels) {
                    let mono = frame.iter().map(|s| *s as f32 / 32768.0).sum::<f32>()
                        / frame.len().max(1) as f32;
                    input.push(mono);
                }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("microphone stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        cpal::SampleFormat::U16 => {
            let mut input = InputAssembler::new(rate, tx, transmitting, Arc::clone(&voice_activation), Arc::clone(&voice_activation_threshold), Arc::clone(&microphone_gain));
            let errors = events;
            device.build_input_stream(&config, move |data: &[u16], _| {
                for frame in data.chunks(channels) {
                    let mono = frame.iter().map(|s| (*s as f32 - 32768.0) / 32768.0).sum::<f32>()
                        / frame.len().max(1) as f32;
                    input.push(mono);
                }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("microphone stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        other => Err(format!("unsupported microphone sample format {other:?}")),
    }
}

struct PlaybackFrame {
    frame: AudioFrame,
    position: f64,
    left_gain: f32,
    right_gain: f32,
}

struct OutputAssembler {
    rx: Receiver<AudioFrame>,
    speakers: HashMap<[u8; 16], VecDeque<PlaybackFrame>>,
    step: f64,
    listener: Arc<Mutex<ListenerPose>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
}

impl OutputAssembler {
    const MAX_SPEAKERS: usize = 32;
    const MAX_QUEUED_FRAMES_PER_SPEAKER: usize = 6;

    fn new(
        rx: Receiver<AudioFrame>,
        output_rate: u32,
        listener: Arc<Mutex<ListenerPose>>,
        player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
    ) -> Self {
        Self {
            rx,
            speakers: HashMap::with_capacity(8),
            step: SAMPLE_RATE as f64 / f64::from(output_rate.max(1)),
            listener,
            player_audio_settings,
        }
    }

    fn gains_for(&self, frame: &AudioFrame) -> (f32, f32) {
        let Some(source) = frame.position else { return (1.0, 1.0); };
        let Ok(listener) = self.listener.lock() else { return (1.0, 1.0); };
        if !listener.active { return (1.0, 1.0); }
        let dx = source[0] - listener.x;
        let dy = source[1] - listener.y;
        let dz = source[2] - listener.z;
        let distance = (dx * dx + dy * dy + dz * dz).sqrt();
        let max_distance = f64::from(frame.max_distance.unwrap_or(0.0));
        let attenuation = if max_distance > 0.0 {
            ((max_distance - distance) / max_distance).clamp(0.0, 1.0) as f32
        } else { 1.0 };
        let pan = if distance > 0.001 {
            ((dx * listener.yaw.cos() - dz * listener.yaw.sin()) / distance).clamp(-1.0, 1.0)
        } else { 0.0 };
        (
            attenuation * (1.0 - pan).clamp(0.0, 1.0),
            attenuation * (1.0 + pan).clamp(0.0, 1.0),
        )
    }

    /// Mix one current frame per speaker. Consecutive packets from one
    /// speaker remain sequential, while different speakers overlap.
    fn next_stereo(&mut self) -> [i16; 2] {
        for frame in self.rx.try_iter() {
            if !self.speakers.contains_key(&frame.sender)
                && self.speakers.len() >= Self::MAX_SPEAKERS
            {
                continue;
            }
            let (mut left_gain, mut right_gain) = self.gains_for(&frame);
            let settings = self.player_audio_settings.read().ok()
                .and_then(|all| all.get(&frame.sender).copied())
                .unwrap_or_default();
            if settings.muted {
                left_gain = 0.0;
                right_gain = 0.0;
            } else {
                left_gain *= settings.volume;
                right_gain *= settings.volume;
            }
            let queue = self.speakers.entry(frame.sender).or_default();
            if queue.len() >= Self::MAX_QUEUED_FRAMES_PER_SPEAKER {
                continue;
            }
            queue.push_back(PlaybackFrame {
                frame,
                position: 0.0,
                left_gain,
                right_gain,
            });
        }

        let mut left = 0.0f32;
        let mut right = 0.0f32;
        let step = self.step;
        self.speakers.retain(|_, queue| {
            if let Some(voice) = queue.front_mut() {
                if voice.position < FRAME_SAMPLES as f64 {
                    let index = voice.position.floor() as usize;
                    let next = (index + 1).min(FRAME_SAMPLES - 1);
                    let fraction = (voice.position - index as f64) as f32;
                    let pcm = &voice.frame.pcm;
                    let sample = pcm[index] as f32
                        + (pcm[next] as f32 - pcm[index] as f32) * fraction;
                    left += sample * voice.left_gain;
                    right += sample * voice.right_gain;
                    voice.position += step;
                }
                if voice.position >= FRAME_SAMPLES as f64 {
                    queue.pop_front();
                }
            }
            !queue.is_empty()
        });

        [
            left.clamp(i16::MIN as f32, i16::MAX as f32) as i16,
            right.clamp(i16::MIN as f32, i16::MAX as f32) as i16,
        ]
    }
}

fn build_output_stream(
    device: &cpal::Device,
    rx: Receiver<AudioFrame>,
    events: SyncSender<VoiceEvent>,
    listener: Arc<Mutex<ListenerPose>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = usize::from(config.channels).max(1);
    let rate = config.sample_rate.0;
    match format {
        cpal::SampleFormat::F32 => {
            let mut output = OutputAssembler::new(rx, rate, Arc::clone(&listener), Arc::clone(&player_audio_settings));
            let errors = events;
            device.build_output_stream(&config, move |data: &mut [f32], _| {
                for frame in data.chunks_mut(channels) {
                    let stereo = output.next_stereo();
                    if frame.len() == 1 {
                        frame[0] = (i32::from(stereo[0]) + i32::from(stereo[1])) as f32 / 65536.0;
                    } else {
                        frame[0] = stereo[0] as f32 / 32768.0;
                        frame[1] = stereo[1] as f32 / 32768.0;
                        for sample in frame.iter_mut().skip(2) { *sample = 0.0; }
                    }
                }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("speaker stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        cpal::SampleFormat::I16 => {
            let mut output = OutputAssembler::new(rx, rate, Arc::clone(&listener), Arc::clone(&player_audio_settings));
            let errors = events;
            device.build_output_stream(&config, move |data: &mut [i16], _| {
                for frame in data.chunks_mut(channels) {
                    let stereo = output.next_stereo();
                    if frame.len() == 1 { frame[0] = ((i32::from(stereo[0]) + i32::from(stereo[1])) / 2) as i16; }
                    else {
                        frame[0] = stereo[0]; frame[1] = stereo[1];
                        for sample in frame.iter_mut().skip(2) { *sample = 0; }
                    }
                }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("speaker stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        cpal::SampleFormat::U16 => {
            let mut output = OutputAssembler::new(rx, rate, listener, player_audio_settings);
            let errors = events;
            device.build_output_stream(&config, move |data: &mut [u16], _| {
                for frame in data.chunks_mut(channels) {
                    let stereo = output.next_stereo();
                    if frame.len() == 1 {
                        frame[0] = ((i32::from(stereo[0]) + i32::from(stereo[1])) / 2 + 32768) as u16;
                    } else {
                        frame[0] = (i32::from(stereo[0]) + 32768) as u16;
                        frame[1] = (i32::from(stereo[1]) + 32768) as u16;
                        for sample in frame.iter_mut().skip(2) { *sample = 32768; }
                    }
                }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("speaker stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        other => Err(format!("unsupported speaker sample format {other:?}")),
    }
}

#[derive(Debug)]
enum Packet {
    Mic { opus: Vec<u8>, sequence: i64, whispering: bool },
    Authenticate { player: [u8; 16], secret: [u8; 16] },
    ConnectionCheck,
    KeepAlive,
    AuthenticateAck,
    ConnectionCheckAck,
    Ping { id: [u8; 16], timestamp: i64 },
    PlayerSound {
        channel: [u8; 16], sender: [u8; 16], opus: Vec<u8>,
        sequence: i64, distance: f32, whispering: bool, category: Option<String>,
    },
    GroupSound {
        channel: [u8; 16], sender: [u8; 16], opus: Vec<u8>,
        sequence: i64, category: Option<String>,
    },
    LocationSound {
        channel: [u8; 16], sender: [u8; 16], location: [f64; 3],
        opus: Vec<u8>, sequence: i64, distance: f32, category: Option<String>,
    },
}

fn send_packet(socket: &UdpSocket, info: &VoiceServerInfo, packet: &Packet) -> Result<(), VoiceError> {
    let plain = encode_packet(packet).ok_or(VoiceError::InvalidServerInfo("invalid outbound packet"))?;
    let mut nonce = [0u8; NONCE_BYTES];
    SystemRandom::new().fill(&mut nonce)
        .map_err(|_| VoiceError::InvalidServerInfo("secure random generator failed"))?;
    let key = LessSafeKey::new(
        UnboundKey::new(&aead::AES_128_GCM, &info.secret)
            .map_err(|_| VoiceError::InvalidServerInfo("invalid AES-128 key"))?,
    );
    let mut encrypted = plain;
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut encrypted)
        .map_err(|_| VoiceError::InvalidServerInfo("AES-GCM encryption failed"))?;
    let mut datagram = Vec::with_capacity(1 + 16 + 3 + NONCE_BYTES + encrypted.len());
    datagram.push(MAGIC);
    datagram.extend_from_slice(&info.player);
    let mut body = Vec::with_capacity(NONCE_BYTES + encrypted.len());
    body.extend_from_slice(&nonce);
    body.extend_from_slice(&encrypted);
    write_byte_array(&mut datagram, &body);
    socket.send(&datagram)?;
    Ok(())
}

fn decode_server_datagram(datagram: &[u8], secret: &[u8; 16]) -> Option<Packet> {
    let mut reader = Reader::new(datagram);
    if reader.u8()? != MAGIC { return None; }
    let encrypted = reader.byte_array(MAX_DATAGRAM)?;
    if !reader.done() || encrypted.len() < NONCE_BYTES + TAG_BYTES { return None; }
    let nonce: [u8; NONCE_BYTES] = encrypted[..NONCE_BYTES].try_into().ok()?;
    let mut plaintext = encrypted[NONCE_BYTES..].to_vec();
    let key = LessSafeKey::new(UnboundKey::new(&aead::AES_128_GCM, secret).ok()?);
    let opened = key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut plaintext).ok()?;
    decode_packet(opened)
}

fn encode_packet(packet: &Packet) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match packet {
        Packet::Mic { opus, sequence, whispering } => {
            if opus.len() > MAX_OPUS { return None; }
            out.push(MIC); write_byte_array(&mut out, opus); out.extend_from_slice(&sequence.to_be_bytes());
            out.push(u8::from(*whispering));
        }
        Packet::Authenticate { player, secret } => {
            out.push(AUTHENTICATE); out.extend_from_slice(player); out.extend_from_slice(secret);
        }
        Packet::ConnectionCheck => out.push(CONNECTION_CHECK),
        Packet::KeepAlive => out.push(KEEP_ALIVE),
        Packet::Ping { id, timestamp } => {
            out.push(PING); out.extend_from_slice(id); out.extend_from_slice(&timestamp.to_be_bytes());
        }
        _ => return None,
    }
    Some(out)
}

fn decode_packet(bytes: &[u8]) -> Option<Packet> {
    let mut r = Reader::new(bytes);
    let packet = match r.u8()? {
        MIC => Packet::Mic { opus: r.byte_array(MAX_OPUS)?, sequence: r.i64()?, whispering: r.boolean()? },
        AUTHENTICATE => Packet::Authenticate { player: r.array16()?, secret: r.array16()? },
        CONNECTION_CHECK => Packet::ConnectionCheck,
        KEEP_ALIVE => Packet::KeepAlive,
        AUTHENTICATE_ACK => Packet::AuthenticateAck,
        CONNECTION_CHECK_ACK => Packet::ConnectionCheckAck,
        PING => Packet::Ping { id: r.array16()?, timestamp: r.i64()? },
        PLAYER_SOUND => {
            let channel = r.array16()?; let sender = r.array16()?;
            let opus = r.byte_array(MAX_OPUS)?; let sequence = r.i64()?;
            let distance = r.f32()?; let flags = r.u8()?;
            let category = if flags & 2 != 0 { Some(r.string(16)?) } else { None };
            Packet::PlayerSound { channel, sender, opus, sequence, distance, whispering: flags & 1 != 0, category }
        }
        GROUP_SOUND => {
            let channel = r.array16()?; let sender = r.array16()?;
            let opus = r.byte_array(MAX_OPUS)?; let sequence = r.i64()?;
            let flags = r.u8()?; let category = if flags & 2 != 0 { Some(r.string(16)?) } else { None };
            Packet::GroupSound { channel, sender, opus, sequence, category }
        }
        LOCATION_SOUND => {
            let channel = r.array16()?; let sender = r.array16()?;
            let location = [r.f64()?, r.f64()?, r.f64()?];
            let opus = r.byte_array(MAX_OPUS)?; let sequence = r.i64()?;
            let distance = r.f32()?; let flags = r.u8()?;
            let category = if flags & 2 != 0 { Some(r.string(16)?) } else { None };
            Packet::LocationSound { channel, sender, location, opus, sequence, distance, category }
        }
        _ => return None,
    };
    if r.done() { Some(packet) } else { None }
}

fn write_varint(out: &mut Vec<u8>, mut value: usize) {
    while value > 0x7f { out.push((value as u8 & 0x7f) | 0x80); value >>= 7; }
    out.push(value as u8);
}
fn write_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(out, bytes.len()); out.extend_from_slice(bytes);
}

struct Reader<'a> { bytes: &'a [u8], pos: usize }
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self { Self { bytes, pos: 0 } }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let result = self.bytes.get(self.pos..end)?; self.pos = end; Some(result)
    }
    fn u8(&mut self) -> Option<u8> { Some(*self.take(1)?.first()?) }
    fn boolean(&mut self) -> Option<bool> { match self.u8()? { 0 => Some(false), 1 => Some(true), _ => None } }
    fn array16(&mut self) -> Option<[u8; 16]> { self.take(16)?.try_into().ok() }
    fn i64(&mut self) -> Option<i64> { Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?)) }
    fn f32(&mut self) -> Option<f32> { Some(f32::from_bits(u32::from_be_bytes(self.take(4)?.try_into().ok()?))) }
    fn f64(&mut self) -> Option<f64> { Some(f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into().ok()?))) }
    fn varint(&mut self) -> Option<usize> {
        let mut value = 0usize;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 4 && b & 0xf0 != 0 { return None; }
            value |= usize::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 { return Some(value); }
        }
        None
    }
    fn byte_array(&mut self, max: usize) -> Option<Vec<u8>> {
        let len = self.varint()?;
        if len > max { return None; }
        Some(self.take(len)?.to_vec())
    }
    fn string(&mut self, max_chars: usize) -> Option<String> {
        let bytes = self.byte_array(max_chars.checked_mul(4)?)?;
        let value = String::from_utf8(bytes).ok()?;
        if value.chars().count() > max_chars { return None; }
        Some(value)
    }
    fn done(&self) -> bool { self.pos == self.bytes.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_voice_host_overrides_like_the_official_client() {
        // An empty voice_host uses the game server hostname but the separate
        // voice UDP port, never the Minecraft TCP port.
        assert_eq!(
            resolve_voice_address("", "127.0.0.1:25565", 24454).unwrap(),
            "127.0.0.1:24454".parse::<SocketAddr>().unwrap()
        );

        // A numeric voice_host overrides only the UDP port.
        assert_eq!(
            resolve_voice_address("25566", "127.0.0.1:25565", 24454).unwrap(),
            "127.0.0.1:25566".parse::<SocketAddr>().unwrap()
        );

        // A host:port voice_host overrides both host and UDP port.
        assert_eq!(
            resolve_voice_address("127.0.0.1:25567", "192.0.2.1:25565", 24454).unwrap(),
            "127.0.0.1:25567".parse::<SocketAddr>().unwrap()
        );

        // A host-only override keeps the packet's UDP port.
        assert_eq!(
            resolve_voice_address("127.0.0.1", "192.0.2.1:25565", 24454).unwrap(),
            "127.0.0.1:24454".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn resolves_bracketed_ipv6_voice_host() {
        assert_eq!(
            resolve_voice_address("[::1]:24455", "127.0.0.1:25565", 24454).unwrap(),
            "[::1]:24455".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn rejects_invalid_voice_host_port() {
        assert!(resolve_voice_address("127.0.0.1:0", "127.0.0.1:25565", 24454).is_err());
        assert!(resolve_voice_address("127.0.0.1:not-a-port", "127.0.0.1:25565", 24454).is_err());
    }

    #[test]
    fn outbound_auth_packet_matches_official_field_order() {
        let player = [0x11; 16];
        let secret = [0x22; 16];
        let encoded = encode_packet(&Packet::Authenticate { player, secret }).unwrap();
        assert_eq!(encoded[0], AUTHENTICATE);
        assert_eq!(&encoded[1..17], &player);
        assert_eq!(&encoded[17..33], &secret);
    }

    #[test]
    fn mixes_speakers_but_keeps_each_speakers_packets_sequential() {
        let (tx, rx) = mpsc::sync_channel(8);
        let listener = Arc::new(Mutex::new(ListenerPose::default()));
        let mut output = OutputAssembler::new(rx, SAMPLE_RATE as u32, listener, Arc::new(RwLock::new(HashMap::new())));

        let make_frame = |sender, sample| AudioFrame {
            sender,
            pcm: [sample; FRAME_SAMPLES],
            position: None,
            max_distance: None,
        };

        tx.send(make_frame([1; 16], 1_000)).unwrap();
        tx.send(make_frame([1; 16], 4_000)).unwrap();
        tx.send(make_frame([2; 16], 2_000)).unwrap();

        assert_eq!(output.next_stereo(), [3_000, 3_000]);
        for _ in 1..FRAME_SAMPLES - 1 {
            let _ = output.next_stereo();
        }
        assert_eq!(output.next_stereo(), [4_000, 4_000]);
    }

    #[test]
    fn default_player_audio_settings_are_unmuted() {
        let settings = PlayerAudioSettings::default();
        assert_eq!(settings.volume, 1.0);
        assert!(!settings.muted);
    }

    #[test]
    fn jitter_buffer_reorders_packets_for_each_speaker() {
        let now = Instant::now();
        let make = |sequence| PendingAudio {
            sender: [4; 16],
            sequence,
            opus: vec![sequence as u8],
            position: None,
            max_distance: None,
        };
        let mut jitter = AudioJitterBuffer::default();

        assert_eq!(jitter.push(make(10), now).len(), 1);
        assert!(jitter.push(make(12), now).is_empty());
        let ready = jitter.push(make(11), now + Duration::from_millis(10));
        assert_eq!(ready.iter().map(|frame| frame.sequence).collect::<Vec<_>>(), vec![11, 12]);
    }

    #[test]
    fn jitter_buffer_skips_a_missing_packet_after_its_reorder_window() {
        let now = Instant::now();
        let make = |sequence| PendingAudio {
            sender: [5; 16],
            sequence,
            opus: vec![sequence as u8],
            position: None,
            max_distance: None,
        };
        let mut jitter = AudioJitterBuffer::default();

        assert_eq!(jitter.push(make(20), now).len(), 1);
        assert!(jitter.push(make(22), now).is_empty());
        let ready = jitter.poll(now + Duration::from_millis(41));
        assert_eq!(ready.iter().map(|frame| frame.sequence).collect::<Vec<_>>(), vec![21, 22]);
        assert!(ready[0].opus.is_empty());
    }

    #[test]
    fn voice_activation_sends_frames_above_the_rms_threshold() {
        let (tx, rx) = mpsc::sync_channel(2);
        let mut input = InputAssembler::new(
            SAMPLE_RATE as u32,
            tx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicU32::new(0.025f32.to_bits())),
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        for _ in 0..=FRAME_SAMPLES {
            input.push(0.1);
        }
        assert!(matches!(rx.try_recv(), Ok(Command::Pcm(_))));
    }

    #[test]
    fn voice_activation_ignores_frames_below_the_rms_threshold() {
        let (tx, rx) = mpsc::sync_channel(2);
        let mut input = InputAssembler::new(
            SAMPLE_RATE as u32,
            tx,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicU32::new(0.025f32.to_bits())),
            Arc::new(AtomicU32::new(1.0f32.to_bits())),
        );
        for _ in 0..=FRAME_SAMPLES {
            input.push(0.01);
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn rejects_varint_overflow() {
        let mut reader = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert!(reader.varint().is_none());
    }
}
