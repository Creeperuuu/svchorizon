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

/// Upstream `debug/PingHandler.PING_V1`: while `allow_pings` is on (the
/// default in every released generation), a voice server answers datagrams
/// addressed to this UUID with an unauthenticated, unencrypted 24-byte echo.
/// The probe therefore works identically against Simple Voice Chat 2.5.x,
/// 2.6.x and 2.7.x and separates "the UDP path is blocked" from "the server
/// ignores our authentication datagrams".
const PING_V1: [u8; 16] = [
    0x58, 0xbc, 0x9a, 0xe9, 0xc7, 0xa8, 0x45, 0xe4, 0xa1, 0x1c, 0xef, 0xbb, 0x67, 0x19, 0x94, 0x25,
];
/// Size of the ping payload (UUID + timestamp) the server echoes verbatim.
const PING_PONG_BYTES: usize = 24;
const PING_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// Which wire generation the server speaks. The plugin-channel handshake is
/// gated by a compatibility number: a `request_secret` carrying a different
/// number makes the server send the player an incompatibility chat notice
/// instead of a secret, and the UDP layer differs between the generations
/// that are deployed in the wild:
///
/// * `Compat20` - Simple Voice Chat 2.6.x (compatibility version 20, the
///   release line published for Minecraft 1.12.2 up to the current versions).
///   UDP envelopes carry the player's profile UUID, `Authenticate` packets
///   carry player UUID plus secret, and both ack packets are empty.
/// * `Compat21` - Simple Voice Chat 2.7.x (compatibility version 21). The
///   secret response assigns a per-connection UUID, UDP envelopes carry that
///   connection UUID instead of the player, `Authenticate` is empty, and the
///   `AuthenticateAck` carries a 16-byte HMAC nonce that the client must echo
///   in `ConnectionCheck` from the same source address.
/// * `Compat18` - Simple Voice Chat 2.5.x (compatibility version 18, the
///   previous release line still running on servers that have not updated).
///   Verified against upstream `outdated/1.21.6-2.5.30`: the secret payload,
///   the UDP envelope, the packet type ids and the Authenticate layout are
///   byte-identical to the 2.6.x generation - only the handshake number
///   differs, so this variant reuses the compat-20 encoders.
///
/// None of these is a special case for one Minecraft version: the generation
/// is a property of the server's Simple Voice Chat build and the client
/// supports all three, which covers older and newer game servers alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SvcProfile {
    Compat20,
    Compat21,
    Compat18,
}

impl SvcProfile {
    /// The compatibility number the official client sends in the
    /// `voicechat:request_secret` payload for this generation.
    pub fn compatibility_version(self) -> i32 {
        match self {
            Self::Compat20 => 20,
            Self::Compat21 => 21,
            Self::Compat18 => 18,
        }
    }

    /// The generation a given compatibility number belongs to.
    pub fn from_compatibility_version(version: i32) -> Option<Self> {
        match version {
            20 => Some(Self::Compat20),
            21 => Some(Self::Compat21),
            18 => Some(Self::Compat18),
            _ => None,
        }
    }

    /// Human-readable generation label for diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Compat20 => "compat-20 (Simple Voice Chat 2.6.x)",
            Self::Compat21 => "compat-21 (Simple Voice Chat 2.7.x)",
            Self::Compat18 => "compat-18 (Simple Voice Chat 2.5.x)",
        }
    }
}

/// Every UDP generation supported by this client, in probe order: the
/// released 2.6.x line first (every Minecraft version ships it), then the
/// 2.7.x pre-release, then the 2.5.x line on servers that never updated.
pub const SVC_PROFILES: &[SvcProfile] = &[SvcProfile::Compat20, SvcProfile::Compat21, SvcProfile::Compat18];

#[derive(Clone, Debug)]
pub struct VoiceServerInfo {
    pub secret: [u8; 16],
    pub port: u16,
    pub player: [u8; 16],
    /// Wire generation the server announced through the secret payload.
    pub profile: SvcProfile,
    /// Server-assigned connection UUID (compat-21 only; zeroed for compat-20,
    /// where the player UUID keys every datagram instead).
    pub connection_id: [u8; 16],
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
    fn default() -> Self {
        Self {
            volume: 1.0,
            muted: false,
        }
    }
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
        self.senders
            .retain(|_, state| now.duration_since(state.last_seen) < Self::IDLE_TIMEOUT);
        if !self.senders.contains_key(&packet.sender) && self.senders.len() >= Self::MAX_SENDERS {
            return Vec::new();
        }

        let state = self
            .senders
            .entry(packet.sender)
            .or_insert_with(|| SenderJitter {
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
            let Some(expected) = state.expected else {
                break;
            };
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
            Self::UnsupportedCodec(c) => {
                write!(f, "unsupported Simple Voice Chat codec ordinal {c}")
            }
            Self::WorkerStopped => write!(f, "voice worker has stopped"),
        }
    }
}
impl Error for VoiceError {}
impl From<std::io::Error> for VoiceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl VoiceError {
    /// True when the send failed only because the remote peer answered an
    /// earlier datagram with an ICMP port-unreachable, which connected UDP
    /// sockets on Windows surface as `WSAECONNRESET` on a later send or
    /// recv. On a connectionless voice socket this is reachability noise,
    /// not a socket failure - the unanswered-attempts warning tracks the
    /// reachability signal instead.
    fn is_connection_reset(&self) -> bool {
        matches!(self, Self::Io(error) if error.kind() == std::io::ErrorKind::ConnectionReset)
    }
}

enum Command {
    Pcm([i16; FRAME_SAMPLES]),
    Shutdown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AudioDeviceSelection {
    input: Option<String>,
    output: Option<String>,
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
    auth_attempts: Arc<AtomicU32>,
    ping_answered: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    audio_worker: Option<JoinHandle<()>>,
    listener: Arc<Mutex<ListenerPose>>,
    player_positions: Arc<RwLock<HashMap<[u8; 16], [f64; 3]>>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
    audio_devices: Arc<RwLock<AudioDeviceSelection>>,
}

impl VoiceClient {
    pub fn connect(info: VoiceServerInfo, fallback_host: &str) -> Result<Self, VoiceError> {
        if info.port == 0 {
            return Err(VoiceError::InvalidServerInfo("UDP port is zero"));
        }
        if info.keep_alive_ms <= 0 {
            return Err(VoiceError::InvalidServerInfo(
                "keepalive interval is not positive",
            ));
        }
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
        let endpoints = resolve_voice_addresses(&info.host, fallback_host, info.port)?;
        eprintln!(
            "[svchorizon] Voice endpoint(s): {} | server protocol generation: {}",
            endpoints
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(", "),
            info.profile.label()
        );
        match udp_self_test() {
            Ok(()) => {
                eprintln!("[svchorizon] UDP self-test: ok (the game process can receive UDP)")
            }
            Err(error) => eprintln!(
                "[svchorizon] UDP self-test FAILED: {error} - the game process cannot receive UDP at all; a sandbox/antivirus is blocking it"
            ),
        }
        let mixed_family =
            endpoints.iter().any(|a| a.is_ipv4()) && endpoints.iter().any(|a| a.is_ipv6());
        // A dual-stack bind keeps one socket usable for both address
        // families; IPv4 targets are sent as v4-mapped IPv6 addresses then.
        let dual_stack = mixed_family || endpoints.iter().all(|a| a.is_ipv6());
        let bind_address = if dual_stack { "[::]:0" } else { "0.0.0.0:0" };
        let socket = UdpSocket::bind(bind_address)?;
        // Deliberately not connect()ed: the worker rotates through the
        // resolved endpoints (and needs replies from whichever it last
        // used), and connectionless UDP avoids Windows surfacing stale
        // ICMP responses as WSAECONNRESET on later operations.
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
        let auth_attempts = Arc::new(AtomicU32::new(0));
        let ping_answered = Arc::new(AtomicBool::new(false));

        let audio_running = Arc::clone(&running);
        let audio_transmitting = Arc::clone(&transmitting);
        let audio_command_tx = command_tx.clone();
        let audio_events = event_tx.clone();
        let audio_listener = Arc::clone(&listener);
        let audio_settings = Arc::clone(&player_audio_settings);
        let audio_voice_activation = Arc::clone(&voice_activation);
        let audio_voice_threshold = Arc::clone(&voice_activation_threshold);
        let audio_microphone_gain = Arc::clone(&microphone_gain);
        let audio_devices = Arc::new(RwLock::new(AudioDeviceSelection::default()));
        let worker_audio_devices = Arc::clone(&audio_devices);
        let audio_worker = thread::Builder::new()
            .name("svchorizon-audio".into())
            .spawn(move || {
                run_audio_io(
                    audio_command_tx,
                    audio_rx,
                    audio_running,
                    audio_transmitting,
                    audio_events,
                    audio_listener,
                    audio_settings,
                    audio_voice_activation,
                    audio_voice_threshold,
                    audio_microphone_gain,
                    worker_audio_devices,
                )
            })
            .map_err(VoiceError::Io)?;

        let worker_running = Arc::clone(&running);
        let worker_transmitting = Arc::clone(&transmitting);
        let worker_whispering = Arc::clone(&whispering);
        let worker_voice_activation = Arc::clone(&voice_activation);
        let worker_connected = Arc::clone(&connected);
        let worker_auth_attempts = Arc::clone(&auth_attempts);
        let worker_ping_answered = Arc::clone(&ping_answered);
        let worker_positions = Arc::clone(&player_positions);
        let worker = match thread::Builder::new()
            .name("svchorizon-udp".into())
            .spawn(move || {
                run_worker(
                    socket,
                    info,
                    endpoints,
                    dual_stack,
                    application,
                    command_rx,
                    event_tx,
                    audio_tx,
                    worker_running,
                    worker_transmitting,
                    worker_whispering,
                    worker_voice_activation,
                    worker_connected,
                    worker_auth_attempts,
                    worker_ping_answered,
                    worker_positions,
                );
            }) {
            Ok(worker) => worker,
            Err(error) => {
                running.store(false, Ordering::Release);
                let _ = audio_worker.join();
                return Err(VoiceError::Io(error));
            }
        };

        Ok(Self {
            command_tx,
            event_rx,
            running,
            transmitting,
            whispering,
            voice_activation,
            voice_activation_threshold,
            microphone_gain,
            connected,
            auth_attempts,
            ping_answered,
            worker: Some(worker),
            audio_worker: Some(audio_worker),
            listener,
            player_positions,
            player_audio_settings,
            audio_devices,
        })
    }

