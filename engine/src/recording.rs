//! Voice chat recording, in the spirit of upstream's `AudioRecorder`: one
//! file per speaker, written as 16-bit stereo WAV (upstream writes MP3 via
//! LAME; WAV keeps the engine dependency-free while keeping per-speaker
//! tracks). Chunks are timestamped and gaps between them are filled with
//! silence, exactly like upstream's chunk flush logic.

use std::collections::HashMap;
use std::io::{Seek, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Samples per millisecond of the recording format.
const SAMPLES_PER_MS: f64 = 48.0;
/// Chunks longer than this are flushed so one file never buffers more than
/// a minute of audio (upstream's 60-second chunk cap).
const MAX_CHUNK_MS: u64 = 60_000;

/// One speaker's WAV file under construction.
struct Track {
    file: std::fs::File,
    /// Samples (per channel) written so far, for the header's final sizes.
    samples: u64,
    /// The queued chunk's first sample's timestamp in milliseconds.
    chunk_timestamp: u64,
    chunk: Vec<i16>,
    chunk_end: u64,
    rate: u32,
}

impl Track {
    fn create(path: PathBuf, rate: u32) -> std::io::Result<Self> {
        let mut file = std::fs::File::create(&path)?;
        file.write_all(&wav_header(0, 0, rate))?;
        Ok(Self {
            file,
            samples: 0,
            chunk_timestamp: 0,
            chunk: Vec::new(),
            chunk_end: 0,
            rate,
        })
    }

    /// Writes the queued chunk preceded by the silence the gap needs, as
    /// upstream `writeChunk` does.
    fn flush_chunk(&mut self, timestamp: u64) -> std::io::Result<()> {
        if self.chunk.is_empty() {
            self.chunk_timestamp = timestamp;
            self.chunk_end = timestamp;
            return Ok(());
        }
        let mut relative = timestamp as i64 - self.chunk_end as i64;
        if relative < -100 {
            // The chunk overlaps the previous one by too much: drop it, as
            // upstream logs and skips.
            self.chunk.clear();
            self.chunk_timestamp = timestamp;
            self.chunk_end = timestamp;
            return Ok(());
        }
        if relative < 0 {
            relative = 0;
        }
        let mut silence = (relative as f64 * SAMPLES_PER_MS) as usize * 2;
        // Insert silence in 10-second blocks so a long gap cannot allocate
        // a huge buffer at once.
        let ten_seconds = self.rate as usize * 2 * 10;
        while silence > ten_seconds {
            self.write_pcm(&vec![0i16; ten_seconds])?;
            silence -= ten_seconds;
        }
        if silence > 0 {
            self.write_pcm(&vec![0i16; silence])?;
        }
        let chunk = std::mem::take(&mut self.chunk);
        self.write_pcm(&chunk)?;
        self.chunk_end = timestamp + audio_ms(chunk.len(), self.rate);
        self.chunk_timestamp = self.chunk_end;
        Ok(())
    }

    fn write_pcm(&mut self, pcm: &[i16]) -> std::io::Result<()> {
        if pcm.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.samples += pcm.len() as u64 / 2;
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        // Write the still-buffered chunk before the final header.
        self.flush_chunk(self.chunk_end)?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        let data_bytes = self.samples * 4;
        self.file.write_all(&wav_header(self.samples, data_bytes, self.rate))?;
        self.file.flush()
    }
}

/// A 44-byte PCM WAV header. `samples` counts stereo sample frames.
fn wav_header(samples: u64, data_bytes: u64, rate: u32) -> [u8; 44] {
    let byte_rate = rate * 4;
    // The RIFF sizes are 32-bit; clamp at the WAV format's own limit.
    let riff_size = (36u64.saturating_add(data_bytes)).min(u32::MAX as u64) as u32;
    let data_size = data_bytes.min(u32::MAX as u64) as u32;
    let mut header = [0u8; 44];
    header[..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&2u16.to_le_bytes());
    header[24..28].copy_from_slice(&rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&4u16.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());
    let _ = samples;
    header
}

fn audio_ms(samples: usize, rate: u32) -> u64 {
    (samples as f64 / 2.0 / rate as f64 * 1000.0) as u64
}

/// Names the recordings directory for one session, like upstream's
/// `yyyy-MM-dd-HH-mm-ss-SSS` folder.
pub fn session_dir_name(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let millis = now
        .duration_since(UNIX_EPOCH)
        .map(|value| value.subsec_millis())
        .unwrap_or(0);
    // A compact UTC timestamp keeps folder names sortable without pulling
    // in a date library.
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}-{h:02}-{m:02}-{s:02}-{millis:03}",
        h = rest / 3600,
        m = rest % 3600 / 60,
        s = rest % 60,
    )
}

