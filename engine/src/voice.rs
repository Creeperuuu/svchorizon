//! Simple Voice Chat UDP client, AES-GCM envelope and Opus frame pipeline.
//! UDP and audio workers are independent of Horizon's game thread. CPAL
//! captures mono PCM and plays decoded Opus frames; callers control PTT.

use crate::recording::Recorder;
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

pub(crate) const MAGIC: u8 = 0xff;
pub(crate) const MAX_DATAGRAM: usize = 2048;
pub(crate) const MAX_OPUS: usize = 1275;
const FRAME_SAMPLES: usize = 960;
const SAMPLE_RATE: i32 = 48_000;
pub(crate) const NONCE_BYTES: usize = 12;
pub(crate) const TAG_BYTES: usize = 16;

pub(crate) const MIC: u8 = 0x01;
pub(crate) const PLAYER_SOUND: u8 = 0x02;
pub(crate) const GROUP_SOUND: u8 = 0x03;
pub(crate) const LOCATION_SOUND: u8 = 0x04;
pub(crate) const AUTHENTICATE: u8 = 0x05;
pub(crate) const AUTHENTICATE_ACK: u8 = 0x06;
pub(crate) const PING: u8 = 0x07;
pub(crate) const KEEP_ALIVE: u8 = 0x08;
pub(crate) const CONNECTION_CHECK: u8 = 0x09;
pub(crate) const CONNECTION_CHECK_ACK: u8 = 0x0a;

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
impl VoiceError {
    /// The underlying `std::io::ErrorKind` when this error wraps an I/O error.
    ///
    /// Non-I/O errors (bad metadata, encryption failures, ...) have no kind and
    /// return `None`, so they are never mistaken for e.g. a Windows
    /// `ConnectionReset` caused by a stray ICMP port-unreachable.
    pub fn io_kind(&self) -> Option<std::io::ErrorKind> {
        match self {
            Self::Io(error) => Some(error.kind()),
            _ => None,
        }
    }

    /// True when the OS reports that the voice server's host rejected a datagram
    /// with an ICMP port-unreachable: `ConnectionReset` on Windows (WSAECONNRESET
    /// on the next send/recv of a connected UDP socket), `ConnectionRefused` on
    /// Linux/macOS. It is not a socket failure: it proves the host is reachable
    /// but nothing is listening on that UDP port.
    pub fn is_icmp_rejection(&self) -> bool {
        matches!(
            self.io_kind(),
            Some(std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused)
        )
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AudioDeviceSelection { input: Option<String>, output: Option<String> }

pub struct VoiceClient {
    command_tx: SyncSender<Command>,
    event_rx: Receiver<VoiceEvent>,
    running: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    whispering: Arc<AtomicBool>,
    voice_activation: Arc<AtomicBool>,
    voice_activation_threshold: Arc<AtomicU32>,
    microphone_gain: Arc<AtomicU32>,
    noise_suppression: Arc<AtomicBool>,
    disabled: Arc<AtomicBool>,
    recorder: Arc<RwLock<Option<Arc<Recorder>>>>,
    talk_timestamps: Arc<Mutex<HashMap<[u8; 16], Instant>>>,
    connected: Arc<AtomicBool>,
    auth_attempts: Arc<AtomicU32>,
    icmp_rejections: Arc<AtomicU32>,
    received_datagrams: Arc<AtomicU32>,
    rejected_replies: Arc<AtomicU32>,
    worker: Option<JoinHandle<()>>,
    audio_worker: Option<JoinHandle<()>>,
    listener: Arc<Mutex<ListenerPose>>,
    player_positions: Arc<RwLock<HashMap<[u8; 16], [f64; 3]>>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
    audio_devices: Arc<RwLock<AudioDeviceSelection>>,
    endpoint: SocketAddr,
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
        eprintln!("[svchorizon] Voice endpoint: {address}");
        match udp_self_test() {
            Ok(()) => eprintln!("[svchorizon] UDP self-test: ok (loopback only: the game process can receive UDP; this does not test the firewall or the network)"),
            Err(error) => eprintln!(
                "[svchorizon] UDP self-test FAILED: {error} - the game process cannot receive UDP at all; a sandbox/antivirus is blocking it"
            ),
        }
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
        // Simple Voice Chat enables its RNNoise denoiser by default.
        let noise_suppression = Arc::new(AtomicBool::new(true));
        let disabled = Arc::new(AtomicBool::new(false));
        let recorder: Arc<RwLock<Option<Arc<Recorder>>>> = Arc::new(RwLock::new(None));
        let talk_timestamps: Arc<Mutex<HashMap<[u8; 16], Instant>>> = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(false));
        let auth_attempts = Arc::new(AtomicU32::new(0));
        let icmp_rejections = Arc::new(AtomicU32::new(0));
        let received_datagrams = Arc::new(AtomicU32::new(0));
        let rejected_replies = Arc::new(AtomicU32::new(0));

        let audio_running = Arc::clone(&running);
        let audio_transmitting = Arc::clone(&transmitting);
        let audio_command_tx = command_tx.clone();
        let audio_events = event_tx.clone();
        let audio_listener = Arc::clone(&listener);
        let audio_settings = Arc::clone(&player_audio_settings);
        let audio_voice_activation = Arc::clone(&voice_activation);
        let audio_voice_threshold = Arc::clone(&voice_activation_threshold);
        let audio_microphone_gain = Arc::clone(&microphone_gain);
        let audio_noise_suppression = Arc::clone(&noise_suppression);
        let audio_disabled = Arc::clone(&disabled);
        let audio_recorder = Arc::clone(&recorder);
        let audio_devices = Arc::new(RwLock::new(AudioDeviceSelection::default()));
        let worker_audio_devices = Arc::clone(&audio_devices);
        let audio_worker = thread::Builder::new()
            .name("svchorizon-audio".into())
            .spawn(move || run_audio_io(
                audio_command_tx, audio_rx, audio_running, audio_transmitting, audio_events, audio_listener,
                audio_settings, audio_voice_activation, audio_voice_threshold, audio_microphone_gain,
                audio_noise_suppression, audio_disabled, audio_recorder, worker_audio_devices,
            ))
            .map_err(VoiceError::Io)?;

        let worker_running = Arc::clone(&running);
        let worker_transmitting = Arc::clone(&transmitting);
        let worker_whispering = Arc::clone(&whispering);
        let worker_voice_activation = Arc::clone(&voice_activation);
        let worker_connected = Arc::clone(&connected);
        let worker_auth_attempts = Arc::clone(&auth_attempts);
        let worker_icmp_rejections = Arc::clone(&icmp_rejections);
        let worker_received = Arc::clone(&received_datagrams);
        let worker_rejected = Arc::clone(&rejected_replies);
        let worker_positions = Arc::clone(&player_positions);
        let worker_talk = Arc::clone(&talk_timestamps);
        let worker = match thread::Builder::new()
            .name("svchorizon-udp".into())
            .spawn(move || {
                run_worker(
                    socket, info, application, command_rx, event_tx, audio_tx,
                    worker_running, worker_transmitting, worker_whispering, worker_voice_activation,
                    worker_connected, worker_auth_attempts, worker_icmp_rejections, worker_received, worker_rejected, worker_positions,
                    worker_talk,
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
            command_tx, event_rx, running, transmitting, whispering,
            voice_activation, voice_activation_threshold, microphone_gain,
            noise_suppression, disabled, recorder, talk_timestamps,
            connected, auth_attempts, icmp_rejections, received_datagrams, rejected_replies,
            worker: Some(worker), audio_worker: Some(audio_worker), listener, player_positions, player_audio_settings,
            audio_devices, endpoint: address,
        })
    }

    /// The voice server UDP endpoint this client sends to.
    pub fn endpoint(&self) -> SocketAddr { self.endpoint }

    /// Enumerate available local microphone devices.
    pub fn input_devices() -> Vec<String> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host().input_devices().map(|devices| devices.filter_map(|d| d.name().ok()).collect()).unwrap_or_default()
    }