    /// Enumerate available local microphone devices.
    pub fn input_devices() -> Vec<String> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host()
            .input_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// Enumerate available local speaker devices.
    pub fn output_devices() -> Vec<String> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host()
            .output_devices()
            .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default()
    }

    /// Select a local microphone by CPAL device name; None restores the OS default.
    pub fn set_input_device(&self, name: Option<String>) {
        if let Ok(mut selected) = self.audio_devices.write() {
            selected.input = name;
        }
    }

    /// Select a local speaker by CPAL device name; None restores the OS default.
    pub fn set_output_device(&self, name: Option<String>) {
        if let Ok(mut selected) = self.audio_devices.write() {
            selected.output = name;
        }
    }

    pub fn selected_audio_devices(&self) -> (Option<String>, Option<String>) {
        self.audio_devices
            .read()
            .map(|s| (s.input.clone(), s.output.clone()))
            .unwrap_or_default()
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

    /// Authentication attempts sent since the last answer from the voice
    /// server (0 once authenticated). The HUD uses this to distinguish
    /// "still connecting" from "the server is not answering at all".
    pub fn unanswered_auth_attempts(&self) -> u32 {
        self.auth_attempts.load(Ordering::Relaxed)
    }

    /// Whether the voice server answered an unauthenticated PING_V1 probe.
    /// True means the datagram path works and only authentication datagrams
    /// are being ignored; false with unanswered authentication attempts means
    /// nothing at all came back over UDP.
    pub fn ping_answered(&self) -> bool {
        self.ping_answered.load(Ordering::Acquire)
    }

    pub fn is_transmitting(&self) -> bool {
        self.transmitting.load(Ordering::Relaxed)
    }

    pub fn is_whispering(&self) -> bool {
        self.whispering.load(Ordering::Relaxed)
    }

    /// Enable voice activation and set its normalized RMS threshold (0.0..=1.0).
    pub fn set_voice_activation(&self, enabled: bool, threshold: f32) {
        let threshold = if threshold.is_finite() {
            threshold.clamp(0.0, 1.0)
        } else {
            0.025
        };
        self.voice_activation_threshold
            .store(threshold.to_bits(), Ordering::Relaxed);
        self.voice_activation.store(enabled, Ordering::Relaxed);
    }

    pub fn is_voice_activation_enabled(&self) -> bool {
        self.voice_activation.load(Ordering::Relaxed)
    }

    /// Set microphone amplification from 0.0 (silent) through 4.0.
    pub fn set_microphone_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 4.0)
        } else {
            1.0
        };
        self.microphone_gain
            .store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Set a speaker's playback level. Values are clamped to 0.0..=2.0.
    pub fn set_player_volume(&self, player: [u8; 16], volume: f32) {
        if let Ok(mut settings) = self.player_audio_settings.write() {
            if !settings.contains_key(&player) && settings.len() >= 128 {
                return;
            }
            let remove = {
                let entry = settings.entry(player).or_default();
                entry.volume = if volume.is_finite() {
                    volume.clamp(0.0, 2.0)
                } else {
                    1.0
                };
                entry.volume == 1.0 && !entry.muted
            };
            if remove {
                settings.remove(&player);
            }
        }
    }

    /// Mute or unmute one remote speaker without affecting other players.
    pub fn set_player_muted(&self, player: [u8; 16], muted: bool) {
        if let Ok(mut settings) = self.player_audio_settings.write() {
            if !settings.contains_key(&player) && settings.len() >= 128 {
                return;
            }
            let remove = {
                let entry = settings.entry(player).or_default();
                entry.muted = muted;
                entry.volume == 1.0 && !entry.muted
            };
            if remove {
                settings.remove(&player);
            }
        }
    }

    pub fn set_listener(&self, x: f64, y: f64, z: f64, yaw_degrees: f32) {
        if let Ok(mut pose) = self.listener.lock() {
            *pose = ListenerPose {
                x,
                y,
                z,
                yaw: f64::from(yaw_degrees).to_radians(),
                active: true,
            };
        }
    }

    pub fn clear_listener(&self) {
        if let Ok(mut pose) = self.listener.lock() {
            pose.active = false;
        }
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
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(worker) = self.audio_worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for VoiceClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Cap on how many resolved endpoints the UDP worker rotates through while
/// the voice server stays silent.
const MAX_VOICE_ENDPOINTS: usize = 8;

/// Resolve every candidate address for the voice endpoint. DNS names can
/// legitimately resolve to several records (dual-stack A+AAAA, or multiple
/// front ends); the official client only ever uses the first one, but when
/// the first is unreachable there is no way to learn that from the official
/// behavior, so the worker rotates through the full list while unanswered.
fn resolve_voice_addresses(
    voice_host: &str,
    game_server_host: &str,
    default_port: u16,
) -> Result<Vec<SocketAddr>, VoiceError> {
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
        return Ok(vec![SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port,
        )]);
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let mut candidates = Vec::new();
    for address in (host.as_str(), port)
        .to_socket_addrs()
        .map_err(VoiceError::Io)?
    {
        if candidates.len() >= MAX_VOICE_ENDPOINTS {
            break;
        }
        if !candidates.contains(&address) {
            candidates.push(address);
        }
    }
    if candidates.is_empty() {
        return Err(VoiceError::Resolve(format!("{host}:{port}")));
    }
    Ok(candidates)
}

