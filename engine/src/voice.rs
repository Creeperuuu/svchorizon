//! Simple Voice Chat UDP client, AES-GCM envelope and Opus frame pipeline.
//! UDP and audio workers are independent of Horizon's game thread. CPAL
//! captures mono PCM and plays decoded Opus frames; callers control PTT.

use crate::{Application, OpusDecoder, OpusEncoder};
use ring::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
    connected: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    audio_worker: Option<JoinHandle<()>>,
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

        let host = if info.host.trim().is_empty() { fallback_host } else { &info.host };
        let address = resolve_voice_address(host, info.port)?;
        let bind_address = if address.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socket = UdpSocket::bind(bind_address)?;
        socket.connect(address)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;

        let (command_tx, command_rx) = mpsc::sync_channel(32);
        let (event_tx, event_rx) = mpsc::sync_channel(32);
        let (audio_tx, audio_rx) = mpsc::sync_channel(32);
        let running = Arc::new(AtomicBool::new(true));
        let transmitting = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));

        let audio_running = Arc::clone(&running);
        let audio_transmitting = Arc::clone(&transmitting);
        let audio_command_tx = command_tx.clone();
        let audio_events = event_tx.clone();
        let audio_worker = thread::Builder::new()
            .name("svchorizon-audio".into())
            .spawn(move || run_audio_io(
                audio_command_tx, audio_rx, audio_running, audio_transmitting, audio_events,
            ))
            .ok();

        let worker_running = Arc::clone(&running);
        let worker_transmitting = Arc::clone(&transmitting);
        let worker_connected = Arc::clone(&connected);
        let worker = match thread::Builder::new()
            .name("svchorizon-udp".into())
            .spawn(move || {
                run_worker(
                    socket, info, application, command_rx, event_tx, audio_tx,
                    worker_running, worker_transmitting, worker_connected,
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
            command_tx, event_rx, running, transmitting, connected,
            worker: Some(worker), audio_worker,
        })
    }

    /// Enable only while push-to-talk is held or voice activation is active.
    pub fn set_transmitting(&self, enabled: bool) {
        self.transmitting.store(enabled, Ordering::Relaxed);
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
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

fn resolve_voice_address(host: &str, port: u16) -> Result<SocketAddr, VoiceError> {
    let host = host.trim();
    if host.is_empty() { return Err(VoiceError::Resolve("empty host".into())); }
    if let Ok(address) = host.parse::<SocketAddr>() {
        return Ok(SocketAddr::new(address.ip(), port));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let host = if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else if host.matches(':').count() == 1 {
        host.split_once(':').map_or(host, |(name, _)| name)
    } else {
        host
    };
    (host, port).to_socket_addrs()
        .map_err(VoiceError::Io)?
        .next()
        .ok_or_else(|| VoiceError::Resolve(format!("{host}:{port}")))
}

fn run_worker(
    socket: UdpSocket,
    info: VoiceServerInfo,
    application: Application,
    commands: Receiver<Command>,
    events: SyncSender<VoiceEvent>,
    audio: SyncSender<[i16; FRAME_SAMPLES]>,
    running: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
) {
    let mut encoder = match OpusEncoder::new(SAMPLE_RATE, 1, application) {
        Ok(value) => value,
        Err(_) => {
            let _ = events.try_send(VoiceEvent::Disconnected("Opus encoder initialization failed".into()));
            return;
        }
    };
    let mut decoders: HashMap<[u8; 16], OpusDecoder> = HashMap::new();
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
                    if validated && transmitting.load(Ordering::Relaxed) {
                        let mut opus = [0u8; 1276];
                        if let Ok(length) = encoder.encode_i16(&pcm, FRAME_SAMPLES, &mut opus) {
                            let packet = Packet::Mic {
                                opus: opus[..length].to_vec(),
                                sequence,
                                whispering: false,
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
                    Packet::PlayerSound { channel, sender, opus, distance, whispering, category, .. } => {
                        if validated {
                            emit_audio(&mut decoders, &audio, channel, sender, None,
                                Some(distance), whispering, category, &opus);
                        }
                    }
                    Packet::GroupSound { channel, sender, opus, category, .. } => {
                        if validated {
                            emit_audio(&mut decoders, &audio, channel, sender, None,
                                None, false, category, &opus);
                        }
                    }
                    Packet::LocationSound { channel, sender, location, opus, distance, category, .. } => {
                        if validated {
                            emit_audio(&mut decoders, &audio, channel, sender, Some(location),
                                Some(distance), false, category, &opus);
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
    }
    connected.store(false, Ordering::Release);
    running.store(false, Ordering::Release);
}

fn emit_audio(
    decoders: &mut HashMap<[u8; 16], OpusDecoder>,
    events: &SyncSender<[i16; FRAME_SAMPLES]>,
    _channel: [u8; 16],
    sender: [u8; 16],
    _position: Option<[f64; 3]>,
    _distance: Option<f32>,
    _whispering: bool,
    _category: Option<String>,
    opus: &[u8],
) {
    if opus.is_empty() || opus.len() > MAX_OPUS { return; }
    if !decoders.contains_key(&sender) {
        let Ok(decoder) = OpusDecoder::new(SAMPLE_RATE, 1) else { return; };
        decoders.insert(sender, decoder);
    }
    let Some(decoder) = decoders.get_mut(&sender) else { return; };
    let mut float_pcm = [0.0f32; FRAME_SAMPLES];
    let Ok(samples) = decoder.decode(opus, FRAME_SAMPLES, &mut float_pcm) else { return; };
    let mut pcm = [0i16; FRAME_SAMPLES];
    for (out, input) in pcm.iter_mut().zip(float_pcm.iter()).take(samples.min(FRAME_SAMPLES)) {
        *out = (input.clamp(-1.0, 1.0) * 32767.0) as i16;
    }
    let _ = events.try_send(pcm);
}

fn run_audio_io(
    command_tx: SyncSender<Command>,
    audio_rx: Receiver<[i16; FRAME_SAMPLES]>,
    running: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    events: SyncSender<VoiceEvent>,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let mut streams = Vec::new();

    match host.default_input_device() {
        Some(device) => match build_input_stream(&device, command_tx, transmitting, events.clone()) {
            Ok(stream) => if let Err(error) = stream.play() {
                let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("could not start microphone: {error}")));
            } else { streams.push(stream); },
            Err(error) => { let _ = events.try_send(VoiceEvent::AudioUnavailable(format!("microphone unavailable: {error}"))); }
        },
        None => { let _ = events.try_send(VoiceEvent::AudioUnavailable("no default microphone device".into())); }
    }

    match host.default_output_device() {
        Some(device) => match build_output_stream(&device, audio_rx, events.clone()) {
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
}
impl InputAssembler {
    fn new(input_rate: u32, tx: SyncSender<Command>, transmitting: Arc<AtomicBool>) -> Self {
        Self { input_rate: input_rate.max(1), phase: 0, previous: 0.0,
            has_previous: false, frame: [0; FRAME_SAMPLES], frame_pos: 0, tx, transmitting }
    }
    fn push(&mut self, sample: f32) {
        let sample = sample.clamp(-1.0, 1.0);
        if !self.has_previous { self.previous = sample; self.has_previous = true; return; }
        self.phase += SAMPLE_RATE as u64;
        while self.phase >= u64::from(self.input_rate) {
            self.phase -= u64::from(self.input_rate);
            let fraction = 1.0 - self.phase as f32 / SAMPLE_RATE as f32;
            let value = self.previous + (sample - self.previous) * fraction;
            self.frame[self.frame_pos] = (value * 32767.0) as i16;
            self.frame_pos += 1;
            if self.frame_pos == FRAME_SAMPLES {
                if self.transmitting.load(Ordering::Relaxed) {
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
            let mut input = InputAssembler::new(rate, tx, transmitting);
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
            let mut input = InputAssembler::new(rate, tx, transmitting);
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
            let mut input = InputAssembler::new(rate, tx, transmitting);
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

struct OutputAssembler {
    rx: Receiver<[i16; FRAME_SAMPLES]>,
    current: Option<[i16; FRAME_SAMPLES]>,
    position: f64,
    step: f64,
}
impl OutputAssembler {
    fn new(rx: Receiver<[i16; FRAME_SAMPLES]>, output_rate: u32) -> Self {
        Self { rx, current: None, position: 0.0,
            step: SAMPLE_RATE as f64 / f64::from(output_rate.max(1)) }
    }
    fn next_sample(&mut self) -> i16 {
        if self.current.is_none() || self.position >= FRAME_SAMPLES as f64 {
            self.current = self.rx.try_recv().ok();
            self.position = 0.0;
        }
        let Some(frame) = self.current.as_ref() else { return 0; };
        let index = self.position.floor() as usize;
        let next = (index + 1).min(FRAME_SAMPLES - 1);
        let fraction = (self.position - index as f64) as f32;
        let sample = frame[index] as f32 + (frame[next] as f32 - frame[index] as f32) * fraction;
        self.position += self.step;
        sample.clamp(i16::MIN as f32, i16::MAX as f32) as i16
    }
}

fn build_output_stream(
    device: &cpal::Device,
    rx: Receiver<[i16; FRAME_SAMPLES]>,
    events: SyncSender<VoiceEvent>,
) -> Result<cpal::Stream, String> {
    use cpal::traits::DeviceTrait;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config = supported.config();
    let channels = usize::from(config.channels).max(1);
    let rate = config.sample_rate.0;
    match format {
        cpal::SampleFormat::F32 => {
            let mut output = OutputAssembler::new(rx, rate);
            let errors = events;
            device.build_output_stream(&config, move |data: &mut [f32], _| {
                for frame in data.chunks_mut(channels) {
                    frame.fill(output.next_sample() as f32 / 32768.0);
                }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("speaker stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        cpal::SampleFormat::I16 => {
            let mut output = OutputAssembler::new(rx, rate);
            let errors = events;
            device.build_output_stream(&config, move |data: &mut [i16], _| {
                for frame in data.chunks_mut(channels) { frame.fill(output.next_sample()); }
            }, move |error| {
                let _ = errors.try_send(VoiceEvent::AudioUnavailable(format!("speaker stream error: {error}")));
            }, None).map_err(|e| e.to_string())
        }
        cpal::SampleFormat::U16 => {
            let mut output = OutputAssembler::new(rx, rate);
            let errors = events;
            device.build_output_stream(&config, move |data: &mut [u16], _| {
                for frame in data.chunks_mut(channels) {
                    frame.fill((i32::from(output.next_sample()) + 32768) as u16);
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
    if datagram.len() > info.mtu as usize {
        return Err(VoiceError::InvalidServerInfo("encrypted UDP packet exceeds negotiated MTU"));
    }
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
    fn resolves_host_with_and_without_port() {
        assert_eq!(resolve_voice_address("127.0.0.1:25565", 24454).unwrap(),
            "127.0.0.1:24454".parse::<SocketAddr>().unwrap());
        assert_eq!(resolve_voice_address("127.0.0.1", 24454).unwrap(),
            "127.0.0.1:24454".parse::<SocketAddr>().unwrap());
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
    fn rejects_varint_overflow() {
        let mut reader = Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x7f]);
        assert!(reader.varint().is_none());
    }
}