/// Howard Hinnant's `civil_from_days` for the timestamp folders.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The shared recorder the audio output thread feeds. File IO happens on the
/// caller of `append`/`flush` — the engine hands chunks to this sink from a
/// helper thread (see `Recorder`), never from the audio callback.
pub struct RecorderSink {
    inner: Mutex<Inner>,
    start_ms: u64,
    active: AtomicU64,
}

struct Inner {
    dir: PathBuf,
    rate: u32,
    tracks: HashMap<[u8; 16], Track>,
    names: Arc<std::sync::RwLock<HashMap<[u8; 16], String>>>,
}

impl RecorderSink {
    /// Starts a session under `dir`, named like upstream's timestamp folder.
    pub fn start(dir: PathBuf, names: Arc<std::sync::RwLock<HashMap<[u8; 16], String>>>, rate: u32) -> std::io::Result<Arc<Self>> {
        let session = dir.join(session_dir_name(SystemTime::now()));
        std::fs::create_dir_all(&session)?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                dir: session,
                rate,
                tracks: HashMap::new(),
                names,
            }),
            start_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|value| value.as_millis() as u64)
                .unwrap_or(0),
            active: AtomicU64::new(1),
        }))
    }

    /// Marks the recorder finished; queued chunks are written by `flush`.
    pub fn stop(&self) {
        self.active.store(0, Ordering::Release);
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire) != 0
    }

    /// Milliseconds since the recording started, for the HUD.
    pub fn elapsed_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0)
            .saturating_sub(self.start_ms)
    }

    /// The session folder, for chat feedback.
    pub fn location(&self) -> PathBuf {
        self.inner.lock().map(|inner| inner.dir.clone()).unwrap_or_default()
    }

    pub fn track_count(&self) -> usize {
        self.inner.lock().map(|inner| inner.tracks.len()).unwrap_or(0)
    }

    /// Appends stereo samples for one speaker at `timestamp` (ms since the
    /// UNIX epoch, matching the chunk timestamps upstream uses). Only called
    /// from the recorder thread, which stops at the `Stop` message.
    pub(crate) fn append(&self, speaker: [u8; 16], timestamp: u64, stereo: &[i16]) {
        if stereo.is_empty() {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else { return; };
        let rate = inner.rate;
        let track = match inner.tracks.get_mut(&speaker) {
            Some(track) => track,
            None => {
                let name = inner
                    .names
                    .read()
                    .ok()
                    .and_then(|names| names.get(&speaker).cloned())
                    .unwrap_or_else(|| format!("player-{}", uuid_string(&speaker)));
                let file_name = sanitize(&name);
                let path = inner.dir.join(format!("{file_name}.wav"));
                match Track::create(path, rate) {
                    Ok(track) => {
                        inner.tracks.insert(speaker, track);
                        inner.tracks.get_mut(&speaker).unwrap()
                    }
                    Err(_) => return,
                }
            }
        };
        let passed_time = timestamp as i64 - track.chunk_end as i64;
        let threshold = 40 * 20; // outputBufferSize (40 packets) * 20 ms, upstream's default
        if (passed_time < threshold && track.chunk_end - track.chunk_timestamp < MAX_CHUNK_MS)
            || track.chunk.is_empty()
        {
            if track.chunk.is_empty() {
                track.chunk_timestamp = timestamp;
            }
            track.chunk.extend_from_slice(stereo);
            track.chunk_end = timestamp + audio_ms(stereo.len(), rate);
        } else {
            let _ = track.flush_chunk(timestamp);
            track.chunk_timestamp = timestamp;
            track.chunk.extend_from_slice(stereo);
            track.chunk_end = timestamp + audio_ms(stereo.len(), rate);
        }
    }

    /// Ends a speaker's chunk (a zero-length frame upstream), writing its
    /// audio so gaps stay aligned.
    pub fn flush(&self, speaker: [u8; 16]) {
        let Ok(mut inner) = self.inner.lock() else { return; };
        if let Some(track) = inner.tracks.get_mut(&speaker) {
            let timestamp = track.chunk_end;
            let _ = track.flush_chunk(timestamp);
        }
    }

    /// Finishes every file (writes the final WAV headers).
    pub fn finish(&self) {
        let Ok(mut inner) = self.inner.lock() else { return; };
        for (_, track) in inner.tracks.drain() {
            let _ = track.finish();
        }
    }
}