/// Split a Minecraft-style host authority without accidentally treating the
/// port of the game server as the voice UDP port.
fn split_host_port(value: &str) -> Result<(&str, Option<u16>), VoiceError> {
    if let Some(rest) = value.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or_else(|| VoiceError::Resolve(format!("invalid IPv6 authority: {value}")))?;
        let host = &rest[..close];
        let suffix = &rest[close + 1..];
        if suffix.is_empty() {
            return Ok((host, None));
        }
        let port_text = suffix
            .strip_prefix(':')
            .ok_or_else(|| VoiceError::Resolve(format!("invalid host authority: {value}")))?;
        let port = port_text
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| VoiceError::Resolve(format!("invalid port in authority: {value}")))?;
        return Ok((host, Some(port)));
    }

    if value.matches(':').count() == 1 {
        let (host, port_text) = value.split_once(':').expect("one colon was counted");
        let port = port_text
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
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
        return Err(VoiceError::Resolve(
            "voice server host contains whitespace".into(),
        ));
    }
    Ok(value.to_owned())
}

/// Unanswered authentication attempts after which the worker moves on to the
/// next resolved endpoint. 5s per endpoint keeps the first candidate - the
/// same address the official client would use - active for long enough,
/// while still cycling through all candidates within reasonable time.
const ENDPOINT_RETRY_THRESHOLD: u32 = 5;

/// IPv4 targets on a dual-stack (IPv6-bound) socket must be sent as
/// v4-mapped IPv6 addresses.
fn mapped_target(dual_stack: bool, target: SocketAddr) -> SocketAddr {
    if dual_stack && target.is_ipv4() {
        let mapped = match target.ip() {
            IpAddr::V4(v4) => {
                // ::ffff:0:0/96 followed by the four IPv4 octets.
                let [a, b, c, d] = v4.octets();
                IpAddr::V6(std::net::Ipv6Addr::new(
                    0,
                    0,
                    0,
                    0,
                    0,
                    0xffff,
                    u16::from_be_bytes([a, b]),
                    u16::from_be_bytes([c, d]),
                ))
            }
            IpAddr::V6(v6) => IpAddr::V6(v6),
        };
        SocketAddr::new(mapped, target.port())
    } else {
        target
    }
}