    /// Enumerate available local speaker devices.
    pub fn output_devices() -> Vec<String> {
        use cpal::traits::{DeviceTrait, HostTrait};
        cpal::default_host().output_devices().map(|devices| devices.filter_map(|d| d.name().ok()).collect()).unwrap_or_default()
    }

    /// Select a local microphone by CPAL device name; None restores the OS default.
    pub fn set_input_device(&self, name: Option<String>) {
        if let Ok(mut selected) = self.audio_devices.write() { selected.input = name; }
    }

    /// Select a local speaker by CPAL device name; None restores the OS default.
    pub fn set_output_device(&self, name: Option<String>) {
        if let Ok(mut selected) = self.audio_devices.write() { selected.output = name; }
    }

    pub fn selected_audio_devices(&self) -> (Option<String>, Option<String>) {
        self.audio_devices.read().map(|s| (s.input.clone(), s.output.clone())).unwrap_or_default()
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

    /// How many times the voice server's host answered a datagram with an ICMP
    /// port-unreachable. Non-zero means the host is reachable but nothing is
    /// listening on the voice UDP port; zero with unanswered attempts means the
    /// datagrams are silently dropped somewhere on the way.
    pub fn icmp_rejections(&self) -> u32 {
        self.icmp_rejections.load(Ordering::Relaxed)
    }

    /// Datagrams received from the voice server, and how many of them were
    /// rejected as undecodable. Non-zero received with an unauthenticated client
    /// means the server IS answering and the reply is being refused locally.
    pub fn received_datagrams(&self) -> u32 { self.received_datagrams.load(Ordering::Relaxed) }
    pub fn rejected_replies(&self) -> u32 { self.rejected_replies.load(Ordering::Relaxed) }

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

    /// Toggles the RNNoise denoiser on the microphone path (Simple Voice
    /// Chat's noise suppression, on by default like the official client).
    pub fn set_noise_suppression(&self, enabled: bool) {
        self.noise_suppression.store(enabled, Ordering::Relaxed);
    }

    /// The speaker toggle of the official client's voice chat screen: when
    /// disabled, no received audio is played at all.
    pub fn set_disabled(&self, disabled: bool) {
        self.disabled.store(disabled, Ordering::Relaxed);
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }

    pub fn is_noise_suppression_enabled(&self) -> bool {
        self.noise_suppression.load(Ordering::Relaxed)
    }

    /// Attaches or detaches the recording sink. The output path captures
    /// each speaker's post-volume stereo signal into it.
    pub fn set_recorder(&self, recorder: Option<Arc<Recorder>>) {
        if let Ok(mut slot) = self.recorder.write() {
            *slot = recorder;
        }
    }

    /// Whether a speaker was heard within the last 600 ms (the group HUD's
    /// talking indicator, standing in for the official nameplate icons).
    pub fn is_talking(&self, player: [u8; 16]) -> bool {
        self.talk_timestamps
            .lock()
            .map(|timestamps| {
                timestamps
                    .get(&player)
                    .is_some_and(|at| at.elapsed() < Duration::from_millis(600))
            })
            .unwrap_or(false)
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
    let candidates: Vec<SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(VoiceError::Io)?
        .collect();
    if candidates.len() > 1 {
        eprintln!("[svchorizon] {host} resolves to {candidates:?}");
    }
    pick_preferred_address(&candidates)
        .ok_or_else(|| VoiceError::Resolve(format!("{host}:{port}")))
}

/// Pick the address to send voice datagrams to. IPv4 wins whenever the host has
/// one, like the official client (Java prefers IPv4 by default) and like
/// Horizon's own TCP connection (`ipv4_first`). The OS resolver may list an
/// AAAA address first on dual-stack hosts, and Simple Voice Chat servers
/// commonly listen on IPv4 only, so taking the first entry blindly can send
/// every authentication datagram into the void.
fn pick_preferred_address(candidates: &[SocketAddr]) -> Option<SocketAddr> {
    candidates
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| candidates.first())
        .copied()
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
    auth_attempts: Arc<AtomicU32>,
    icmp_rejections: Arc<AtomicU32>,
    received_datagrams: Arc<AtomicU32>,
    rejected_replies: Arc<AtomicU32>,
    player_positions: Arc<RwLock<HashMap<[u8; 16], [f64; 3]>>>,
    talk_timestamps: Arc<Mutex<HashMap<[u8; 16], Instant>>>,
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
    let mut noted_padding = false;
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
            if let Err(error) = send_packet(&socket, &info, &Packet::Authenticate {
                player: info.player,
                secret: info.secret,
            }) {
                // Windows reports an ICMP port-unreachable from a previous
                // datagram as WSAECONNRESET on the next send; that is a
                // reachability signal, not a socket failure. It is counted and
                // explained by the unanswered-attempts warning below.
                if error.is_icmp_rejection() {
                    icmp_rejections.fetch_add(1, Ordering::Relaxed);
                } else {
                    eprintln!("[svchorizon] sending authentication failed: {error}");
                }
            }
            let attempts = auth_attempts.fetch_add(1, Ordering::Relaxed) + 1;
            if attempts % 5 == 0 {
                eprintln!(
                    "[svchorizon] Voice server has not answered {attempts} authentication attempts yet (voice UDP port blocked or wrong endpoint?)"
                );
            }
            if attempts == 15 {
                let peer = socket.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "<unknown>".into());
                let rejections = icmp_rejections.load(Ordering::Relaxed);
                let replies = received_datagrams.load(Ordering::Relaxed);
                if replies > 0 {
                    eprintln!(
                        "[svchorizon] The voice server at {peer} DID send {replies} datagram(s) back, so the network path works; the replies are being refused locally (see the 'rejected it' lines above for the reason)."
                    );
                } else if rejections > 0 {
                    eprintln!(
                        "[svchorizon] The host of {peer} answered {rejections} datagram(s) with an ICMP port-unreachable: the machine is reachable but NOTHING IS LISTENING on that UDP port. The Simple Voice Chat server is not bound to it, or the hosting panel/NAT does not map that UDP port to the server. Check the plugin's `port` setting and the server's network allocations (UDP)."
                    );
                } else {
                    eprintln!(
                        "[svchorizon] The voice server at {peer} never answered authentication for 15s and sent no ICMP rejection, so the datagrams are being silently dropped or ignored. Most often the host/cloud firewall does not allow inbound UDP on that port. If official voice chat works on this server from this PC, check that horizon-client.exe is allowed in Windows Firewall (inbound UDP) and in any antivirus with network protection. (The loopback self-test above does not exercise the firewall or the network.)"
                    );
                }
            }
            last_auth = Some(Instant::now());
        } else if authenticated && !validated
            && last_check.is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            if let Err(error) = send_packet(&socket, &info, &Packet::ConnectionCheck) {
                if error.is_icmp_rejection() {
                    icmp_rejections.fetch_add(1, Ordering::Relaxed);
                } else {
                    eprintln!("[svchorizon] sending connection check failed: {error}");
                }
            }
            last_check = Some(Instant::now());
        }

        if validated && last_keepalive.elapsed() > keepalive_timeout {
            connected.store(false, Ordering::Release);
            let _ = events.try_send(VoiceEvent::Disconnected("voice server keepalive timed out".into()));
            break;
        }

        match socket.recv(&mut receive) {
            Ok(length) => {
                let received = received_datagrams.fetch_add(1, Ordering::Relaxed) + 1;
                let packet = match decode_server_datagram_checked(&receive[..length], &info.secret) {
                    Ok((packet, trailing)) => {
                        if trailing > 0 && !noted_padding {
                            noted_padding = true;
                            eprintln!(
                                "[svchorizon] The voice server pads its packets ({trailing} trailing byte(s) after a {length}-byte datagram's packet); ignoring the padding like the official client does."
                            );
                        }
                        packet
                    }
                    Err(reason) => {
                        let rejected = rejected_replies.fetch_add(1, Ordering::Relaxed) + 1;
                        if rejected <= 3 {
                            let head: Vec<String> = receive[..length.min(8)].iter().map(|b| format!("{b:02x}")).collect();
                            eprintln!(
                                "[svchorizon] Received a {length}-byte datagram from the voice server but rejected it: {reason} (first bytes {}; reply #{received})",
                                head.join(" ")
                            );
                        }
                        continue;
                    }
                };
                match packet {
                    Packet::AuthenticateAck => {
                        authenticated = true;
                        auth_attempts.store(0, Ordering::Relaxed);
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
                        if let Err(error) = send_packet(&socket, &info, &Packet::KeepAlive) {
                            if error.is_icmp_rejection() {
                                icmp_rejections.fetch_add(1, Ordering::Relaxed);
                            } else {
                                eprintln!("[svchorizon] sending keepalive failed: {error}");
                            }
                        }
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
                                emit_audio(&mut decoders, &audio, frame, &talk_timestamps);
                            }
                        }
                    }
                    Packet::GroupSound { sender, opus, sequence, .. } => {
                        if validated {
                            let ready = jitter.push(PendingAudio {
                                sender, sequence, opus, position: None, max_distance: None,
                            }, Instant::now());
                            for frame in ready {
                                emit_audio(&mut decoders, &audio, frame, &talk_timestamps);
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
                                emit_audio(&mut decoders, &audio, frame, &talk_timestamps);
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Windows surfaces an ICMP port-unreachable as WSAECONNRESET on a
            // connected UDP socket (Linux/macOS: ConnectionRefused). Keep
            // running, but count it: it is the one signal that tells "host
            // reachable, nothing listening" apart from "silently dropped".
            Err(error) if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused
            ) => {
                icmp_rejections.fetch_add(1, Ordering::Relaxed);
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
            emit_audio(&mut decoders, &audio, frame, &talk_timestamps);
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
    talk_timestamps: &Arc<Mutex<HashMap<[u8; 16], Instant>>>,
) {
    if pending.opus.len() > MAX_OPUS { return; }
    if let Ok(mut timestamps) = talk_timestamps.lock() {
        timestamps.insert(pending.sender, Instant::now());
        timestamps.retain(|_, at| at.elapsed() < Duration::from_secs(30));
    }
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
    noise_suppression: Arc<AtomicBool>,
    disabled: Arc<AtomicBool>,
    recorder: Arc<RwLock<Option<Arc<Recorder>>>>,
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
                Some(name) => host.input_devices().ok().and_then(|ds| ds.filter(|d| d.name().ok().as_deref() == Some(name)).next())
                    .or_else(|| host.default_input_device()),
                None => host.default_input_device(),
            };
            match input {
                Some(device) => {
                    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
                    match build_input_stream(&device, command_tx.clone(), Arc::clone(&transmitting),
                        Arc::clone(&voice_activation), Arc::clone(&voice_activation_threshold),
                        Arc::clone(&microphone_gain), Arc::clone(&noise_suppression), events.clone()) {
                        Ok(stream) => match stream.play() {
                            Ok(()) => {
                                eprintln!("[svchorizon] Microphone opened: {name}");
                                streams.push(stream);
                            }
                            Err(err) => { let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("could not start microphone: {err}"))); }
                        },
                        Err(err) => { let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("microphone unavailable: {err}"))); }
                    }
                }
                None => { let _ = events.try_send(VoiceEvent::AudioUnavailable("no selected/default microphone device".into())); }
            }
            let output = match selected.output.as_deref() {
                Some(name) => host.output_devices().ok().and_then(|ds| ds.filter(|d| d.name().ok().as_deref() == Some(name)).next())
                    .or_else(|| host.default_output_device()),
                None => host.default_output_device(),
            };
            match output {
                Some(device) => {
                    let name = device.name().unwrap_or_else(|_| "<unknown>".into());
                    match build_output_stream(&device, Arc::clone(&shared_rx), events.clone(),
                        Arc::clone(&listener), Arc::clone(&player_audio_settings), Arc::clone(&disabled), Arc::clone(&recorder)) {
                        Ok(stream) => match stream.play() {
                            Ok(()) => {
                                eprintln!("[svchorizon] Speaker opened: {name}");
                                streams.push(stream);
                            }
                            Err(err) => { let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("could not start speaker: {err}"))); }
                        },
                        Err(err) => { let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("speaker unavailable: {err}"))); }
                    }
                }
                None => { let _ = events.try_send(VoiceEvent::AudioUnavailable("no selected/default speaker device".into())); }
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
    noise_suppression: Arc<AtomicBool>,
    denoiser: Option<crate::rnnoise::Denoiser>,
}
impl InputAssembler {
    fn new(
        input_rate: u32,
        tx: SyncSender<Command>,
        transmitting: Arc<AtomicBool>,
        voice_activation: Arc<AtomicBool>,
        voice_activation_threshold: Arc<AtomicU32>,
        microphone_gain: Arc<AtomicU32>,
        noise_suppression: Arc<AtomicBool>,
    ) -> Self {
        // The RNNoise denoiser is built lazily per stream rebuild; the model
        // weights behind it are parsed once and shared.
        let denoiser = match crate::rnnoise::Denoiser::new() {
            Some(denoiser) => Some(denoiser),
            None => {
                eprintln!("[svchorizon] Noise suppression unavailable (denoiser init failed)");
                None
            }
        };
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
            noise_suppression,
            denoiser,
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
                // Noise suppression runs on the denoiser's 480-sample frames
                // before the activation decision, so voice activation also
                // sees the cleaned signal (like upstream's mic processors).
                if self.noise_suppression.load(Ordering::Relaxed)
                    && let Some(denoiser) = self.denoiser.as_mut()
                {
                    for half in self.frame.chunks_exact_mut(crate::rnnoise::Denoiser::frame_size()) {
                        let mut block = [0.0f32; crate::rnnoise::Denoiser::frame_size()];
                        for (value, sample) in block.iter_mut().zip(half.iter()) {
                            *value = f32::from(*sample) / 32768.0;
                        }
                        denoiser.process(&mut block);
                        for (sample, value) in half.iter_mut().zip(block.iter()) {
                            *sample = (value * 32767.0).clamp(-32768.0, 32767.0) as i16;
                        }
                    }
                }
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
    noise_suppression: Arc<AtomicBool>,
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
                Arc::clone(&microphone_gain), Arc::clone(&noise_suppression),
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
                Arc::clone(&microphone_gain), Arc::clone(&noise_suppression),
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
            let mut input = InputAssembler::new(rate, tx, transmitting, Arc::clone(&voice_activation), Arc::clone(&voice_activation_threshold), Arc::clone(&microphone_gain), Arc::clone(&noise_suppression));
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
    rx: Arc<Mutex<Receiver<AudioFrame>>>,
    speakers: HashMap<[u8; 16], VecDeque<PlaybackFrame>>,
    step: f64,
    listener: Arc<Mutex<ListenerPose>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
    disabled: Arc<AtomicBool>,
    recorder: Arc<RwLock<Option<Arc<Recorder>>>>,
    recording_buffers: HashMap<[u8; 16], Vec<i16>>,
}

impl OutputAssembler {
    const MAX_SPEAKERS: usize = 32;
    const MAX_QUEUED_FRAMES_PER_SPEAKER: usize = 6;
    /// Recording chunks of about 20 ms at 48 kHz stereo.
    const RECORDING_CHUNK_SAMPLES: usize = 960 * 2;