/// The engine's recorder handle: a background thread drains stereo chunks
/// the output stream captures, so the audio callback never touches a file.
pub struct Recorder {
    sink: Arc<RecorderSink>,
    sender: std::sync::mpsc::Sender<RecordingMessage>,
    thread: Option<std::thread::JoinHandle<()>>,
}

enum RecordingMessage {
    Chunk([u8; 16], u64, Vec<i16>),
    Flush([u8; 16]),
    Stop,
}

impl Recorder {
    /// Starts a recording session; `names` maps speaker UUIDs to the file
    /// base names (the mod's player-state cache feeds it).
    pub fn start(
        dir: PathBuf,
        names: Arc<std::sync::RwLock<HashMap<[u8; 16], String>>>,
        rate: u32,
    ) -> Option<Self> {
        let sink = RecorderSink::start(dir, names, rate).ok()?;
        let (sender, receiver) = std::sync::mpsc::channel::<RecordingMessage>();
        let thread_sink = sink.clone();
        let thread = std::thread::Builder::new()
            .name("svchorizon-recorder".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        RecordingMessage::Chunk(speaker, timestamp, pcm) => {
                            thread_sink.append(speaker, timestamp, &pcm)
                        }
                        RecordingMessage::Flush(speaker) => thread_sink.flush(speaker),
                        RecordingMessage::Stop => break,
                    }
                }
                thread_sink.finish();
            })
            .ok()?;
        Some(Self {
            sink,
            sender,
            thread: Some(thread),
        })
    }

    pub fn sink(&self) -> &Arc<RecorderSink> {
        &self.sink
    }

    /// Captures one stereo chunk for a speaker (never blocks the caller for
    /// file IO; drops the chunk when the writer thread is gone).
    pub fn capture(&self, speaker: [u8; 16], stereo: &[i16]) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or(0);
        let _ = self
            .sender
            .send(RecordingMessage::Chunk(speaker, timestamp, stereo.to_vec()));
    }

    /// Ends this recording: pending audio is written, headers finalized.
    pub fn stop_and_finish(mut self) -> PathBuf {
        self.sink.stop();
        let location = self.sink.location();
        let _ = self.sender.send(RecordingMessage::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        location
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.sender.send(RecordingMessage::Stop);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

/// Upstream replaces every character outside `[a-zA-Z0-9-_ ]`.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' {
            c
        } else {
            '_'
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "unknown".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn uuid_string(uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "svchorizon-recording-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn sanitizes_recording_file_names_like_upstream() {
        assert_eq!(sanitize("Player_123"), "Player_123");
        assert_eq!(sanitize("bad/name:of*it"), "bad_name_of_it");
        // Upstream replaces every excluded character; a name of only
        // excluded characters becomes only underscores.
        assert_eq!(sanitize("###"), "___");
        assert_eq!(sanitize("   "), "unknown");
    }

    #[test]
    fn writes_a_valid_wav_per_speaker_with_gap_silence() {
        let dir = temp_dir("wav");
        let names = Arc::new(std::sync::RwLock::new(HashMap::new()));
        names.write().unwrap().insert([1; 16], "Tester".into());
        let recorder = Recorder::start(dir.clone(), names, 48_000).unwrap();
        // One 20 ms stereo frame, then a gap beyond the 800 ms chunk
        // threshold, then another frame: the gap becomes silence.
        let frame = vec![100i16; 960 * 2];
        recorder.capture([1; 16], &frame);
        std::thread::sleep(std::time::Duration::from_millis(900));
        recorder.capture([1; 16], &frame);
        let location = recorder.stop_and_finish();
        let file = location.join("Tester.wav");
        let bytes = std::fs::read(&file).expect("recording exists");
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let data_bytes = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        // 20 ms + the ~900 ms gap of silence + 20 ms of audio, stereo 16-bit
        // at 48 kHz: 192 bytes per millisecond.
        let low = (900 + 15) * 192;
        let high = (900 + 90) * 192;
        assert!(
            (low..=high).contains(&data_bytes),
            "data_bytes {data_bytes} vs [{low}, {high}]"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_dirs_sort_chronologically() {
        let earlier = session_dir_name(UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 19_000));
        let later = session_dir_name(UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 19_001));
        assert!(earlier < later);
        assert_eq!(earlier.len(), "2026-11-02-12-34-56-789".len());
    }
}