fn run_worker(
    socket: UdpSocket,
    info: VoiceServerInfo,
    endpoints: Vec<SocketAddr>,
    dual_stack: bool,
    application: Application,
    commands: Receiver<Command>,
    events: SyncSender<VoiceEvent>,
    audio: SyncSender<AudioFrame>,
    running: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    whispering: Arc<AtomicBool>,
    voice_activation: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    auth_attempts: Arc<AtomicU32>,
    ping_answered: Arc<AtomicBool>,
    player_positions: Arc<RwLock<HashMap<[u8; 16], [f64; 3]>>>,
) {
    let mut encoder = match OpusEncoder::new(SAMPLE_RATE, 1, application) {
        Ok(value) => value,
        Err(_) => {
            let _ = events.try_send(VoiceEvent::Disconnected(
                "Opus encoder initialization failed".into(),
            ));
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
    // The official client talks to exactly one resolved address. This client
    // starts there too, but keeps rotating through every resolved endpoint
    // while authentication goes unanswered, so dual-stack or multi-record
    // hosts still work when the first address is a dead end.
    let mut endpoint_index = 0usize;
    let mut attempts_on_endpoint: u32 = 0;
    let mut auth_nonce: [u8; 16] = [0u8; 16];
    let mut endpoint = endpoints[endpoint_index];
    // Datagram envelope candidates, most likely first (see envelope_keys).
    // While authentication goes unanswered the worker cycles through them so
    // custom server builds that key the secret lookup differently still work;
    // once an ack arrives the working key is frozen for all traffic.
    let envelopes = envelope_keys(&info);
    let mut envelope_index = 0usize;
    let mut ping_counter = 0u64;
    let mut last_ping: Option<Instant> = None;
    let mut last_ping_id = [0u8; 16];

    while running.load(Ordering::Acquire) {
        let envelope = envelopes[envelope_index];
        loop {
            match commands.try_recv() {
                Ok(Command::Pcm(pcm)) => {
                    if validated
                        && (transmitting.load(Ordering::Relaxed)
                            || voice_activation.load(Ordering::Relaxed))
                    {
                        let mut opus = [0u8; 1276];
                        if let Ok(length) =
                            encoder.encode_i16(&pcm, FRAME_SAMPLES, &mut opus[..payload_limit])
                        {
                            let packet = Packet::Mic {
                                opus: opus[..length].to_vec(),
                                sequence,
                                whispering: whispering.load(Ordering::Relaxed),
                            };
                            sequence = sequence.wrapping_add(1);
                            let _ = send_packet(
                                &socket,
                                &info,
                                endpoint,
                                dual_stack,
                                &packet,
                                envelope,
                            );
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
        if !running.load(Ordering::Acquire) {
            break;
        }

        // Unauthenticated reachability probe (upstream PingHandler, on by
        // default): a pong proves the datagram path to the voice server works
        // and that only our authentication datagrams are being ignored. It
        // runs beside the authentication loop until the first ack arrives.
        if !authenticated && last_ping.is_none_or(|at| at.elapsed() >= PING_PROBE_INTERVAL) {
            ping_counter = ping_counter.wrapping_add(1);
            last_ping_id[..8].copy_from_slice(&ping_counter.to_be_bytes());
            let probe = encode_ping_probe(last_ping_id, now_millis() as i64);
            if let Err(error) = socket.send_to(&probe, mapped_target(dual_stack, endpoint)) {
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::WouldBlock
                ) {
                    eprintln!("[svchorizon] sending ping probe failed: {error}");
                }
            }
            last_ping = Some(Instant::now());
        }

        if !authenticated && last_auth.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)) {
            // Compat-20 servers identify the player inside the Authenticate
            // payload; compat-21 servers identify the datagram by the
            // connection UUID in the envelope and the payload is empty.
            let authenticate = match info.profile {
                SvcProfile::Compat20 | SvcProfile::Compat18 => Packet::Authenticate {
                    player: info.player,
                    secret: info.secret,
                },
                SvcProfile::Compat21 => Packet::Authenticate {
                    player: [0u8; 16],
                    secret: [0u8; 16],
                },
            };
            if let Err(error) =
                send_packet(&socket, &info, endpoint, dual_stack, &authenticate, envelope)
            {
                // A connectionless socket should not surface stale ICMP
                // failures; if one still appears it is a reachability
                // signal, not a socket failure - the unanswered-attempts
                // warning below tracks that instead.
                if !error.is_connection_reset() {
                    eprintln!("[svchorizon] sending authentication failed: {error}");
                }
            }
            let attempts = auth_attempts.fetch_add(1, Ordering::Relaxed) + 1;
            attempts_on_endpoint += 1;
            if attempts % 5 == 0 {
                eprintln!(
                    "[svchorizon] Voice server has not answered {attempts} authentication attempts yet (endpoint {endpoint})"
                );
            }
            if attempts_on_endpoint >= ENDPOINT_RETRY_THRESHOLD {
                attempts_on_endpoint = 0;
                if endpoints.len() > 1 {
                    endpoint_index = (endpoint_index + 1) % endpoints.len();
                    endpoint = endpoints[endpoint_index];
                    eprintln!(
                        "[svchorizon] No answer yet - switching voice endpoint candidate to {endpoint}"
                    );
                }
                if envelopes.len() > 1 {
                    envelope_index = (envelope_index + 1) % envelopes.len();
                    eprintln!(
                        "[svchorizon] Still no authentication answer - also addressing datagrams to the {} (custom server builds key the secret lookup differently)",
                        describe_envelope(&info, envelopes[envelope_index])
                    );
                }
            }
            if attempts == 15 {
                if ping_answered.load(Ordering::Acquire) {
                    eprintln!(
                        "[svchorizon] The voice server at {endpoint} answers unauthenticated pings but ignored {attempts} authentication attempts. The client speaks Simple Voice Chat protocol generations {} and tried every known datagram envelope - the server likely runs a custom voice build or rejects this client's secret.",
                        SVC_PROFILES
                            .iter()
                            .map(|p| p.compatibility_version().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                } else {
                    eprintln!(
                        "[svchorizon] No answer at all from the voice server at {endpoint} - not even to unauthenticated pings. The UDP path to port {} is blocked somewhere in between: check that horizon-client.exe is allowed in Windows Firewall and any antivirus, and that the server's voice UDP port {} is open/forwarded (a proxy in front of the game server must forward UDP as well).",
                        info.port, info.port
                    );
                }
            }
            last_auth = Some(Instant::now());
        } else if authenticated
            && !validated
            && last_check.is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            // Compat-21 validation echoes the nonce the server handed out in
            // the AuthenticateAck; compat-20 servers expect an empty packet.
            let check = match info.profile {
                SvcProfile::Compat20 | SvcProfile::Compat18 => {
                    Packet::ConnectionCheck { nonce: None }
                }
                SvcProfile::Compat21 => Packet::ConnectionCheck {
                    nonce: Some(auth_nonce),
                },
            };
            if let Err(error) = send_packet(&socket, &info, endpoint, dual_stack, &check, envelope) {
                if !error.is_connection_reset() {
                    eprintln!("[svchorizon] sending connection check failed: {error}");
                }
            }
            last_check = Some(Instant::now());
        }

        if validated && last_keepalive.elapsed() > keepalive_timeout {
            connected.store(false, Ordering::Release);
            let _ = events.try_send(VoiceEvent::Disconnected(
                "voice server keepalive timed out".into(),
            ));
            break;
        }

        match socket.recv(&mut receive) {
            Ok(length) => {
                if !ping_answered.load(Ordering::Acquire)
                    && last_ping_id != [0u8; 16]
                    && decode_ping_pong(&receive[..length]) == Some(last_ping_id)
                {
                    ping_answered.store(true, Ordering::Release);
                    eprintln!(
                        "[svchorizon] Voice server answered an unauthenticated ping - the UDP path to {endpoint} works; authentication datagrams are being ignored (protocol or secret mismatch)"
                    );
                    continue;
                }
                let Some(packet) = decode_server_datagram(&receive[..length], &info.secret) else {
                    continue;
                };
                match packet {
                    Packet::AuthenticateAck { nonce } => {
                        authenticated = true;
                        auth_attempts.store(0, Ordering::Relaxed);
                        attempts_on_endpoint = 0;
                        last_check = None;
                        if let Some(nonce) = nonce {
                            auth_nonce = nonce;
                        }
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
                        if let Err(error) = send_packet(
                            &socket,
                            &info,
                            endpoint,
                            dual_stack,
                            &Packet::KeepAlive,
                            envelope,
                        ) {
                            if !error.is_connection_reset() {
                                eprintln!("[svchorizon] sending keepalive failed: {error}");
                            }
                        }
                    }
                    Packet::Ping { id, timestamp } => {
                        let _ = send_packet(
                            &socket,
                            &info,
                            endpoint,
                            dual_stack,
                            &Packet::Ping { id, timestamp },
                            envelope,
                        );
                    }
                    Packet::PlayerSound {
                        sender,
                        opus,
                        sequence,
                        distance,
                        ..
                    } => {
                        if validated {
                            let position = player_positions
                                .read()
                                .ok()
                                .and_then(|positions| positions.get(&sender).copied());
                            let ready = jitter.push(
                                PendingAudio {
                                    sender,
                                    sequence,
                                    opus,
                                    position,
                                    max_distance: Some(distance),
                                },
                                Instant::now(),
                            );
                            for frame in ready {
                                emit_audio(&mut decoders, &audio, frame);
                            }
                        }
                    }
                    Packet::GroupSound {
                        sender,
                        opus,
                        sequence,
                        ..
                    } => {
                        if validated {
                            let ready = jitter.push(
                                PendingAudio {
                                    sender,
                                    sequence,
                                    opus,
                                    position: None,
                                    max_distance: None,
                                },
                                Instant::now(),
                            );
                            for frame in ready {
                                emit_audio(&mut decoders, &audio, frame);
                            }
                        }
                    }
                    Packet::LocationSound {
                        sender,
                        location,
                        opus,
                        sequence,
                        distance,
                        ..
                    } => {
                        if validated {
                            let ready = jitter.push(
                                PendingAudio {
                                    sender,
                                    sequence,
                                    opus,
                                    position: Some(location),
                                    max_distance: Some(distance),
                                },
                                Instant::now(),
                            );
                            for frame in ready {
                                emit_audio(&mut decoders, &audio, frame);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                // Windows surfaces a stray ICMP port-unreachable as
                // WSAECONNRESET on a connected UDP socket; keep running and
                // let the unanswered-authentication counter report it.
                | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(error) => {
                connected.store(false, Ordering::Release);
                let _ = events.try_send(VoiceEvent::Disconnected(format!(
                    "UDP socket failed: {error}"
                )));
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
    if pending.opus.len() > MAX_OPUS {
        return;
    }
    if !decoders.contains_key(&pending.sender) {
        let Ok(decoder) = OpusDecoder::new(SAMPLE_RATE, 1) else {
            return;
        };
        decoders.insert(pending.sender, decoder);
    }
    let Some(decoder) = decoders.get_mut(&pending.sender) else {
        return;
    };
    let mut float_pcm = [0.0f32; FRAME_SAMPLES];
    let Ok(samples) = decoder.decode(&pending.opus, FRAME_SAMPLES, &mut float_pcm) else {
        return;
    };
    let mut pcm = [0i16; FRAME_SAMPLES];
    for (out, input) in pcm
        .iter_mut()
        .zip(float_pcm.iter())
        .take(samples.min(FRAME_SAMPLES))
    {
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
    selection: Arc<RwLock<AudioDeviceSelection>>,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let shared_rx = Arc::new(Mutex::new(audio_rx));
    let mut streams: Vec<cpal::Stream> = Vec::new();
    let mut active_selection: Option<AudioDeviceSelection> = None;
    while running.load(Ordering::Acquire) {
        let selected = selection.read().map(|s| s.clone()).unwrap_or_default();
        if active_selection.as_ref() != Some(&selected) {
            streams.clear();
            let input = match selected.input.as_deref() {
                Some(name) => host
                    .input_devices()
                    .ok()
                    .and_then(|ds| ds.filter(|d| d.name().ok().as_deref() == Some(name)).next())
                    .or_else(|| host.default_input_device()),
                None => host.default_input_device(),
            };
            match input {
                Some(device) => {
                    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
                    match build_input_stream(
                        &device,
                        command_tx.clone(),
                        Arc::clone(&transmitting),
                        Arc::clone(&voice_activation),
                        Arc::clone(&voice_activation_threshold),
                        Arc::clone(&microphone_gain),
                        events.clone(),
                    ) {
                        Ok(stream) => match stream.play() {
                            Ok(()) => {
                                eprintln!("[svchorizon] Microphone opened: {name}");
                                streams.push(stream);
                            }
                            Err(err) => {
                                let _ = events.try_send(VoiceEvent::AudioUnavailable(format!(
                                    "could not start microphone: {err}"
                                )));
                            }
                        },
                        Err(err) => {
                            let _ = events.try_send(VoiceEvent::AudioUnavailable(format!(
                                "microphone unavailable: {err}"
                            )));
                        }
                    }
                }
                None => {
                    let _ = events.try_send(VoiceEvent::AudioUnavailable(
                        "no selected/default microphone device".into(),
                    ));
                }
            }
            let output = match selected.output.as_deref() {
                Some(name) => host
                    .output_devices()
                    .ok()
                    .and_then(|ds| ds.filter(|d| d.name().ok().as_deref() == Some(name)).next())
                    .or_else(|| host.default_output_device()),
                None => host.default_output_device(),
            };
            match output {
                Some(device) => {
                    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
                    match build_output_stream(
                        &device,
                        Arc::clone(&shared_rx),
                        events.clone(),
                        Arc::clone(&listener),
                        Arc::clone(&player_audio_settings),
                    ) {
                        Ok(stream) => match stream.play() {
                            Ok(()) => {
                                eprintln!("[svchorizon] Speaker opened: {name}");
                                streams.push(stream);
                            }
                            Err(err) => {
                                let _ = events.try_send(VoiceEvent::AudioUnavailable(format!(
                                    "could not start speaker: {err}"
                                )));
                            }
                        },
                        Err(err) => {
                            let _ = events.try_send(VoiceEvent::AudioUnavailable(format!(
                                "speaker unavailable: {err}"
                            )));
                        }
                    }
                }
                None => {
                    let _ = events.try_send(VoiceEvent::AudioUnavailable(
                        "no selected/default speaker device".into(),
                    ));
                }
            }
            active_selection = Some(selected);
        }
        thread::sleep(Duration::from_millis(100));
    }
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
        if !self.has_previous {
            self.previous = sample;
            self.has_previous = true;
            return;
        }
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
                    let energy = self
                        .frame
                        .iter()
                        .map(|sample| {
                            let normalized = *sample as f32 / 32768.0;
                            normalized * normalized
                        })
                        .sum::<f32>()
                        / FRAME_SAMPLES as f32;
                    let rms = energy.sqrt();
                    let threshold =
                        f32::from_bits(self.voice_activation_threshold.load(Ordering::Relaxed));
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
                rate,
                tx,
                transmitting,
                Arc::clone(&voice_activation),
                Arc::clone(&voice_activation_threshold),
                Arc::clone(&microphone_gain),
            );
            let errors = events;
            device
                .build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        for frame in data.chunks(channels) {
                            input.push(
                                frame.iter().copied().sum::<f32>() / frame.len().max(1) as f32,
                            );
                        }
                    },
                    move |error| {
                        let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!(
                            "microphone stream error: {error}"
                        )));
                    },
                    None,
                )
                .map_err(|e| e.to_string())
        }
        cpal::SampleFormat::I16 => {
            let mut input = InputAssembler::new(
                rate,
                tx,
                transmitting,
                Arc::clone(&voice_activation),
                Arc::clone(&voice_activation_threshold),
                Arc::clone(&microphone_gain),
            );
            let errors = events;
            device
                .build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        for frame in data.chunks(channels) {
                            let mono = frame.iter().map(|s| *s as f32 / 32768.0).sum::<f32>()
                                / frame.len().max(1) as f32;
                            input.push(mono);
                        }
                    },
                    move |error| {
                        let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!(
                            "microphone stream error: {error}"
                        )));
                    },
                    None,
                )
                .map_err(|e| e.to_string())
        }
        cpal::SampleFormat::U16 => {
            let mut input = InputAssembler::new(
                rate,
                tx,
                transmitting,
                Arc::clone(&voice_activation),
                Arc::clone(&voice_activation_threshold),
                Arc::clone(&microphone_gain),
            );
            let errors = events;
            device
                .build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        for frame in data.chunks(channels) {
                            let mono = frame
                                .iter()
                                .map(|s| (*s as f32 - 32768.0) / 32768.0)
                                .sum::<f32>()
                                / frame.len().max(1) as f32;
                            input.push(mono);
                        }
                    },
                    move |error| {
                        let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!(
                            "microphone stream error: {error}"
                        )));
                    },
                    None,
                )
                .map_err(|e| e.to_string())
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
    rx: Arc<Mutex<Receiver<AudioFrame>>>,
    speakers: HashMap<[u8; 16], VecDeque<PlaybackFrame>>,
    step: f64,
    listener: Arc<Mutex<ListenerPose>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
}

impl OutputAssembler {
    const MAX_SPEAKERS: usize = 32;
    const MAX_QUEUED_FRAMES_PER_SPEAKER: usize = 6;

    fn new(
        rx: Arc<Mutex<Receiver<AudioFrame>>>,
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
        let Some(source) = frame.position else {
            return (1.0, 1.0);
        };
        let Ok(listener) = self.listener.lock() else {
            return (1.0, 1.0);
        };
        if !listener.active {
            return (1.0, 1.0);
        }
        let dx = source[0] - listener.x;
        let dy = source[1] - listener.y;
        let dz = source[2] - listener.z;
        let distance = (dx * dx + dy * dy + dz * dz).sqrt();
        let max_distance = f64::from(frame.max_distance.unwrap_or(0.0));
        let attenuation = if max_distance > 0.0 {
            ((max_distance - distance) / max_distance).clamp(0.0, 1.0) as f32
        } else {
            1.0
        };
        let pan: f32 = if distance > 0.001 {
            ((dx * listener.yaw.cos() - dz * listener.yaw.sin()) / distance).clamp(-1.0, 1.0) as f32
        } else {
            0.0
        };
        (
            attenuation * (1.0f32 - pan).clamp(0.0, 1.0),
            attenuation * (1.0f32 + pan).clamp(0.0, 1.0),
        )
    }

    /// Mix one current frame per speaker. Consecutive packets from one
    /// speaker remain sequential, while different speakers overlap.
    fn next_stereo(&mut self) -> [i16; 2] {
        loop {
            let frame = match self.rx.lock() {
                Ok(rx) => rx.try_recv().ok(),
                Err(_) => None,
            };
            let Some(frame) = frame else {
                break;
            };
            if !self.speakers.contains_key(&frame.sender)
                && self.speakers.len() >= Self::MAX_SPEAKERS
            {
                continue;
            }
            let (mut left_gain, mut right_gain) = self.gains_for(&frame);
            let settings = self
                .player_audio_settings
                .read()
                .ok()
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
            let completed = if let Some(voice) = queue.front_mut() {
                if voice.position < FRAME_SAMPLES as f64 {
                    let index = voice.position.floor() as usize;
                    let next = (index + 1).min(FRAME_SAMPLES - 1);
                    let fraction = (voice.position - index as f64) as f32;
                    let pcm = &voice.frame.pcm;
                    let sample =
                        pcm[index] as f32 + (pcm[next] as f32 - pcm[index] as f32) * fraction;
                    left += sample * voice.left_gain;
                    right += sample * voice.right_gain;
                    voice.position += step;
                }
                voice.position >= FRAME_SAMPLES as f64
            } else {
                false
            };
            if completed {
                queue.pop_front();
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
    rx: Arc<Mutex<Receiver<AudioFrame>>>,
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
            let mut output = OutputAssembler::new(
                rx,
                rate,
                Arc::clone(&listener),
                Arc::clone(&player_audio_settings),
            );
            let errors = events;
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| {
                        for frame in data.chunks_mut(channels) {
                            let stereo = output.next_stereo();
                            if frame.len() == 1 {
                                frame[0] =
                                    (i32::from(stereo[0]) + i32::from(stereo[1])) as f32 / 65536.0;
                            } else {
                                frame[0] = stereo[0] as f32 / 32768.0;
                                frame[1] = stereo[1] as f32 / 32768.0;
                                for sample in frame.iter_mut().skip(2) {
                                    *sample = 0.0;
                                }
                            }
                        }
                    },
                    move |error| {
                        let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!(
                            "speaker stream error: {error}"
                        )));
                    },
                    None,
                )
                .map_err(|e| e.to_string())
        }
        cpal::SampleFormat::I16 => {
            let mut output = OutputAssembler::new(
                rx,
                rate,
                Arc::clone(&listener),
                Arc::clone(&player_audio_settings),
            );
            let errors = events;
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| {
                        for frame in data.chunks_mut(channels) {
                            let stereo = output.next_stereo();
                            if frame.len() == 1 {
                                frame[0] =
                                    ((i32::from(stereo[0]) + i32::from(stereo[1])) / 2) as i16;
                            } else {
                                frame[0] = stereo[0];
                                frame[1] = stereo[1];
                                for sample in frame.iter_mut().skip(2) {
                                    *sample = 0;
                                }
                            }
                        }
                    },
                    move |error| {
                        let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!(
                            "speaker stream error: {error}"
                        )));
                    },
                    None,
                )
                .map_err(|e| e.to_string())
        }
        cpal::SampleFormat::U16 => {
            let mut output = OutputAssembler::new(rx, rate, listener, player_audio_settings);
            let errors = events;
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [u16], _| {
                        for frame in data.chunks_mut(channels) {
                            let stereo = output.next_stereo();
                            if frame.len() == 1 {
                                frame[0] = ((i32::from(stereo[0]) + i32::from(stereo[1])) / 2
                                    + 32768) as u16;
                            } else {
                                frame[0] = (i32::from(stereo[0]) + 32768) as u16;
                                frame[1] = (i32::from(stereo[1]) + 32768) as u16;
                                for sample in frame.iter_mut().skip(2) {
                                    *sample = 32768;
                                }
                            }
                        }
                    },
                    move |error| {
                        let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!(
                            "speaker stream error: {error}"
                        )));
                    },
                    None,
                )
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unsupported speaker sample format {other:?}")),
    }
}