    fn new(
        rx: Arc<Mutex<Receiver<AudioFrame>>>,
        output_rate: u32,
        listener: Arc<Mutex<ListenerPose>>,
        player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
        disabled: Arc<AtomicBool>,
        recorder: Arc<RwLock<Option<Arc<Recorder>>>>,
    ) -> Self {
        Self {
            rx,
            speakers: HashMap::with_capacity(8),
            step: SAMPLE_RATE as f64 / f64::from(output_rate.max(1)),
            listener,
            player_audio_settings,
            disabled,
            recorder,
            recording_buffers: HashMap::new(),
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
        let pan: f32 = if distance > 0.001 {
            ((dx * listener.yaw.cos() - dz * listener.yaw.sin()) / distance).clamp(-1.0, 1.0) as f32
        } else { 0.0 };
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
            let Some(frame) = frame else { break; };
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

        if self.disabled.load(Ordering::Relaxed) {
            return [0, 0];
        }
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        let step = self.step;
        let mut recorded: Vec<([u8; 16], f32, f32)> = Vec::new();
        self.speakers.retain(|speaker, queue| {
            let completed = if let Some(voice) = queue.front_mut() {
                if voice.position < FRAME_SAMPLES as f64 {
                    let index = voice.position.floor() as usize;
                    let next = (index + 1).min(FRAME_SAMPLES - 1);
                    let fraction = (voice.position - index as f64) as f32;
                    let pcm = &voice.frame.pcm;
                    let sample = pcm[index] as f32
                        + (pcm[next] as f32 - pcm[index] as f32) * fraction;
                    left += sample * voice.left_gain;
                    right += sample * voice.right_gain;
                    recorded.push((*speaker, sample * voice.left_gain, sample * voice.right_gain));
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
        for (speaker, left_sample, right_sample) in recorded {
            self.record_sample(&speaker, left_sample, right_sample);
        }

        [
            left.clamp(i16::MIN as f32, i16::MAX as f32) as i16,
            right.clamp(i16::MIN as f32, i16::MAX as f32) as i16,
        ]
    }

    /// Feeds one post-volume stereo sample of one speaker into the active
    /// recording, flushing 20 ms chunks to the recorder thread.
    fn record_sample(&mut self, speaker: &[u8; 16], left: f32, right: f32) {
        let Ok(slot) = self.recorder.read() else { return };
        let Some(recorder) = slot.as_ref() else { return };
        let buffer = self.recording_buffers.entry(*speaker).or_default();
        buffer.push((left.clamp(i16::MIN as f32, i16::MAX as f32)) as i16);
        buffer.push((right.clamp(i16::MIN as f32, i16::MAX as f32)) as i16);
        if buffer.len() >= Self::RECORDING_CHUNK_SAMPLES {
            recorder.capture(*speaker, buffer);
            buffer.clear();
        }
    }
}

fn build_output_stream(
    device: &cpal::Device,
    rx: Arc<Mutex<Receiver<AudioFrame>>>,
    events: SyncSender<VoiceEvent>,
    listener: Arc<Mutex<ListenerPose>>,
    player_audio_settings: Arc<RwLock<HashMap<[u8; 16], PlayerAudioSettings>>>,
    disabled: Arc<AtomicBool>,
    recorder: Arc<RwLock<Option<Arc<Recorder>>>>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = usize::from(config.channels).max(1);
    let rate = config.sample_rate.0;
    match format {
        cpal::SampleFormat::F32 => {
            let mut output = OutputAssembler::new(rx, rate, Arc::clone(&listener), Arc::clone(&player_audio_settings), Arc::clone(&disabled), Arc::clone(&recorder));
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
            let mut output = OutputAssembler::new(rx, rate, Arc::clone(&listener), Arc::clone(&player_audio_settings), Arc::clone(&disabled), Arc::clone(&recorder));
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
            let mut output = OutputAssembler::new(rx, rate, listener, player_audio_settings, disabled, recorder);
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
pub(crate) enum Packet {
    Mic { opus: Vec<u8>, sequence: i64, whispering: bool },
    /// SVC 2.6.27 authentication: player UUID followed by the raw 16-byte
    /// secret issued in the SecretPacket (AuthenticatePacket.toBytes order).
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
    // SVC 2.6.27 (compatibility 20): the server's secret map is keyed by the
    // player's profile UUID, so every client datagram carries that UUID and
    // the server resolves the AES key with Server.getSecret(playerUUID).
    datagram.extend_from_slice(&info.player);
    let mut body = Vec::with_capacity(NONCE_BYTES + encrypted.len());
    body.extend_from_slice(&nonce);
    body.extend_from_slice(&encrypted);
    write_byte_array(&mut datagram, &body);
    socket.send(&datagram)?;
    Ok(())
}

/// Decode a server datagram, or say why it cannot be used. The reason is only
/// for diagnostics: a server that answers but whose reply we reject looks
/// exactly like a silent network from the outside.
fn decode_server_datagram_checked(datagram: &[u8], secret: &[u8; 16]) -> Result<(Packet, usize), String> {
    let mut reader = Reader::new(datagram);
    if reader.u8() != Some(MAGIC) { return Err("bad magic byte".into()); }
    let Some(encrypted) = reader.byte_array(MAX_DATAGRAM) else { return Err("bad length prefix".into()); };
    if !reader.done() { return Err("trailing bytes after the payload".into()); }
    if encrypted.len() < NONCE_BYTES + TAG_BYTES { return Err("payload shorter than IV + GCM tag".into()); }
    let nonce: [u8; NONCE_BYTES] = encrypted[..NONCE_BYTES].try_into().map_err(|_| "bad IV".to_owned())?;
    let mut plaintext = encrypted[NONCE_BYTES..].to_vec();
    let key = LessSafeKey::new(UnboundKey::new(&aead::AES_128_GCM, secret).map_err(|_| "bad key".to_owned())?);
    let opened = key
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut plaintext)
        .map_err(|_| "AES-GCM authentication failed (the reply was not encrypted with our secret)".to_owned())?;
    decode_packet_prefix(opened).ok_or_else(|| {
        let head: Vec<String> = opened.iter().take(16).map(|b| format!("{b:02x}")).collect();
        format!(
            "decrypted, but the {}-byte packet is unknown or malformed (type byte {}, first plaintext bytes {})",
            opened.len(),
            opened.first().map_or("<none>".to_owned(), |b| format!("0x{b:02x}")),
            head.join(" ")
        )
    })
}

#[cfg(test)]
fn decode_server_datagram(datagram: &[u8], secret: &[u8; 16]) -> Option<Packet> {
    decode_server_datagram_checked(datagram, secret).ok().map(|(packet, _)| packet)
}

pub(crate) fn encode_packet(packet: &Packet) -> Option<Vec<u8>> {
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
        // Server-side packets: the hosted-voice server encodes these.
        Packet::AuthenticateAck => out.push(AUTHENTICATE_ACK),
        Packet::ConnectionCheckAck => out.push(CONNECTION_CHECK_ACK),
        Packet::PlayerSound { channel, sender, opus, sequence, distance, whispering, category } => {
            if opus.len() > MAX_OPUS || category.is_some() { return None; }
            out.push(PLAYER_SOUND);
            out.extend_from_slice(channel);
            out.extend_from_slice(sender);
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&sequence.to_be_bytes());
            out.extend_from_slice(&distance.to_bits().to_be_bytes());
            out.push(u8::from(*whispering));
        }
        Packet::GroupSound { channel, sender, opus, sequence, category } => {
            if opus.len() > MAX_OPUS || category.is_some() { return None; }
            out.push(GROUP_SOUND);
            out.extend_from_slice(channel);
            out.extend_from_slice(sender);
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&sequence.to_be_bytes());
            out.push(0);
        }
        Packet::LocationSound { channel, sender, location, opus, sequence, distance, category } => {
            if opus.len() > MAX_OPUS || category.is_some() { return None; }
            out.push(LOCATION_SOUND);
            out.extend_from_slice(channel);
            out.extend_from_slice(sender);
            for value in location { out.extend_from_slice(&value.to_bits().to_be_bytes()); }
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&sequence.to_be_bytes());
            out.extend_from_slice(&distance.to_bits().to_be_bytes());
            out.push(0);
        }
    }
    Some(out)
}

/// Strict decode: the whole buffer must be consumed. Used for our own encodings.
#[cfg(test)]
fn decode_packet(bytes: &[u8]) -> Option<Packet> {
    match decode_packet_prefix(bytes) {
        Some((packet, 0)) => Some(packet),
        _ => None,
    }
}

/// Decode one packet from the start of `bytes` and return how many bytes follow
/// it. The reference Java client reads the type byte and the fields it expects
/// and never looks at what remains (`NetworkMessage.readFromBytes`), so a server
/// or proxy that pads its packets is fully valid for it; we must tolerate that
/// too instead of treating padding as corruption.
pub(crate) fn decode_packet_prefix(bytes: &[u8]) -> Option<(Packet, usize)> {
    let mut r = Reader::new(bytes);
    let packet = match r.u8()? {
        MIC => Packet::Mic { opus: r.byte_array(MAX_OPUS)?, sequence: r.i64()?, whispering: r.boolean()? },
        AUTHENTICATE => Packet::Authenticate {
            player: r.array16()?,
            secret: r.array16()?,
        },
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
    Some((packet, bytes.len() - r.pos))
}

pub(crate) fn write_varint(out: &mut Vec<u8>, mut value: usize) {
    while value > 0x7f { out.push((value as u8 & 0x7f) | 0x80); value >>= 7; }
    out.push(value as u8);
}
pub(crate) fn write_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(out, bytes.len()); out.extend_from_slice(bytes);
}

pub(crate) struct Reader<'a> { bytes: &'a [u8], pos: usize }
impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self { Self { bytes, pos: 0 } }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let result = self.bytes.get(self.pos..end)?; self.pos = end; Some(result)
    }
    pub(crate) fn u8(&mut self) -> Option<u8> { Some(*self.take(1)?.first()?) }
    pub(crate) fn boolean(&mut self) -> Option<bool> { match self.u8()? { 0 => Some(false), 1 => Some(true), _ => None } }
    pub(crate) fn array16(&mut self) -> Option<[u8; 16]> { self.take(16)?.try_into().ok() }
    pub(crate) fn i16(&mut self) -> Option<i16> { Some(i16::from_be_bytes(self.take(2)?.try_into().ok()?)) }
    pub(crate) fn i32(&mut self) -> Option<i32> { Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?)) }
    fn i64(&mut self) -> Option<i64> { Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?)) }
    fn f32(&mut self) -> Option<f32> { Some(f32::from_bits(u32::from_be_bytes(self.take(4)?.try_into().ok()?))) }
    pub(crate) fn f64(&mut self) -> Option<f64> { Some(f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into().ok()?))) }
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
    pub(crate) fn byte_array(&mut self, max: usize) -> Option<Vec<u8>> {
        let len = self.varint()?;
        if len > max { return None; }
        Some(self.take(len)?.to_vec())
    }
    pub(crate) fn string(&mut self, max_chars: usize) -> Option<String> {
        let bytes = self.byte_array(max_chars.checked_mul(4)?)?;
        let value = String::from_utf8(bytes).ok()?;
        if value.chars().count() > max_chars { return None; }
        Some(value)
    }
    pub(crate) fn done(&self) -> bool { self.pos == self.bytes.len() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A datagram the way the server writes it (NetworkMessage.writeServer):
    /// magic, then a VarInt-prefixed `IV || AES-GCM(type || body) || tag`.
    fn server_datagram(secret: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let nonce = [7u8; NONCE_BYTES];
        let key = LessSafeKey::new(UnboundKey::new(&aead::AES_128_GCM, secret).unwrap());
        let mut sealed = plain.to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut sealed).unwrap();
        let mut body = nonce.to_vec();
        body.extend_from_slice(&sealed);
        let mut datagram = vec![MAGIC];
        write_byte_array(&mut datagram, &body);
        datagram
    }

    #[test]
    fn server_replies_are_decoded_or_rejected_with_a_reason() {
        let secret = [3u8; 16];
        let ack = server_datagram(&secret, &[AUTHENTICATE_ACK]);
        assert!(matches!(decode_server_datagram_checked(&ack, &secret), Ok((Packet::AuthenticateAck, 0))));

        let mut wrong_magic = ack.clone();
        wrong_magic[0] = 0x00;
        assert_eq!(decode_server_datagram_checked(&wrong_magic, &secret).unwrap_err(), "bad magic byte");

        // Encrypted with a different secret: the failure that makes a working
        // server look dead (stale or mismatched secret).
        let other = server_datagram(&[9u8; 16], &[AUTHENTICATE_ACK]);
        assert!(decode_server_datagram_checked(&other, &secret).unwrap_err().starts_with("AES-GCM authentication failed"));

        let mut trailing = ack.clone();
        trailing.push(0);
        assert_eq!(decode_server_datagram_checked(&trailing, &secret).unwrap_err(), "trailing bytes after the payload");
        assert!(decode_server_datagram_checked(&[MAGIC], &secret).is_err());

        let unknown = server_datagram(&secret, &[0x7e, 1, 2, 3]);
        let reason = decode_server_datagram_checked(&unknown, &secret).unwrap_err();
        assert!(reason.starts_with("decrypted, but the 4-byte packet"), "{reason}");
        assert!(reason.contains("type byte 0x7e") && reason.contains("7e 01 02 03"), "{reason}");
    }

    #[test]
    fn a_padded_server_reply_is_accepted_like_the_official_client_does() {
        // The 287-byte replies seen from a live server: an AuthenticateAck whose
        // plaintext is padded out to 256 bytes. The Java reader ignores whatever
        // follows the fields it expects, so we must too.
        let secret = [5u8; 16];
        let mut plain = vec![AUTHENTICATE_ACK];
        plain.extend((0..255u16).map(|i| (i * 7) as u8));
        assert_eq!(plain.len(), 256);
        let datagram = server_datagram(&secret, &plain);
        assert_eq!(datagram.len(), 287);
        assert!(matches!(
            decode_server_datagram_checked(&datagram, &secret),
            Ok((Packet::AuthenticateAck, 255))
        ));
        // Our own strict decoder (used for round trips) is unchanged.
        assert!(decode_packet(&plain).is_none());
        assert!(decode_packet(&[AUTHENTICATE_ACK]).is_some());
    }

    #[test]
    fn a_real_closed_udp_port_is_classified_as_an_icmp_rejection() {
        // Bind then drop to get a loopback port nothing listens on, then talk to
        // it over a connected UDP socket exactly like the voice worker does. The
        // OS answers with an ICMP port-unreachable, surfaced as ConnectionReset
        // (Windows) or ConnectionRefused (Linux/macOS).
        let closed = {
            let probe = UdpSocket::bind("127.0.0.1:0").unwrap();
            probe.local_addr().unwrap()
        };
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(closed).unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut rejected = false;
        let mut buffer = [0u8; 16];
        for _ in 0..5 {
            let _ = socket.send(b"x");
            if let Err(error) = socket.recv(&mut buffer) {
                if VoiceError::from(error).is_icmp_rejection() {
                    rejected = true;
                    break;
                }
            }
        }
        assert!(rejected, "closed loopback UDP port was not reported as an ICMP rejection");
    }

    #[test]
    fn voice_endpoint_prefers_ipv4_over_an_earlier_ipv6_address() {
        let v6: SocketAddr = "[2001:db8::1]:24454".parse().unwrap();
        let v4: SocketAddr = "192.0.2.7:24454".parse().unwrap();
        let other_v4: SocketAddr = "192.0.2.8:24454".parse().unwrap();
        // IPv6 listed first (common on Windows dual-stack lookups): IPv4 still wins.
        assert_eq!(pick_preferred_address(&[v6, v4, other_v4]), Some(v4));
        // Only IPv6 available: use it rather than fail.
        assert_eq!(pick_preferred_address(&[v6]), Some(v6));
        assert_eq!(pick_preferred_address(&[]), None);
    }

    #[test]
    fn peer_socket_address_text_is_accepted_as_the_game_server_host() {
        // The mod passes the real connected peer (`SocketAddr::to_string()`) as
        // the fallback host; both families must keep their voice UDP port.
        assert_eq!(
            resolve_voice_address("", "203.0.113.9:25565", 24454).unwrap(),
            "203.0.113.9:24454".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            resolve_voice_address("", "[2001:db8::9]:25565", 24454).unwrap(),
            "[2001:db8::9]:24454".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn voice_error_reports_icmp_rejection_only_for_io_errors() {
        // Regression: send_packet() returns VoiceError, which has no `kind()`
        // of its own. The worker's "ignore Windows WSAECONNRESET" checks must
        // go through VoiceError, not assume a bare std::io::Error.
        let reset = VoiceError::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert_eq!(reset.io_kind(), Some(std::io::ErrorKind::ConnectionReset));
        assert!(reset.is_icmp_rejection());

        // Linux/macOS report the same ICMP port-unreachable as ConnectionRefused.
        let refused = VoiceError::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(refused.is_icmp_rejection());

        let denied = VoiceError::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert_eq!(denied.io_kind(), Some(std::io::ErrorKind::PermissionDenied));
        assert!(!denied.is_icmp_rejection());

        // Non-I/O failures must never be swallowed as a reachability signal.
        assert_eq!(VoiceError::WorkerStopped.io_kind(), None);
        assert!(!VoiceError::WorkerStopped.is_icmp_rejection());
        assert!(!VoiceError::InvalidServerInfo("x").is_icmp_rejection());
    }

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
    fn outbound_auth_packet_carries_player_and_secret() {
        // SVC 2.6.27 AuthenticatePacket.toBytes: playerUUID, then the raw secret.
        let encoded = encode_packet(&Packet::Authenticate {
            player: [0x11; 16],
            secret: [0x42; 16],
        }).unwrap();
        let mut expected = vec![AUTHENTICATE];
        expected.extend_from_slice(&[0x11; 16]);
        expected.extend_from_slice(&[0x42; 16]);
        assert_eq!(encoded, expected);
        assert!(matches!(decode_packet(&encoded), Some(Packet::Authenticate { player, secret })
            if player == [0x11; 16] && secret == [0x42; 16]));
    }

    #[test]
    fn mixes_speakers_but_keeps_each_speakers_packets_sequential() {
        let (tx, rx) = mpsc::sync_channel(8);
        let listener = Arc::new(Mutex::new(ListenerPose::default()));
        let mut output = OutputAssembler::new(Arc::new(Mutex::new(rx)), SAMPLE_RATE as u32, listener, Arc::new(RwLock::new(HashMap::new())), Arc::new(AtomicBool::new(false)), Arc::new(RwLock::new(None)));

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
            Arc::new(AtomicBool::new(false)),
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
            Arc::new(AtomicBool::new(false)),
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
            Arc::new(AtomicBool::new(false)),
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
}