#[derive(Debug)]
enum Packet {
    Mic {
        opus: Vec<u8>,
        sequence: i64,
        whispering: bool,
    },
    /// Compat-20 (SVC 2.6.x) authentication: player UUID followed by the raw
    /// 16-byte secret issued in the SecretPacket (AuthenticatePacket.toBytes
    /// order). Compat-21 (SVC 2.7.x) encodes this packet empty; the fields
    /// are only filled for the compat-20 generation.
    Authenticate {
        player: [u8; 16],
        secret: [u8; 16],
    },
    KeepAlive,
    /// Compat-20: empty. Compat-21: carries the 16-byte nonce that the
    /// client must echo back in ConnectionCheck.
    AuthenticateAck {
        nonce: Option<[u8; 16]>,
    },
    /// Compat-20: empty. Compat-21: echoes the AuthenticateAck nonce; the
    /// server binds it to the client's source address.
    ConnectionCheck {
        nonce: Option<[u8; 16]>,
    },
    ConnectionCheckAck,
    Ping {
        id: [u8; 16],
        timestamp: i64,
    },
    PlayerSound {
        channel: [u8; 16],
        sender: [u8; 16],
        opus: Vec<u8>,
        sequence: i64,
        distance: f32,
        whispering: bool,
        category: Option<String>,
    },
    GroupSound {
        channel: [u8; 16],
        sender: [u8; 16],
        opus: Vec<u8>,
        sequence: i64,
        category: Option<String>,
    },
    LocationSound {
        channel: [u8; 16],
        sender: [u8; 16],
        location: [f64; 3],
        opus: Vec<u8>,
        sequence: i64,
        distance: f32,
        category: Option<String>,
    },
}

fn udp_self_test() -> Result<(), std::io::Error> {
    let receiver = UdpSocket::bind("127.0.0.1:0")?;
    let sender = UdpSocket::bind("127.0.0.1:0")?;
    let address = receiver.local_addr()?;
    receiver.set_read_timeout(Some(Duration::from_millis(500)))?;
    sender.send_to(&[0x53, 0x56, 0x43], address)?;
    let mut buffer = [0u8; 16];
    let received = receiver.recv(&mut buffer)?;
    if received == 3 && buffer[..3] == [0x53, 0x56, 0x43] {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unexpected self-test datagram",
        ))
    }
}

fn send_packet(
    socket: &UdpSocket,
    info: &VoiceServerInfo,
    endpoint: SocketAddr,
    dual_stack: bool,
    packet: &Packet,
    envelope: [u8; 16],
) -> Result<(), VoiceError> {
    let plain = encode_packet(packet, info.profile)
        .ok_or(VoiceError::InvalidServerInfo("invalid outbound packet"))?;
    let mut nonce = [0u8; NONCE_BYTES];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| VoiceError::InvalidServerInfo("secure random generator failed"))?;
    let key = LessSafeKey::new(
        UnboundKey::new(&aead::AES_128_GCM, &info.secret)
            .map_err(|_| VoiceError::InvalidServerInfo("invalid AES-128 key"))?,
    );
    let mut encrypted = plain;
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::empty(),
        &mut encrypted,
    )
    .map_err(|_| VoiceError::InvalidServerInfo("AES-GCM encryption failed"))?;
    let mut datagram = Vec::with_capacity(1 + 16 + 3 + NONCE_BYTES + encrypted.len());
    datagram.push(MAGIC);
    // The envelope UUID keys the server-side secret lookup and changes with
    // the wire generation: compat-20 servers map it from the player's profile
    // UUID, compat-21 servers from the per-connection UUID issued in the
    // secret response (upstream ClientNetworkMessage.writeClient). The worker
    // picks the candidate - and falls back to the alternates while
    // authentication goes unanswered.
    datagram.extend_from_slice(&envelope);
    let mut body = Vec::with_capacity(NONCE_BYTES + encrypted.len());
    body.extend_from_slice(&nonce);
    body.extend_from_slice(&encrypted);
    write_byte_array(&mut datagram, &body);
    socket.send_to(&datagram, mapped_target(dual_stack, endpoint))?;
    Ok(())
}

fn decode_server_datagram(datagram: &[u8], secret: &[u8; 16]) -> Option<Packet> {
    let mut reader = Reader::new(datagram);
    if reader.u8()? != MAGIC {
        return None;
    }
    let encrypted = reader.byte_array(MAX_DATAGRAM)?;
    if !reader.done() || encrypted.len() < NONCE_BYTES + TAG_BYTES {
        return None;
    }
    let nonce: [u8; NONCE_BYTES] = encrypted[..NONCE_BYTES].try_into().ok()?;
    let mut plaintext = encrypted[NONCE_BYTES..].to_vec();
    let key = LessSafeKey::new(UnboundKey::new(&aead::AES_128_GCM, secret).ok()?);
    let opened = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::empty(),
            &mut plaintext,
        )
        .ok()?;
    decode_packet(opened)
}

/// Datagram envelope UUIDs the client may address a generation with, most
/// likely first. Upstream keys the server-side secret lookup by the player's
/// profile UUID (2.5.x/2.6.x, `Server.getSecret(playerID)`) or the
/// per-connection UUID issued in the secret response (2.7.x,
/// `ConnectionSecrets.getByConnectionId`). The 16-byte secret is itself a
/// UUID, and custom server builds are known to key the lookup by it instead;
/// keeping it as a fallback costs nothing upstream and recovers on forks.
fn envelope_keys(info: &VoiceServerInfo) -> Vec<[u8; 16]> {
    let mut keys = Vec::with_capacity(3);
    let primary = match info.profile {
        SvcProfile::Compat20 | SvcProfile::Compat18 => info.player,
        SvcProfile::Compat21 => info.connection_id,
    };
    for key in [primary, info.player, info.secret, info.connection_id] {
        if key != [0u8; 16] && !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// Human-readable envelope description for the diagnostics log.
fn describe_envelope(info: &VoiceServerInfo, key: [u8; 16]) -> &'static str {
    if key == info.player {
        "player UUID"
    } else if key == info.connection_id {
        "connection UUID"
    } else if key == info.secret {
        "secret UUID"
    } else {
        "fallback UUID"
    }
}

/// Builds the unauthenticated reachability probe frame (upstream
/// PingHandler): envelope magic, the PING_V1 UUID and a 24-byte payload
/// (UUID + timestamp) the server echoes verbatim. Not encrypted.
fn encode_ping_probe(ping_id: [u8; 16], timestamp: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 16 + 5 + PING_PONG_BYTES);
    out.push(MAGIC);
    out.extend_from_slice(&PING_V1);
    let mut payload = Vec::with_capacity(PING_PONG_BYTES);
    payload.extend_from_slice(&ping_id);
    payload.extend_from_slice(&timestamp.to_be_bytes());
    write_byte_array(&mut out, &payload);
    out
}

/// Recognizes the 24-byte pong the voice server answers a PING_V1 probe
/// with (raw payload, no envelope, no encryption) and returns the id.
fn decode_ping_pong(datagram: &[u8]) -> Option<[u8; 16]> {
    if datagram.len() != PING_PONG_BYTES {
        return None;
    }
    Some(datagram[..16].try_into().ok()?)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn encode_packet(packet: &Packet, profile: SvcProfile) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match packet {
        Packet::Mic {
            opus,
            sequence,
            whispering,
        } => {
            if opus.len() > MAX_OPUS {
                return None;
            }
            out.push(MIC);
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&sequence.to_be_bytes());
            out.push(u8::from(*whispering));
        }
        Packet::Authenticate { player, secret } => {
            out.push(AUTHENTICATE);
            if matches!(profile, SvcProfile::Compat20 | SvcProfile::Compat18) {
                // Upstream 2.5.x/2.6.x AuthenticatePacket.toBytes: UUID, then
                // the raw secret. The 2.7.x AuthenticatePacket is empty; the
                // server identifies the connection from the envelope UUID.
                out.extend_from_slice(player);
                out.extend_from_slice(secret);
            }
        }
        Packet::ConnectionCheck { nonce } => {
            out.push(CONNECTION_CHECK);
            if let Some(nonce) = nonce {
                out.extend_from_slice(nonce);
            }
        }
        Packet::KeepAlive => out.push(KEEP_ALIVE),
        Packet::Ping { id, timestamp } => {
            out.push(PING);
            out.extend_from_slice(id);
            out.extend_from_slice(&timestamp.to_be_bytes());
        }
        _ => return None,
    }
    Some(out)
}

fn decode_packet(bytes: &[u8]) -> Option<Packet> {
    let mut r = Reader::new(bytes);
    let packet = match r.u8()? {
        MIC => Packet::Mic {
            opus: r.byte_array(MAX_OPUS)?,
            sequence: r.i64()?,
            whispering: r.boolean()?,
        },
        AUTHENTICATE => Packet::Authenticate {
            player: r.array16()?,
            secret: r.array16()?,
        },
        CONNECTION_CHECK => Packet::ConnectionCheck {
            nonce: r.opt_array16()?,
        },
        KEEP_ALIVE => Packet::KeepAlive,
        AUTHENTICATE_ACK => Packet::AuthenticateAck {
            nonce: r.opt_array16()?,
        },
        CONNECTION_CHECK_ACK => Packet::ConnectionCheckAck,
        PING => Packet::Ping {
            id: r.array16()?,
            timestamp: r.i64()?,
        },
        PLAYER_SOUND => {
            let channel = r.array16()?;
            let sender = r.array16()?;
            let opus = r.byte_array(MAX_OPUS)?;
            let sequence = r.i64()?;
            let distance = r.f32()?;
            let flags = r.u8()?;
            let category = if flags & 2 != 0 {
                Some(r.string(16)?)
            } else {
                None
            };
            Packet::PlayerSound {
                channel,
                sender,
                opus,
                sequence,
                distance,
                whispering: flags & 1 != 0,
                category,
            }
        }
        GROUP_SOUND => {
            let channel = r.array16()?;
            let sender = r.array16()?;
            let opus = r.byte_array(MAX_OPUS)?;
            let sequence = r.i64()?;
            let flags = r.u8()?;
            let category = if flags & 2 != 0 {
                Some(r.string(16)?)
            } else {
                None
            };
            Packet::GroupSound {
                channel,
                sender,
                opus,
                sequence,
                category,
            }
        }
        LOCATION_SOUND => {
            let channel = r.array16()?;
            let sender = r.array16()?;
            let location = [r.f64()?, r.f64()?, r.f64()?];
            let opus = r.byte_array(MAX_OPUS)?;
            let sequence = r.i64()?;
            let distance = r.f32()?;
            let flags = r.u8()?;
            let category = if flags & 2 != 0 {
                Some(r.string(16)?)
            } else {
                None
            };
            Packet::LocationSound {
                channel,
                sender,
                location,
                opus,
                sequence,
                distance,
                category,
            }
        }
        _ => return None,
    };
    if r.done() { Some(packet) } else { None }
}

fn write_varint(out: &mut Vec<u8>, mut value: usize) {
    while value > 0x7f {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
fn write_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(out, bytes.len());
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let result = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(result)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }
    fn boolean(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn array16(&mut self) -> Option<[u8; 16]> {
        self.take(16)?.try_into().ok()
    }
    /// Reads a trailing 16-byte field that older generations omit entirely:
    /// None when the packet already ended, Some(value) when 16 bytes remain.
    fn opt_array16(&mut self) -> Option<Option<[u8; 16]>> {
        if self.done() {
            Some(None)
        } else if self.bytes.len() - self.pos >= 16 {
            Some(Some(self.array16()?))
        } else {
            None
        }
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(u32::from_be_bytes(
            self.take(4)?.try_into().ok()?,
        )))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(u64::from_be_bytes(
            self.take(8)?.try_into().ok()?,
        )))
    }
    fn varint(&mut self) -> Option<usize> {
        let mut value = 0usize;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 4 && b & 0xf0 != 0 {
                return None;
            }
            value |= usize::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
    fn byte_array(&mut self, max: usize) -> Option<Vec<u8>> {
        let len = self.varint()?;
        if len > max {
            return None;
        }
        Some(self.take(len)?.to_vec())
    }
    fn string(&mut self, max_chars: usize) -> Option<String> {
        let bytes = self.byte_array(max_chars.checked_mul(4)?)?;
        let value = String::from_utf8(bytes).ok()?;
        if value.chars().count() > max_chars {
            return None;
        }
        Some(value)
    }
    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_voice_host_overrides_like_the_official_client() {
        // An empty voice_host uses the game server hostname but the separate
        // voice UDP port, never the Minecraft TCP port.
        assert_eq!(
            resolve_voice_addresses("", "127.0.0.1:25565", 24454).unwrap(),
            vec!["127.0.0.1:24454".parse::<SocketAddr>().unwrap()]
        );

        // A numeric voice_host overrides only the UDP port.
        assert_eq!(
            resolve_voice_addresses("25566", "127.0.0.1:25565", 24454).unwrap(),
            vec!["127.0.0.1:25566".parse::<SocketAddr>().unwrap()]
        );

        // A host:port voice_host overrides both host and UDP port.
        assert_eq!(
            resolve_voice_addresses("127.0.0.1:25567", "192.0.2.1:25565", 24454).unwrap(),
            vec!["127.0.0.1:25567".parse::<SocketAddr>().unwrap()]
        );

        // A host-only override keeps the packet's UDP port.
        assert_eq!(
            resolve_voice_addresses("127.0.0.1", "192.0.2.1:25565", 24454).unwrap(),
            vec!["127.0.0.1:24454".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn resolves_bracketed_ipv6_voice_host() {
        assert_eq!(
            resolve_voice_addresses("[::1]:24455", "127.0.0.1:25565", 24454).unwrap(),
            vec!["[::1]:24455".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn rejects_invalid_voice_host_port() {
        assert!(resolve_voice_addresses("127.0.0.1:0", "127.0.0.1:25565", 24454).is_err());
        assert!(resolve_voice_addresses("127.0.0.1:not-a-port", "127.0.0.1:25565", 24454).is_err());
    }

    #[test]
    fn compat20_outbound_auth_packet_carries_player_and_secret() {
        // Upstream 2.6.x AuthenticatePacket.toBytes: playerUUID, then the raw
        // secret; the 2.7.x generation encodes the packet empty instead.
        let encoded = encode_packet(
            &Packet::Authenticate {
                player: [0x11; 16],
                secret: [0x42; 16],
            },
            SvcProfile::Compat20,
        )
        .unwrap();
        let mut expected = vec![AUTHENTICATE];
        expected.extend_from_slice(&[0x11; 16]);
        expected.extend_from_slice(&[0x42; 16]);
        assert_eq!(encoded, expected);
        assert!(
            matches!(decode_packet(&encoded), Some(Packet::Authenticate { player, secret })
            if player == [0x11; 16] && secret == [0x42; 16])
        );
    }

    #[test]
    fn compat21_outbound_auth_packet_is_empty() {
        let encoded = encode_packet(
            &Packet::Authenticate {
                player: [0x11; 16],
                secret: [0x42; 16],
            },
            SvcProfile::Compat21,
        )
        .unwrap();
        assert_eq!(encoded, vec![AUTHENTICATE]);
    }

    #[test]
    fn compat18_wire_is_identical_to_compat20_except_the_handshake_number() {
        // Upstream `outdated/1.21.6-2.5.30`: same AuthenticatePacket layout,
        // same envelope UUID, same type ids - only request_secret differs.
        let auth18 = encode_packet(
            &Packet::Authenticate {
                player: [0x11; 16],
                secret: [0x42; 16],
            },
            SvcProfile::Compat18,
        )
        .unwrap();
        let auth20 = encode_packet(
            &Packet::Authenticate {
                player: [0x11; 16],
                secret: [0x42; 16],
            },
            SvcProfile::Compat20,
        )
        .unwrap();
        assert_eq!(auth18, auth20);
        assert_eq!(
            SvcProfile::Compat18.compatibility_version(),
            18,
            "the generation is told apart only by the probed number"
        );
        assert_eq!(
            SvcProfile::from_compatibility_version(18),
            Some(SvcProfile::Compat18)
        );
        assert_eq!(SvcProfile::from_compatibility_version(19), None);
    }

    #[test]
    fn authenticate_ack_decodes_with_and_without_nonce() {
        // Compat-20 servers answer with an empty packet...
        let empty = vec![AUTHENTICATE_ACK];
        assert!(matches!(
            decode_packet(&empty),
            Some(Packet::AuthenticateAck { nonce: None })
        ));
        // ...compat-21 servers append the 16-byte HMAC challenge nonce.
        let mut challenge = vec![AUTHENTICATE_ACK];
        challenge.extend_from_slice(&[0x77; 16]);
        assert!(matches!(
            decode_packet(&challenge),
            Some(Packet::AuthenticateAck { nonce: Some(n) }) if n == [0x77; 16]
        ));
        // A truncated nonce is rejected outright.
        challenge.truncate(8);
        assert!(decode_packet(&challenge).is_none());
    }

    #[test]
    fn envelope_uses_player_uuid_for_compat20_and_connection_uuid_for_compat21() {
        let info = VoiceServerInfo {
            secret: [0x42; 16],
            port: 24454,
            player: [0x11; 16],
            profile: SvcProfile::Compat20,
            connection_id: [0x22; 16],
            codec: 0,
            mtu: 1024,
            distance: 48.0,
            keep_alive_ms: 1000,
            groups: true,
            host: String::new(),
            allow_recording: true,
        };
        let (socket_a, _keep_a) = bound_loopback_pair();
        let (socket_b, _keep_b) = bound_loopback_pair();
        let target = socket_b.local_addr().unwrap();
        let dual_stack = false;

        send_packet(
            &socket_a,
            &info,
            target,
            dual_stack,
            &Packet::Authenticate {
                player: [0; 16],
                secret: [0; 16],
            },
            info.player,
        )
        .unwrap();
        let mut buffer = [0u8; MAX_DATAGRAM];
        socket_b.recv(&mut buffer).unwrap();
        assert_eq!(buffer[0], MAGIC);
        assert_eq!(&buffer[1..17], &[0x11; 16]);

        let info21 = VoiceServerInfo {
            profile: SvcProfile::Compat21,
            ..info.clone()
        };
        send_packet(
            &socket_a,
            &info21,
            target,
            dual_stack,
            &Packet::Authenticate {
                player: [0; 16],
                secret: [0; 16],
            },
            info21.connection_id,
        )
        .unwrap();
        let length = socket_b.recv(&mut buffer).unwrap();
        assert_eq!(buffer[0], MAGIC);
        assert_eq!(&buffer[1..17], &[0x22; 16]);
        // Same envelope payload shape either way (varint-prefixed body).
        assert!(length > 17);

        // The 2.5.x generation keys the envelope with the player UUID just
        // like 2.6.x.
        let info18 = VoiceServerInfo {
            profile: SvcProfile::Compat18,
            ..info.clone()
        };
        send_packet(
            &socket_a,
            &info18,
            target,
            dual_stack,
            &Packet::Authenticate {
                player: [0; 16],
                secret: [0; 16],
            },
            info18.player,
        )
        .unwrap();
        socket_b.recv(&mut buffer).unwrap();
        assert_eq!(buffer[0], MAGIC);
        assert_eq!(&buffer[1..17], &[0x11; 16]);
    }

    /// Two UDP sockets bound on the loopback, kept alive for the test body.
    fn bound_loopback_pair() -> (UdpSocket, UdpSocket) {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        a.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        (a, b)
    }

    #[test]
    fn mixes_speakers_but_keeps_each_speakers_packets_sequential() {
        let (tx, rx) = mpsc::sync_channel(8);
        let listener = Arc::new(Mutex::new(ListenerPose::default()));
        let mut output = OutputAssembler::new(
            Arc::new(Mutex::new(rx)),
            SAMPLE_RATE as u32,
            listener,
            Arc::new(RwLock::new(HashMap::new())),
        );

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
        // Frame one spans exactly FRAME_SAMPLES outputs; drain the rest of it.
        for _ in 1..FRAME_SAMPLES {
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
        assert_eq!(
            ready.iter().map(|frame| frame.sequence).collect::<Vec<_>>(),
            vec![11, 12]
        );
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
        assert_eq!(
            ready.iter().map(|frame| frame.sequence).collect::<Vec<_>>(),
            vec![21, 22]
        );
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
    fn microphone_gain_amplifies_and_clamps_input_samples() {
        let (tx, rx) = mpsc::sync_channel(2);
        let mut input = InputAssembler::new(
            SAMPLE_RATE as u32,
            tx,
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicU32::new(0.025f32.to_bits())),
            Arc::new(AtomicU32::new(2.0f32.to_bits())),
        );
        for _ in 0..=FRAME_SAMPLES {
            input.push(0.1);
        }
        match rx.try_recv() {
            Ok(Command::Pcm(frame)) => assert!((i32::from(frame[100]) - 6_553).abs() <= 1),
            _ => panic!("expected an amplified PCM frame"),
        }
    }

    #[test]
    fn rejects_varint_overflow() {
        let mut reader = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert!(reader.varint().is_none());
    }

    #[test]
    fn ping_probe_frame_matches_upstream_pinghandler() {
        // PingHandler reads MAGIC, a UUID and a 24-byte payload (UUID + long)
        // and echoes exactly those 24 payload bytes back, unencrypted.
        let id = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        let frame = encode_ping_probe(id, 0x1122334455667788);
        assert_eq!(&frame[..1], &[MAGIC]);
        assert_eq!(&frame[1..17], &PING_V1);
        let mut reader = Reader::new(&frame[17..]);
        let payload = reader.byte_array(usize::MAX).unwrap();
        assert!(reader.done());
        assert_eq!(payload.len(), PING_PONG_BYTES);
        assert_eq!(&payload[..16], &id);
        assert_eq!(&payload[16..], &0x1122334455667788i64.to_be_bytes());
    }

    #[test]
    fn ping_pong_recognition_ignores_other_traffic() {
        let id = [7u8; 16];
        let mut pong = id.to_vec();
        pong.extend_from_slice(&42i64.to_be_bytes());
        assert_eq!(decode_ping_pong(&pong), Some(id));
        // Wrong length: not a pong (encrypted datagrams carry a varint
        // length and never total exactly 24 bytes with a full tag).
        assert_eq!(decode_ping_pong(&pong[..23]), None);
        let mut long = pong.clone();
        long.push(0);
        assert_eq!(decode_ping_pong(&long), None);
    }

    #[test]
    fn envelope_keys_follow_the_generation_and_dedupe() {
        let mut info = VoiceServerInfo {
            secret: [1; 16],
            port: 24454,
            player: [2; 16],
            profile: SvcProfile::Compat20,
            connection_id: [0; 16],
            codec: 0,
            mtu: 1024,
            distance: 48.0,
            keep_alive_ms: 1000,
            groups: true,
            host: String::new(),
            allow_recording: true,
        };
        // Compat-20 keys lookups by the player UUID; the secret UUID is the
        // fallback for custom builds, the zeroed connection UUID is dropped.
        assert_eq!(
            envelope_keys(&info),
            vec![[2; 16], [1; 16]]
        );
        // Compat-21 keys lookups by the per-connection UUID first.
        info.profile = SvcProfile::Compat21;
        info.connection_id = [3; 16];
        assert_eq!(
            envelope_keys(&info),
            vec![[3; 16], [2; 16], [1; 16]]
        );
        // A server that assigns the secret as the connection UUID still
        // yields a sane, duplicate-free candidate list.
        info.connection_id = [1; 16];
        assert_eq!(envelope_keys(&info), vec![[1; 16], [2; 16]]);
    }

    #[test]
    fn envelope_descriptions_name_the_key() {
        let info = VoiceServerInfo {
            secret: [1; 16],
            port: 24454,
            player: [2; 16],
            profile: SvcProfile::Compat20,
            connection_id: [0; 16],
            codec: 0,
            mtu: 1024,
            distance: 48.0,
            keep_alive_ms: 1000,
            groups: true,
            host: String::new(),
            allow_recording: true,
        };
        assert_eq!(describe_envelope(&info, info.player), "player UUID");
        assert_eq!(describe_envelope(&info, info.secret), "secret UUID");
    }
}
