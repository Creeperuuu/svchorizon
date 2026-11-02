use std::{sync::{Mutex, OnceLock, atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering}, RwLock, Arc}, collections::HashMap, path::PathBuf, thread, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use svchorizon_engine::recording::Recorder;
use svchorizon_engine::server_host;
use svchorizon_engine::voice::{VoiceClient, VoiceEvent, VoiceServerInfo};

// Compatibility number of the shipped Simple Voice Chat bukkit plugin for
// MC 26.3 (2.6.27). Verified against the plugin jar: it logs "Compatibility
// version 20" and rejects every other value without sending a secret.
const SVC_COMPATIBILITY_VERSION: i32 = 20;
const REQUEST_SECRET: &str = "voicechat:request_secret";
const SECRET: &str = "voicechat:secret";
const REGISTER: &str = "minecraft:register";
/// Every custom-payload channel the official plugin registers (NetManager.init).
/// Bukkit/Paper deliver a payload only for channels the client registered, so
/// this must go out before the secret request on every server join.
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

fn register_channels_payload() -> Vec<u8> {
    let mut data = Vec::new();
    for (index, channel) in REGISTERED_CHANNELS.iter().enumerate() {
        if index > 0 { data.push(0); }
        data.extend_from_slice(channel.as_bytes());
    }
    data
}

// ---------------------------------------------------------------------------
// Recording (upstream's AudioRecorder: one file per speaker, silence in the
// gaps). Toggle with the recording keybind; the secret's `recording` flag
// gates it exactly like the official client's record button.
// ---------------------------------------------------------------------------

static RECORDER: OnceLock<Mutex<Option<Arc<Recorder>>>> = OnceLock::new();
static RECORDER_NAMES: OnceLock<Arc<RwLock<HashMap<[u8; 16], String>>>> = OnceLock::new();
static RECORDING_ALLOWED: AtomicBool = AtomicBool::new(false);

fn recorder_names() -> Arc<RwLock<HashMap<[u8; 16], String>>> {
    RECORDER_NAMES
        .get_or_init(|| Arc::new(RwLock::new(HashMap::new())))
        .clone()
}

fn recordings_root(game: &crate::game::Game<'_>) -> PathBuf {
    game.native_data_dir("svchorizon").join("recordings")
}

fn recording_started_at() -> Option<SystemTime> {
    RECORDER.get()?.lock().ok()?.as_ref().map(|_| SystemTime::now())
}

fn recorder_active() -> bool {
    RECORDER.get().and_then(|slot| slot.lock().ok())
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

fn recording_elapsed_text() -> String {
    let Some(start) = recording_started_at() else { return "0:00".into() };
    let elapsed = start.elapsed().map(|value| value.as_secs()).unwrap_or(0);
    format!("{}:{:02}", elapsed / 60, elapsed % 60)
}

fn toggle_recording(game: &mut crate::game::Game<'_>) {
    let slot = RECORDER.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().expect("SVC recorder poisoned");
    if let Some(recorder) = guard.take() {
        let location = recorder.finish_detached();
        if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
            client.set_recorder(None);
        }
        game.native_chat(&format!("SVC: Recording saved to {}", location.display()));
        return;
    }
    if !RECORDING_ALLOWED.load(Ordering::Acquire) {
        game.native_chat("SVC: This server does not allow recording.");
        return;
    }
    match Recorder::start(recordings_root(game), recorder_names(), 48_000) {
        Ok(recorder) => {
            let recorder = Arc::new(recorder);
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_recorder(Some(Arc::clone(&recorder)));
            }
            game.native_chat("SVC: Recording started.");
            *guard = Some(recorder);
        }
        Err(error) => game.native_chat(&format!("SVC: Could not start recording: {error}")),
    }
}

/// Keeps the recorder's speaker name map current (per-player state payloads).
fn remember_recording_name(id: [u8; 16], name: &str) {
    if let Ok(mut names) = recorder_names().write() {
        names.insert(id, name.to_owned());
    }
}

// ---------------------------------------------------------------------------
// The voice chat menu overlay: an SVC VoiceChatScreen-style panel drawn in
// the HUD hook and driven with the keyboard (Horizon native mods cannot open
// custom screens). Rows are selected with Up/Down, toggled/entered with
// Enter, values adjusted with Left/Right, and the menu closes with Escape.
// ---------------------------------------------------------------------------

static MENU_OPEN: AtomicBool = AtomicBool::new(false);
static MENU_PAGE: AtomicUsize = AtomicUsize::new(0); // 0 main, 1 groups, 2 players, 3 mic devices, 4 speaker devices
static MENU_SELECTED: AtomicUsize = AtomicUsize::new(0);
/// Main page: which column the selection sits in (0 icon strip, 1 rows).
static MENU_FOCUS: AtomicUsize = AtomicUsize::new(1);
/// Main page: which icon button of the vertical strip is selected.
static MENU_ICON: AtomicUsize = AtomicUsize::new(0);
static MENU_KEY_DOWN: AtomicBool = AtomicBool::new(false);
/// Master speaker mute (the official client's "disable voice chat").
static SPEAKERS_DISABLED: AtomicBool = AtomicBool::new(false);
/// The official client's "hide icons" toggle: draw no voice chat HUD at all.
static HIDE_ICONS: AtomicBool = AtomicBool::new(false);

const MENU_MAIN: usize = 0;
const MENU_GROUPS: usize = 1;
const MENU_PLAYERS: usize = 2;
const MENU_MIC_DEVICES: usize = 3;
const MENU_SPK_DEVICES: usize = 4;
/// The main page's vertical icon strip: microphone, hide icons, group,
/// recording - the official VoiceChatScreen's left column.
const MENU_ICON_COUNT: usize = 4;

fn menu_open() -> bool {
    MENU_OPEN.load(Ordering::Relaxed)
}

fn persist_setting(game: &crate::game::Game<'_>, key: &str, value: &str) {
    game.native_set_setting("svchorizon", key, value);
}

fn read_setting(game: &crate::game::Game<'_>, key: &str) -> Option<String> {
    game.native_setting("svchorizon", key)
}

/// Client settings persisted across sessions (Options > Mods), read on the
/// game thread and applied to every fresh VoiceClient.
static PERSISTED_NOISE_SUPPRESSION: AtomicBool = AtomicBool::new(true);
static PERSISTED_MIC_GAIN: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
static PERSISTED_SPEAKER_GAIN: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
static PERSISTED_ACTIVATION_THRESHOLD: AtomicU32 = AtomicU32::new(0.025f32.to_bits());

/// Reads the persisted settings (game thread; native_setting reads files).
fn load_persisted_settings(game: &crate::game::Game<'_>) {
    PERSISTED_NOISE_SUPPRESSION.store(
        !read_setting(game, "noise_suppression").map(|value| value == "off").unwrap_or(false),
        Ordering::Relaxed,
    );
    if let Some(gain) = read_setting(game, "microphone_gain").and_then(|value| value.parse::<f32>().ok()) {
        PERSISTED_MIC_GAIN.store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }
    if let Some(gain) = read_setting(game, "speaker_gain").and_then(|value| value.parse::<f32>().ok()) {
        PERSISTED_SPEAKER_GAIN.store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Relaxed);
    }
    if let Some(threshold) = read_setting(game, "activation_threshold").and_then(|value| value.parse::<f32>().ok()) {
        PERSISTED_ACTIVATION_THRESHOLD.store(threshold.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    HIDE_ICONS.store(read_setting(game, "hide_icons").map(|value| value == "on").unwrap_or(false), Ordering::Relaxed);
}

/// Applies the persisted client settings to a fresh VoiceClient (any thread).
fn apply_client_settings(client: &VoiceClient) {
    client.set_noise_suppression(PERSISTED_NOISE_SUPPRESSION.load(Ordering::Relaxed));
    client.set_microphone_gain(f32::from_bits(PERSISTED_MIC_GAIN.load(Ordering::Relaxed)));
    client.set_speaker_gain(f32::from_bits(PERSISTED_SPEAKER_GAIN.load(Ordering::Relaxed)));
    let enabled = VOICE_ACTIVATION.load(Ordering::Relaxed);
    client.set_voice_activation(
        enabled && !SELF_MUTED.load(Ordering::Relaxed),
        f32::from_bits(PERSISTED_ACTIVATION_THRESHOLD.load(Ordering::Relaxed)),
    );
    client.set_disabled(SPEAKERS_DISABLED.load(Ordering::Relaxed));
}

// ---------------------------------------------------------------------------
// HUD icons: the official client's 16x16 icon art, palette-encoded
// (generated by scripts/generate_icons.py).
// ---------------------------------------------------------------------------

mod icons {
    pub(crate) struct Icon {
        pub(crate) size: u32,
        pub(crate) palette: &'static [u32],
        pub(crate) indices: &'static [u8],
    }

    /// Draws an icon at GUI-pixel `x, y`, one fill quad per opaque pixel.
    pub(crate) fn draw(
        game: &crate::game::Game<'_>,
        batch: &mut horizon_client::hud::HudBatch,
        icon: &Icon,
        x: f32,
        y: f32,
    ) {
        let size = icon.size as f32;
        for (index, &palette_index) in icon.indices.iter().enumerate() {
            if palette_index == 0 {
                continue;
            }
            let argb = match icon.palette.get(palette_index as usize) {
                Some(&color) if color != 0 => color,
                _ => continue,
            };
            let px = x + (index as u32 % icon.size) as f32;
            let py = y + (index as u32 / icon.size) as f32;
            game.native_fill(batch, [px, py, 1.0, 1.0], argb);
        }
        let _ = size;
    }

// ICONS-BEGIN (generated by scripts/generate_icons.py)
/// `textures/icons/microphone.png` (16x16), palette-encoded.
pub(crate) const MICROPHONE: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x424248FF, 0x515151FF,
        0x252528FF, 0x1E1A21FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x2, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x2, 0x3, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x2, 0x3, 0x2, 0x3, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x2, 0x3, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x2, 0x3, 0x2, 0x3, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x1, 0x1, 0x2, 0x3, 0x2, 0x3, 0x2, 0x2, 0x1, 0x1, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x4, 0x1, 0x3, 0x2, 0x3, 0x2, 0x3, 0x2, 0x1, 0x4, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x4, 0x1, 0x2, 0x3, 0x2, 0x3, 0x2, 0x2, 0x1, 0x4, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x4, 0x4, 0x1, 0x2, 0x2, 0x2, 0x2, 0x1, 0x4, 0x5, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x5, 0x4, 0x1, 0x1, 0x1, 0x1, 0x4, 0x5, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x5, 0x4, 0x4, 0x4, 0x5, 0x5, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x4, 0x5, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x4, 0x5, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x4, 0x4, 0x4, 0x5, 0x5, 0x5, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/microphone_whisper.png` (16x16), palette-encoded.
pub(crate) const MICROPHONE_WHISPER: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x515151FF, 0x424248FF,
        0x252528FF, 0x1E1A21FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x2, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x2, 0x3, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x2, 0x3, 0x2, 0x3, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x4, 0x1, 0x3, 0x2, 0x3, 0x3, 0x1, 0x4, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x5, 0x4, 0x1, 0x3, 0x3, 0x1, 0x4, 0x5, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x5, 0x4, 0x1, 0x1, 0x4, 0x5, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x5, 0x5, 0x5, 0x5, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x4, 0x5, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x4, 0x4, 0x5, 0x5, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/microphone_off.png` (16x16), palette-encoded.
pub(crate) const MICROPHONE_OFF: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x424248FF, 0x515151FF,
        0xFF1313FF, 0xBB1327FF, 0x252528FF, 0x1E1A21FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x2, 0x3, 0x1, 0x0, 0x0, 0x4, 0x4, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x2, 0x3, 0x2, 0x2, 0x1, 0x4, 0x4, 0x5, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x2, 0x3, 0x2, 0x3, 0x2, 0x4, 0x4, 0x5, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x2, 0x3, 0x2, 0x4, 0x4, 0x5, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x2, 0x3, 0x2, 0x4, 0x4, 0x5, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x1, 0x1, 0x2, 0x3, 0x2, 0x4, 0x4, 0x5, 0x1, 0x1, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x6, 0x1, 0x3, 0x2, 0x4, 0x4, 0x5, 0x2, 0x1, 0x6, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x6, 0x1, 0x2, 0x4, 0x4, 0x5, 0x2, 0x2, 0x1, 0x6, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x6, 0x6, 0x4, 0x4, 0x5, 0x2, 0x2, 0x1, 0x6, 0x7, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x4, 0x4, 0x5, 0x1, 0x1, 0x1, 0x6, 0x7, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x4, 0x4, 0x5, 0x6, 0x6, 0x6, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x4, 0x4, 0x5, 0x1, 0x1, 0x6, 0x7, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x4, 0x4, 0x5, 0x0, 0x1, 0x1, 0x6, 0x7, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x4, 0x5, 0x0, 0x1, 0x6, 0x6, 0x6, 0x7, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/speaker.png` (16x16), palette-encoded.
pub(crate) const SPEAKER: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x515151FF, 0x424248FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x1, 0x1, 0x1, 0x1, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x2, 0x2, 0x1, 0x1, 0x2, 0x1, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x2, 0x2, 0x2, 0x2, 0x1, 0x1, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x3, 0x3, 0x2, 0x2, 0x2, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x3, 0x3, 0x2, 0x2, 0x1, 0x1, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x3, 0x2, 0x1, 0x1, 0x2, 0x1, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x1, 0x1, 0x1, 0x1, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/speaker_whisper.png` (16x16), palette-encoded.
pub(crate) const SPEAKER_WHISPER: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x515151FF, 0x424248FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x2, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x2, 0x2, 0x2, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x2, 0x2, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x3, 0x3, 0x2, 0x2, 0x2, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x3, 0x3, 0x2, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x3, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/speaker_off.png` (16x16), palette-encoded.
pub(crate) const SPEAKER_OFF: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x515151FF, 0xFF1313FF,
        0xBB1327FF, 0x424248FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x3, 0x3, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x1, 0x1, 0x1, 0x3, 0x3, 0x4, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x2, 0x2, 0x1, 0x1, 0x3, 0x3, 0x4, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x2, 0x2, 0x2, 0x2, 0x1, 0x3, 0x3, 0x4, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x3, 0x3, 0x4, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x2, 0x2, 0x3, 0x3, 0x4, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x5, 0x5, 0x2, 0x2, 0x3, 0x3, 0x4, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x5, 0x3, 0x3, 0x4, 0x1, 0x1, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x3, 0x3, 0x4, 0x2, 0x1, 0x1, 0x2, 0x1, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x3, 0x3, 0x4, 0x1, 0x5, 0x1, 0x1, 0x1, 0x1, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x3, 0x3, 0x4, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x3, 0x4, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/disconnected.png` (16x16), palette-encoded.
pub(crate) const DISCONNECTED: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0xB8B9C4FF, 0xFF1313FF,
        0xBB1327FF, 0x515151FF, 0x424248FF, 0x252528FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x0, 0x0, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x3, 0x3, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x1, 0x2, 0x1, 0x3, 0x3, 0x4, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x1, 0x2, 0x3, 0x3, 0x4, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x1, 0x1, 0x2, 0x1, 0x1, 0x1, 0x1, 0x3, 0x3, 0x4, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x5, 0x5, 0x5, 0x5, 0x5, 0x5, 0x3, 0x3, 0x4, 0x6, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x5, 0x5, 0x5, 0x5, 0x5, 0x3, 0x3, 0x4, 0x6, 0x5, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x6, 0x6, 0x6, 0x6, 0x3, 0x3, 0x4, 0x6, 0x6, 0x6, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x1, 0x5, 0x5, 0x3, 0x3, 0x4, 0x6, 0x5, 0x5, 0x1, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x6, 0x3, 0x3, 0x4, 0x6, 0x5, 0x5, 0x6, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x3, 0x3, 0x4, 0x6, 0x5, 0x5, 0x6, 0x1, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x3, 0x3, 0x4, 0x6, 0x6, 0x6, 0x6, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x3, 0x3, 0x4, 0x1, 0x1, 0x7, 0x7, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x3, 0x3, 0x4, 0x0, 0x0, 0x1, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x3, 0x4, 0x0, 0x0, 0x0, 0x1, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/group.png` (16x16), palette-encoded.
pub(crate) const GROUP: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x332411FF, 0x3F2A15FF,
        0x2B1E0DFF, 0x241808FF, 0x1E1E1EFF, 0x252525FF,
        0xBD8B72FF, 0xC69680FF, 0x141414FF, 0x523D89FF,
        0xB37B62FF, 0xCD843CFF, 0xD0873FFF, 0xCE863EFF,
        0x3A7855FF, 0x815339FF, 0xA26A47FF, 0x45220EFF,
        0xD27A30FF, 0xE3BE99FF, 0xE2BC97FF, 0xFBFBFBFF,
        0x1E641FFF, 0xDEBB96FF, 0xE2BC96FF, 0xDFBB95FF,
        0xD5AB81FF, 0xE0886FFF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x2, 0x3, 0x3, 0x3, 0x4, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x1, 0x1, 0x1, 0x1, 0x1, 0x5, 0x2, 0x3, 0x3, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x1, 0x6, 0x7, 0x7, 0x1, 0x2, 0x8, 0x9, 0x8, 0x8, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x1, 0xA, 0x6, 0x7, 0x1, 0x1, 0xB, 0x8, 0x8, 0xB, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1,
        0x1, 0x6, 0x3, 0x3, 0x1, 0xC, 0x8, 0x8, 0x8, 0x8, 0xC, 0x1, 0xD, 0xE, 0xF, 0x1,
        0x1, 0x1, 0x10, 0x3, 0x1, 0x11, 0x12, 0x13, 0x13, 0x12, 0x11, 0x1, 0x14, 0xF, 0x14, 0x1,
        0x1, 0x3, 0x3, 0x3, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x15, 0x16, 0xF, 0x1,
        0x1, 0x2, 0x3, 0x4, 0x4, 0x3, 0x2, 0x1, 0x1, 0x17, 0x18, 0x16, 0x19, 0x18, 0x17, 0x1,
        0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x16, 0x19, 0x16, 0x16, 0x1A, 0x1B, 0x1,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1C, 0x1A, 0x1D, 0x1D, 0x1A, 0x1C, 0x1,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/lock.png` (16x16), palette-encoded.
pub(crate) const LOCK: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0xFFFFFFFF, 0x515151FF, 0x424248FF,
        0x1E1A21FF, 0x252528FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x2, 0x2, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x1, 0x1, 0x1, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x1, 0x3, 0x1, 0x1, 0x1, 0x1, 0x3, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x4, 0x5, 0x2, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x2, 0x2, 0x2, 0x4, 0x5, 0x2, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x3, 0x2, 0x2, 0x5, 0x5, 0x2, 0x2, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x3, 0x3, 0x3, 0x3, 0x3, 0x3, 0x3, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/adjust_volumes.png` (16x16), palette-encoded.
pub(crate) const ADJUST_VOLUMES: Icon = Icon {
    size: 16,
    palette: &[
        0x00000000, 0x000000FF, 0x252528FF, 0x1E1A21FF,
        0x424248FF, 0x515151FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x2, 0x2, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0,
        0x0, 0x1, 0x4, 0x4, 0x2, 0x2, 0x4, 0x4, 0x5, 0x5, 0x5, 0x5, 0x5, 0x4, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x2, 0x3, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x3, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x2, 0x3, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x2, 0x2, 0x1, 0x1, 0x1, 0x0,
        0x0, 0x1, 0x4, 0x4, 0x5, 0x5, 0x5, 0x5, 0x5, 0x4, 0x2, 0x2, 0x4, 0x4, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x2, 0x3, 0x1, 0x1, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x3, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/record_button.png` (32x32), palette-encoded.
pub(crate) const RECORD_BUTTON: Icon = Icon {
    size: 32,
    palette: &[
        0x00000000, 0x000000FF, 0x515151FF, 0x2C2C35FF,
        0xFF1313FF, 0xD8131DFF, 0x35353FFF, 0x424248FF,
        0xFF9696FF, 0xFF5656FF, 0xBB1327FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x2, 0x2, 0x2, 0x2, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x2, 0x2, 0x2, 0x2, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x2, 0x2, 0x3, 0x3, 0x3, 0x3, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x2, 0x2, 0x4, 0x4, 0x5, 0x5, 0x2, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x3, 0x3, 0x6, 0x6, 0x6, 0x6, 0x3, 0x3, 0x7, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x4, 0x4, 0x8, 0x9, 0x4, 0x5, 0xA, 0xA, 0x7, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x3, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x4, 0x8, 0x9, 0x4, 0x4, 0x4, 0x5, 0xA, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x1, 0x2, 0x3, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x3, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x4, 0x8, 0x9, 0x4, 0x4, 0x4, 0x5, 0x4, 0x5, 0xA, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x3, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x4, 0x9, 0x4, 0x4, 0x4, 0x5, 0x4, 0x5, 0xA, 0xA, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x3, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x7, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x5, 0x4, 0x4, 0x4, 0x5, 0x4, 0x5, 0xA, 0xA, 0xA, 0x7, 0x1, 0x0,
        0x0, 0x1, 0x2, 0x3, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x7, 0x1, 0x0,
        0x0, 0x1, 0x2, 0xA, 0x5, 0x4, 0x5, 0x4, 0x5, 0xA, 0x5, 0xA, 0xA, 0x7, 0x1, 0x0,
        0x0, 0x0, 0x1, 0x2, 0x3, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x7, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x7, 0xA, 0x5, 0x4, 0x5, 0xA, 0x5, 0xA, 0xA, 0x7, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x7, 0x3, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x6, 0x7, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x7, 0xA, 0xA, 0xA, 0xA, 0xA, 0xA, 0xA, 0xA, 0x7, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x7, 0x7, 0x3, 0x6, 0x6, 0x6, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x7, 0x7, 0xA, 0xA, 0xA, 0xA, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x7, 0x7, 0x7, 0x7, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x7, 0x7, 0x7, 0x7, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/speaker_button.png` (32x32), palette-encoded.
pub(crate) const SPEAKER_BUTTON: Icon = Icon {
    size: 32,
    palette: &[
        0x00000000, 0x000000FF, 0xFFFFFFFF, 0xFF1313FF,
        0x515151FF, 0xBB1327FF, 0x424248FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x3, 0x3, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x4, 0x1, 0x1, 0x1, 0x1, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x4, 0x1, 0x1, 0x1, 0x3, 0x3, 0x5, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x4, 0x4, 0x1, 0x1, 0x2, 0x1, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x4, 0x4, 0x1, 0x1, 0x3, 0x3, 0x5, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x4, 0x4, 0x4, 0x4, 0x1, 0x1, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x4, 0x4, 0x4, 0x4, 0x1, 0x3, 0x3, 0x5, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0x1, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0x3, 0x3, 0x5, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0x1, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x4, 0x4, 0x4, 0x4, 0x4, 0x3, 0x3, 0x5, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x6, 0x6, 0x4, 0x4, 0x4, 0x4, 0x1, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x6, 0x6, 0x4, 0x4, 0x3, 0x3, 0x5, 0x2, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x6, 0x6, 0x4, 0x4, 0x1, 0x1, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x1, 0x1, 0x1, 0x6, 0x3, 0x3, 0x5, 0x1, 0x1, 0x1, 0x2, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x6, 0x4, 0x1, 0x1, 0x2, 0x1, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x3, 0x3, 0x5, 0x4, 0x1, 0x1, 0x2, 0x1, 0x1, 0x2, 0x1, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x6, 0x1, 0x1, 0x1, 0x1, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x3, 0x3, 0x5, 0x1, 0x6, 0x1, 0x1, 0x1, 0x1, 0x2, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x3, 0x3, 0x5, 0x0, 0x0, 0x1, 0x1, 0x0, 0x1, 0x2, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x3, 0x5, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/microphone_button.png` (32x32), palette-encoded.
pub(crate) const MICROPHONE_BUTTON: Icon = Icon {
    size: 32,
    palette: &[
        0x00000000, 0x000000FF, 0xFF1313FF, 0x515151FF,
        0x424248FF, 0xBB1327FF, 0x252528FF, 0x1E1A21FF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x2, 0x2, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x4, 0x3, 0x4, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x4, 0x3, 0x4, 0x1, 0x2, 0x2, 0x5, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x4, 0x3, 0x4, 0x3, 0x4, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x4, 0x3, 0x4, 0x3, 0x2, 0x2, 0x5, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x4, 0x3, 0x4, 0x3, 0x4, 0x3, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x4, 0x3, 0x4, 0x3, 0x2, 0x2, 0x5, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x4, 0x3, 0x4, 0x3, 0x4, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x3, 0x4, 0x3, 0x2, 0x2, 0x5, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x1, 0x4, 0x3, 0x4, 0x3, 0x4, 0x3, 0x1, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x1, 0x4, 0x3, 0x2, 0x2, 0x5, 0x3, 0x1, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x6, 0x3, 0x4, 0x3, 0x4, 0x3, 0x4, 0x6, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x6, 0x3, 0x2, 0x2, 0x5, 0x3, 0x4, 0x6, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x6, 0x6, 0x4, 0x4, 0x4, 0x4, 0x6, 0x7, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x6, 0x2, 0x2, 0x5, 0x4, 0x4, 0x6, 0x7, 0x1, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x1, 0x6, 0x6, 0x6, 0x6, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x2, 0x2, 0x5, 0x6, 0x6, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x6, 0x7, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x2, 0x2, 0x5, 0x1, 0x6, 0x7, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x6, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x2, 0x2, 0x5, 0x0, 0x1, 0x6, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x6, 0x6, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x2, 0x5, 0x0, 0x1, 0x6, 0x6, 0x7, 0x7, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x1, 0x1, 0x1, 0x1, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

/// `textures/icons/hide_button.png` (32x32), palette-encoded.
pub(crate) const HIDE_BUTTON: Icon = Icon {
    size: 32,
    palette: &[
        0x00000000, 0xFF1313FF, 0x000000FF, 0xBB1327FF,
        0xB8D8D8FF, 0xFFFFFFFF, 0x3452CEFF, 0x3474CEFF,
        0x2B3BC6FF, 0x80AFAFFF,
    ],
    indices: &[
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x1, 0x1, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x1, 0x1, 0x3, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x2, 0x2, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0x2, 0x2, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x2, 0x2, 0x4, 0x4, 0x4, 0x4, 0x4, 0x1, 0x1, 0x3, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x2, 0x4, 0x4, 0x5, 0x6, 0x7, 0x6, 0x6, 0x5, 0x4, 0x4, 0x2, 0x0, 0x0,
        0x0, 0x0, 0x2, 0x4, 0x4, 0x5, 0x6, 0x7, 0x6, 0x1, 0x1, 0x3, 0x4, 0x2, 0x0, 0x0,
        0x0, 0x2, 0x4, 0x5, 0x5, 0x6, 0x6, 0x2, 0x2, 0x7, 0x6, 0x5, 0x5, 0x4, 0x2, 0x0,
        0x0, 0x2, 0x4, 0x5, 0x5, 0x6, 0x6, 0x2, 0x1, 0x1, 0x3, 0x5, 0x5, 0x4, 0x2, 0x0,
        0x2, 0x4, 0x5, 0x5, 0x5, 0x8, 0x2, 0x2, 0x2, 0x2, 0x6, 0x5, 0x5, 0x5, 0x4, 0x2,
        0x2, 0x4, 0x5, 0x5, 0x5, 0x8, 0x2, 0x1, 0x1, 0x3, 0x6, 0x5, 0x5, 0x5, 0x4, 0x2,
        0x2, 0x9, 0x5, 0x5, 0x5, 0x6, 0x2, 0x2, 0x2, 0x2, 0x7, 0x5, 0x5, 0x5, 0x9, 0x2,
        0x2, 0x9, 0x5, 0x5, 0x5, 0x6, 0x1, 0x1, 0x3, 0x2, 0x7, 0x5, 0x5, 0x5, 0x9, 0x2,
        0x0, 0x2, 0x4, 0x4, 0x4, 0x8, 0x8, 0x2, 0x2, 0x7, 0x6, 0x5, 0x5, 0x4, 0x2, 0x0,
        0x0, 0x2, 0x4, 0x4, 0x4, 0x1, 0x1, 0x3, 0x2, 0x7, 0x6, 0x5, 0x5, 0x4, 0x2, 0x0,
        0x0, 0x0, 0x2, 0x4, 0x4, 0x4, 0x8, 0x8, 0x8, 0x8, 0x4, 0x4, 0x4, 0x2, 0x0, 0x0,
        0x0, 0x0, 0x2, 0x4, 0x1, 0x1, 0x3, 0x8, 0x8, 0x8, 0x4, 0x4, 0x4, 0x2, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x2, 0x2, 0x4, 0x4, 0x4, 0x4, 0x4, 0x4, 0x2, 0x2, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x1, 0x1, 0x3, 0x4, 0x4, 0x4, 0x4, 0x4, 0x2, 0x2, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x1, 0x3, 0x2, 0x2, 0x2, 0x2, 0x2, 0x2, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x1, 0x3, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
        0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0,
    ],
};

// ICONS-END
}

fn send_channel_registration(game: &mut crate::game::Game<'_>) {
    use horizon_protocol::packets::{configuration::CustomPayload, play::Serverbound};
    game.send(Serverbound::CustomPayload(CustomPayload {
        channel: REGISTER.to_owned(),
        data: register_channels_payload(),
    }));
}

static VOICE_CLIENT: OnceLock<Mutex<Option<VoiceClient>>> = OnceLock::new();
static PTT_DOWN: AtomicBool = AtomicBool::new(false);
static WHISPER_DOWN: AtomicBool = AtomicBool::new(false);
static VOICE_ACTIVATION: AtomicBool = AtomicBool::new(false);
static VOICE_ACTIVATION_KEY_DOWN: AtomicBool = AtomicBool::new(false);
static SELF_MUTED: AtomicBool = AtomicBool::new(false);
static SELF_MUTE_KEY_DOWN: AtomicBool = AtomicBool::new(false);
static REMOTE_ACTIVE: AtomicBool = AtomicBool::new(false);
static SESSION_GENERATION: AtomicU64 = AtomicU64::new(0);
static LOCAL_PLAYER_UUID: OnceLock<Mutex<Option<[u8; 16]>>> = OnceLock::new();
static LAST_SERVER_INFO: OnceLock<Mutex<Option<(VoiceServerInfo, String)>>> = OnceLock::new();
static RECONNECTING: AtomicBool = AtomicBool::new(false);
/// The first connect for the current secret is still starting. The server binds a
/// player's voice connection to the first UDP source address that authenticates
/// and sends every later reply there, so a second socket started for the same
/// secret (a reconnect racing the initial connect) is never answered - and can
/// poison the binding for the real client. Exactly one client per secret.
static CONNECTING: AtomicBool = AtomicBool::new(false);
static GROUPS_ENABLED: AtomicBool = AtomicBool::new(false);
static AUDIO_DEVICE_PREFS: OnceLock<Mutex<(Option<String>, Option<String>)>> = OnceLock::new();
/// Device lists are enumerated once when their menu page opens: audio host
/// queries are far too slow to run inside the HUD draw every frame.
static MIC_DEVICE_LIST: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static SPK_DEVICE_LIST: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn mic_device_list() -> &'static Mutex<Vec<String>> {
    MIC_DEVICE_LIST.get_or_init(|| Mutex::new(Vec::new()))
}

fn spk_device_list() -> &'static Mutex<Vec<String>> {
    SPK_DEVICE_LIST.get_or_init(|| Mutex::new(Vec::new()))
}

/// (Re)enumerates the device list a menu page shows.
fn refresh_device_list(page: usize) {
    let (store, output) = match page {
        MENU_MIC_DEVICES => (mic_device_list(), false),
        MENU_SPK_DEVICES => (spk_device_list(), true),
        _ => return,
    };
    let devices = if output {
        VoiceClient::output_devices()
    } else {
        VoiceClient::input_devices()
    };
    if let Ok(mut slot) = store.lock() {
        *slot = devices;
    }
}

/// The menu page that lists `page`'s devices (index 0 is "OS default").
fn device_page_rows(page: usize) -> Vec<(String, bool)> {
    let slot = match page {
        MENU_MIC_DEVICES => mic_device_list(),
        MENU_SPK_DEVICES => spk_device_list(),
        _ => return Vec::new(),
    };
    let selected = audio_device_preferences().lock().ok()
        .map(|preferences| if page == MENU_MIC_DEVICES { preferences.0.clone() } else { preferences.1.clone() })
        .unwrap_or_default();
    let mut rows = vec![("System default".to_owned(), selected.is_none())];
    if let Ok(devices) = slot.lock() {
        for device in devices.iter() {
            rows.push((device.clone(), Some(device) == selected.as_ref()));
        }
    }
    rows
}

fn audio_device_preferences() -> &'static Mutex<(Option<String>, Option<String>)> {
    AUDIO_DEVICE_PREFS.get_or_init(|| Mutex::new((None, None)))
}

fn apply_audio_device_preferences(client: &VoiceClient) {
    let preferences = audio_device_preferences().lock()
        .expect("SVC audio preferences poisoned").clone();
    client.set_input_device(preferences.0);
    client.set_output_device(preferences.1);
}

fn local_player_uuid() -> &'static Mutex<Option<[u8; 16]>> {
    LOCAL_PLAYER_UUID.get_or_init(|| Mutex::new(None))
}

fn last_server_info() -> &'static Mutex<Option<(VoiceServerInfo, String)>> {
    LAST_SERVER_INFO.get_or_init(|| Mutex::new(None))
}

fn apply_player_controls(client: &VoiceClient) {
    let controls = {
        let state = group_state().lock().expect("SVC player state poisoned");
        state.players.iter().map(|player| {
            (player.id, player.volume, player.ignored)
        }).collect::<Vec<_>>()
    };
    for (id, volume, ignored) in controls {
        client.set_player_volume(id, volume);
        client.set_player_muted(id, ignored);
    }
}

fn start_reconnect() {
    if RECONNECTING.swap(true, Ordering::AcqRel) { return; }
    let Some((info, fallback_host)) = last_server_info()
        .lock().expect("SVC reconnect state poisoned").clone()
    else {
        RECONNECTING.store(false, Ordering::Release);
        return;
    };
    let generation = SESSION_GENERATION.load(Ordering::Acquire);
    let spawned = thread::Builder::new().name("svchorizon-reconnect".into()).spawn(move || {
        let mut delay_seconds = 1u64;
        while REMOTE_ACTIVE.load(Ordering::Acquire)
            && SESSION_GENERATION.load(Ordering::Acquire) == generation
        {
            thread::sleep(std::time::Duration::from_secs(delay_seconds));
            if !REMOTE_ACTIVE.load(Ordering::Acquire)
                || SESSION_GENERATION.load(Ordering::Acquire) != generation
            {
                break;
            }
            // Never open a second UDP socket for a secret that already has (or is
            // about to have) a client: see CONNECTING.
            if voice_client().lock().expect("SVC client state poisoned").is_some() {
                break;
            }
            if CONNECTING.load(Ordering::Acquire) {
                continue;
            }
            match VoiceClient::connect(info.clone(), &fallback_host) {
                Ok(client) => {
                    apply_player_controls(&client);
                    apply_audio_device_preferences(&client);
                    apply_client_settings(&client);
                    if SESSION_GENERATION.load(Ordering::Acquire) == generation
                        && REMOTE_ACTIVE.load(Ordering::Acquire)
                    {
                        let mut slot = voice_client().lock().expect("SVC client state poisoned");
                        if slot.is_none() {
                            *slot = Some(client);
                            eprintln!("[svchorizon] Voice reconnect attempt started.");
                        }
                    }
                    break;
                }
                Err(error) => {
                    eprintln!("[svchorizon] Voice reconnect failed: {error}");
                    delay_seconds = (delay_seconds * 2).min(30);
                }
            }
        }
        if SESSION_GENERATION.load(Ordering::Acquire) == generation {
            RECONNECTING.store(false, Ordering::Release);
        }
    });
    if spawned.is_err() {
        RECONNECTING.store(false, Ordering::Release);
    }
}
static GROUP_CREATE_DOWN: AtomicBool = AtomicBool::new(false);
static GROUP_NEXT_DOWN: AtomicBool = AtomicBool::new(false);
static GROUP_JOIN_DOWN: AtomicBool = AtomicBool::new(false);
static GROUP_LEAVE_DOWN: AtomicBool = AtomicBool::new(false);
static PLAYER_NEXT_DOWN: AtomicBool = AtomicBool::new(false);
static PLAYER_IGNORE_DOWN: AtomicBool = AtomicBool::new(false);
static PLAYER_VOL_DOWN: AtomicBool = AtomicBool::new(false);
static PLAYER_VOL_UP: AtomicBool = AtomicBool::new(false);
static INPUT_DEVICE_NEXT_DOWN: AtomicBool = AtomicBool::new(false);
static OUTPUT_DEVICE_NEXT_DOWN: AtomicBool = AtomicBool::new(false);
static INPUT_DEVICE_INDEX: AtomicUsize = AtomicUsize::new(0);
static OUTPUT_DEVICE_INDEX: AtomicUsize = AtomicUsize::new(0);
static GROUP_STATE: OnceLock<Mutex<GroupState>> = OnceLock::new();

#[derive(Clone)]
struct NativeGroup {
    id: [u8; 16],
    name: String,
    has_password: bool,
    persistent: bool,
    hidden: bool,
    group_type: i16,
}

#[derive(Clone)]
struct NativePlayer {
    id: [u8; 16],
    name: String,
    server_muted: bool,
    force_muted: bool,
    disabled: bool,
    force_disabled: bool,
    disconnected: bool,
    group: Option<[u8; 16]>,
    volume: f32,
    ignored: bool,
}

#[derive(Clone)]
struct NativeCategory {
    id: String,
    name: String,
    description: Option<String>,
    tab: Option<String>,
    tab_name: Option<String>,
}

#[derive(Default)]
struct GroupState {
    groups: Vec<NativeGroup>,
    categories: Vec<NativeCategory>,
    selected: usize,
    active: Option<[u8; 16]>,
    wrong_password: bool,
    players: Vec<NativePlayer>,
    selected_player: usize,
}

fn group_state() -> &'static Mutex<GroupState> {
    GROUP_STATE.get_or_init(|| Mutex::new(GroupState::default()))
}

fn voice_client() -> &'static Mutex<Option<VoiceClient>> {
    VOICE_CLIENT.get_or_init(|| Mutex::new(None))
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self { Self { bytes, pos: 0 } }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> { Some(*self.take(1)?.first()?) }
    fn bool(&mut self) -> Option<bool> {
        match self.u8()? { 0 => Some(false), 1 => Some(true), _ => None }
    }
    fn optional_string(&mut self, max_chars: usize) -> Option<Option<String>> {
        if self.bool()? { Some(Some(self.string(max_chars)?)) } else { Some(None) }
    }
    fn i16(&mut self) -> Option<i16> { Some(i16::from_be_bytes(self.take(2)?.try_into().ok()?)) }
    fn i32(&mut self) -> Option<i32> { Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?)) }
    fn uuid(&mut self) -> Option<[u8; 16]> { self.take(16)?.try_into().ok() }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into().ok()?)))
    }
    fn varint(&mut self) -> Option<usize> {
        let mut out = 0usize;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 4 && b & 0xf0 != 0 { return None; }
            out |= usize::from(b & 0x7f).checked_shl(7 * i)?;
            if b & 0x80 == 0 { return Some(out); }
        }
        None
    }
    fn string(&mut self, max_chars: usize) -> Option<String> {
        let len = self.varint()?;
        if len > max_chars.checked_mul(4)? { return None; }
        let value = std::str::from_utf8(self.take(len)?).ok()?;
        if value.chars().count() > max_chars { return None; }
        Some(value.to_owned())
    }
    fn done(&self) -> bool { self.pos == self.bytes.len() }
}

fn decode_secret(bytes: &[u8]) -> Option<VoiceServerInfo> {
    let mut c = Cursor::new(bytes);
    let secret: [u8; 16] = c.take(16)?.try_into().ok()?;
    // SecretPacket.toBytes order for SVC 2.6.27 (compatibility 20): secret,
    // port, player, codec, mtu, distance, keepalive, groups, host, recording.
    // There is no server-assigned connection UUID in this wire generation: the
    // secret doubles as the UUID that keys every UDP datagram we send, so the
    // engine derives it from the secret bytes.
    let port = c.i32()?;
    let port = u16::try_from(port).ok().filter(|p| *p != 0)?;
    let player = c.take(16)?.try_into().ok()?;
    let codec = c.u8()?;
    let mtu = c.i32()?;
    let distance = c.f64()?;
    let keep_alive_ms = c.i32()?;
    let groups = c.bool()?;
    let host = c.string(32767)?;
    let allow_recording = c.bool()?;
    if !c.done() || mtu <= 0 || keep_alive_ms <= 0 || !distance.is_finite() || distance < 0.0 {
        return None;
    }
    Some(VoiceServerInfo {
        secret,
        port,
        player,
        codec,
        mtu,
        distance,
        keep_alive_ms,
        groups,
        host,
        allow_recording,
    })
}

fn client_state_payload(muted: bool, disabled: bool) -> Vec<u8> {
    vec![u8::from(muted), u8::from(disabled)]
}

fn send_client_state(game: &mut crate::game::Game<'_>, muted: bool, disabled: bool) {
    use horizon_protocol::packets::{configuration::CustomPayload, play::Serverbound};
    game.send(Serverbound::CustomPayload(CustomPayload {
        channel: "voicechat:client_state".to_owned(),
        data: client_state_payload(muted, disabled),
    }));
}

fn send_secret_request(game: &mut crate::game::Game<'_>) {
    use horizon_protocol::packets::{
        configuration::CustomPayload,
        play::Serverbound,
    };
    game.send(Serverbound::CustomPayload(CustomPayload {
        channel: REQUEST_SECRET.to_owned(),
        data: SVC_COMPATIBILITY_VERSION.to_be_bytes().to_vec(),
    }));
}

fn write_varint(out: &mut Vec<u8>, mut value: usize) {
    while value > 0x7f {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn write_utf(out: &mut Vec<u8>, value: &str) {
    write_varint(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn send_group_payload(game: &mut crate::game::Game<'_>, channel: &str, data: Vec<u8>) {
    use horizon_protocol::packets::{configuration::CustomPayload, play::Serverbound};
    game.send(Serverbound::CustomPayload(CustomPayload {
        channel: channel.to_owned(),
        data,
    }));
}

fn create_group_payload(name: &str, password: Option<&str>, group_type: i16) -> Vec<u8> {
    let mut data = Vec::new();
    write_utf(&mut data, name);
    data.push(u8::from(password.is_some()));
    if let Some(password) = password { write_utf(&mut data, password); }
    data.extend_from_slice(&group_type.to_be_bytes());
    data
}

fn join_group_payload(group_id: [u8; 16], password: Option<&str>) -> Vec<u8> {
    let mut data = group_id.to_vec();
    data.push(u8::from(password.is_some()));
    if let Some(password) = password { write_utf(&mut data, password); }
    data
}

fn send_create_group_custom(
    game: &mut crate::game::Game<'_>,
    name: &str,
    password: Option<&str>,
    group_type: i16,
) {
    send_group_payload(game, "voicechat:create_group", create_group_payload(name, password, group_type));
}

fn send_create_group(game: &mut crate::game::Game<'_>) {
    send_create_group_custom(game, "Horizon", None, 0);
}

fn send_join_group_password(
    game: &mut crate::game::Game<'_>,
    group: &NativeGroup,
    password: Option<&str>,
) {
    if group.has_password && password.is_none() {
        game.native_chat("SVC: This group requires a password. Use /svchorizon group join <password>.");
        return;
    }
    send_group_payload(game, "voicechat:set_group", join_group_payload(group.id, password));
}

fn send_join_group(game: &mut crate::game::Game<'_>, group: &NativeGroup) {
    if group.has_password {
        group_state().lock().expect("SVC group state poisoned").wrong_password = true;
        return;
    }
    let mut data = group.id.to_vec();
    data.push(0); // no password
    send_group_payload(game, "voicechat:set_group", data);
}

fn send_leave_group(game: &mut crate::game::Game<'_>) {
    send_group_payload(game, "voicechat:leave_group", Vec::new());
}

fn command_words(input: &str) -> Option<Vec<String>> {
    if input.len() > 1024 { return None; }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch.is_whitespace() && !quoted {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
                if words.len() > 8 { return None; }
            }
        } else {
            current.push(ch);
        }
        if current.chars().count() > 256 { return None; }
    }
    if escaped || quoted { return None; }
    if !current.is_empty() { words.push(current); }
    (words.len() <= 8).then_some(words)
}

pub fn on_command(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    if !game.native_on("svchorizon") { return; }
    let Some(words) = command_words(command) else { return; };
    if words.first().map(String::as_str) != Some("svchorizon") { return; }
    *ran = true;
    if !game.native_playing() || !GROUPS_ENABLED.load(Ordering::Acquire) {
        game.native_chat("SVC: Join a server with Simple Voice Chat enabled first.");
        return;
    }
    if words.get(1).map(String::as_str) != Some("group") {
        game.native_chat("Usage: /svchorizon group <list|join|create|leave>");
        return;
    }
    match words.get(2).map(String::as_str) {
        Some("list") => {
            let state = group_state().lock().expect("SVC group state poisoned");
            let names = state.groups.iter().filter(|g| !g.hidden)
                .map(|g| if g.has_password { format!("{} [locked]", g.name) } else { g.name.clone() })
                .collect::<Vec<_>>();
            let message = if names.is_empty() { "SVC: No visible groups.".to_owned() }
                else { format!("SVC groups: {}", names.join(", ")) };
            drop(state);
            game.native_chat(&message);
        }
        Some("join") => {
            let selected = {
                let state = group_state().lock().expect("SVC group state poisoned");
                state.groups.get(state.selected).filter(|g| !g.hidden).cloned()
            };
            match selected {
                Some(group) => {
                    let password = words.get(3).map(String::as_str);
                    if password.is_some_and(|p| p.chars().count() > 256) {
                        game.native_chat("SVC: Password must be 256 characters or fewer.");
                    } else {
                        send_join_group_password(game, &group, password);
                    }
                }
                None => game.native_chat("SVC: Select a visible group first with N."),
            }
        }
        Some("create") => {
            let Some(name) = words.get(3).filter(|name| !name.is_empty()) else {
                game.native_chat("Usage: /svchorizon group create <name> [password] [normal|open|isolated]");
                return;
            };
            let password = words.get(4).filter(|p| p.as_str() != "-").map(String::as_str);
            let kind = words.get(5).map(String::as_str).unwrap_or("normal");
            let group_type = match kind { "normal" => 0, "open" => 1, "isolated" => 2, _ => {
                game.native_chat("SVC: Group type must be normal, open, or isolated.");
                return;
            }};
            let invalid_name = name.chars().next().is_none_or(|c| c.is_control() || c.is_whitespace())
                || name.chars().any(|c| c.is_control());
            // Upstream's CreateGroupPacket writes both name and password with
            // MAX_GROUP_NAME_LENGTH (24); JoinGroupPacket allows 256. A create
            // password above 24 would abort the official client's write.
            if invalid_name || name.chars().count() > 24 || password.is_some_and(|p| p.chars().count() > 24) {
                game.native_chat("SVC: Group names must start with a visible character, contain no control characters, and be 24 characters or fewer; create passwords may be up to 24 characters (join passwords up to 256).");
                return;
            }
            send_create_group_custom(game, name, password, group_type);
            game.native_chat(&format!("SVC: Creating group {name}."));
        }
        Some("leave") => send_leave_group(game),
        _ => game.native_chat("Usage: /svchorizon group <list|join|create|leave>"),
    }
}

fn on_group_payload(channel: &str, bytes: &[u8]) {
    let mut state = group_state().lock().expect("SVC group state poisoned");
    let mut cursor = Cursor::new(bytes);
    match channel {
        "voicechat:joined_group" => {
            let group_id = if cursor.bool().unwrap_or(false) {
                cursor.uuid()
            } else {
                Some([0; 16])
            };
            let Some(wrong_password) = cursor.bool() else { return; };
            if !cursor.done() { return; }
            state.active = group_id.filter(|id| *id != [0; 16]);
            state.wrong_password = wrong_password;
        }
        "voicechat:add_group" => {
            let Some(id) = cursor.uuid() else { return; };
            let Some(name) = cursor.string(24) else { return; };
            let Some(has_password) = cursor.bool() else { return; };
            let Some(persistent) = cursor.bool() else { return; };
            let Some(hidden) = cursor.bool() else { return; };
            let Some(group_type) = cursor.i16() else { return; };
            if !cursor.done() { return; }
            let group = NativeGroup { id, name, has_password, persistent, hidden, group_type };
            if let Some(existing) = state.groups.iter_mut().find(|entry| entry.id == id) {
                *existing = group;
            } else if state.groups.len() < 128 {
                state.groups.push(group);
                if state.groups.len() == 1 { state.selected = 0; }
            }
            if !state.groups.is_empty() {
                state.selected %= state.groups.len();
            }
        }
        "voicechat:remove_group" => {
            let Some(id) = cursor.uuid() else { return; };
            if !cursor.done() { return; }
            state.groups.retain(|group| group.id != id);
            if state.groups.is_empty() {
                state.selected = 0;
            } else {
                state.selected %= state.groups.len();
            }
            if state.active == Some(id) { state.active = None; }
        }
        _ => {}
    }
}

fn decode_category(cursor: &mut Cursor<'_>) -> Option<NativeCategory> {
    let id = cursor.string(16)?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return None;
    }
    let name = cursor.string(16)?;
    let _name_translation_key = cursor.optional_string(256)?;
    let description = cursor.optional_string(256)?;
    let _description_translation_key = cursor.optional_string(256)?;
    if cursor.bool()? {
        // VolumeCategoryImpl transmits a fixed 16x16 ARGB icon.
        cursor.take(16 * 16 * 4)?;
    }
    let tab = cursor.optional_string(16)?;
    let tab_name = cursor.optional_string(16)?;
    let _tab_translation_key = cursor.optional_string(256)?;
    Some(NativeCategory { id, name, description, tab, tab_name })
}

fn on_category_payload(channel: &str, bytes: &[u8]) {
    let mut state = group_state().lock().expect("SVC category state poisoned");
    let mut cursor = Cursor::new(bytes);
    match channel {
        "voicechat:add_category" => {
            let Some(category) = decode_category(&mut cursor) else { return; };
            if !cursor.done() { return; }
            if let Some(existing) = state.categories.iter_mut().find(|entry| entry.id == category.id) {
                *existing = category;
            } else if state.categories.len() < 64 {
                state.categories.push(category);
            }
        }
        "voicechat:remove_category" => {
            let Some(id) = cursor.string(16) else { return; };
            if !cursor.done() { return; }
            state.categories.retain(|category| category.id != id);
        }
        _ => {}
    }
}

fn decode_player_state(cursor: &mut Cursor<'_>) -> Option<NativePlayer> {
    let flags = cursor.u8()?;
    let id = cursor.uuid()?;
    let name = cursor.string(256)?;
    let group = if flags & (1 << 5) != 0 { Some(cursor.uuid()?) } else { None };
    Some(NativePlayer {
        id,
        name,
        server_muted: flags & (1 << 0) != 0,
        force_muted: flags & (1 << 1) != 0,
        disabled: flags & (1 << 2) != 0,
        force_disabled: flags & (1 << 3) != 0,
        disconnected: flags & (1 << 4) != 0,
        group,
        volume: 1.0,
        ignored: false,
    })
}

/// Pulls (uuid, name) pairs out of a count-prefixed `states` payload without
/// failing on anything the strict decoder would reject later.
fn decode_player_names(bytes: &[u8]) -> Vec<([u8; 16], String)> {
    let mut out = Vec::new();
    let mut cursor = Cursor::new(bytes);
    let Some(count) = cursor.i32().and_then(|count| usize::try_from(count).ok()) else { return out };
    if count > 512 { return out; }
    for _ in 0..count {
        let Some((id, name)) = decode_one_player_name(&mut cursor) else { return out };
        out.push((id, name));
        if out.len() >= 128 { break; }
    }
    out
}

/// Pulls the single (uuid, name) pair of a `state` payload.
fn decode_single_player_name(bytes: &[u8]) -> Option<([u8; 16], String)> {
    let mut cursor = Cursor::new(bytes);
    decode_one_player_name(&mut cursor)
}

fn decode_one_player_name(cursor: &mut Cursor<'_>) -> Option<([u8; 16], String)> {
    let flags = cursor.u8()?;
    let id = cursor.uuid()?;
    let name = cursor.string(256)?;
    if flags & (1 << 5) != 0 {
        cursor.uuid()?;
    }
    Some((id, name))
}

fn preserve_player_controls(player: &mut NativePlayer, old: &[NativePlayer]) {
    if let Some(previous) = old.iter().find(|entry| entry.id == player.id) {
        player.volume = previous.volume;
        player.ignored = previous.ignored;
    }
}

fn on_player_payload(channel: &str, bytes: &[u8]) {
    let local_id = *local_player_uuid().lock().expect("SVC local player state poisoned");
    match channel {
        "voicechat:states" => {
            for (id, name) in decode_player_names(bytes) {
                remember_recording_name(id, &name);
            }
        }
        "voicechat:state" => {
            if let Some((id, name)) = decode_single_player_name(bytes) {
                remember_recording_name(id, &name);
            }
        }
        _ => {}
    }
    let mut state = group_state().lock().expect("SVC player state poisoned");
    let mut reset_players = Vec::new();
    match channel {
        "voicechat:states" => {
            let mut cursor = Cursor::new(bytes);
            let Some(count) = cursor.i32().and_then(|n| usize::try_from(n).ok()) else { return; };
            if count > 512 { return; }
            let old = state.players.clone();
            let mut players = Vec::with_capacity(count.min(128));
            for _ in 0..count {
                let Some(mut player) = decode_player_state(&mut cursor) else { return; };
                if Some(player.id) == local_id { continue; }
                preserve_player_controls(&mut player, &old);
                if players.len() < 128 && !players.iter().any(|entry: &NativePlayer| entry.id == player.id) {
                    players.push(player);
                }
            }
            if !cursor.done() { return; }
            reset_players = old.iter()
                .filter(|previous| !players.iter().any(|player| player.id == previous.id))
                .map(|previous| previous.id)
                .collect();
            state.players = players;
            if state.players.is_empty() { state.selected_player = 0; }
            else { state.selected_player %= state.players.len(); }
        }
        "voicechat:state" => {
            let mut cursor = Cursor::new(bytes);
            let Some(mut player) = decode_player_state(&mut cursor) else { return; };
            if !cursor.done() { return; }
            if Some(player.id) == local_id { return; }
            preserve_player_controls(&mut player, &state.players);
            if let Some(existing) = state.players.iter_mut().find(|entry| entry.id == player.id) {
                *existing = player;
            } else if state.players.len() < 128 {
                state.players.push(player);
            }
        }
        "voicechat:remove_state" => {
            let mut cursor = Cursor::new(bytes);
            let Some(id) = cursor.uuid() else { return; };
            if !cursor.done() { return; }
            state.players.retain(|player| player.id != id);
            if state.players.is_empty() { state.selected_player = 0; }
            else { state.selected_player %= state.players.len(); }
            drop(state);
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_player_volume(id, 1.0);
                client.set_player_muted(id, false);
            }
            return;
        }
        _ => return,
    }
    let controls = state.players.iter().map(|p| (p.id, p.volume, p.ignored)).collect::<Vec<_>>();
    drop(state);
    if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
        for id in reset_players {
            client.set_player_volume(id, 1.0);
            client.set_player_muted(id, false);
        }
        for (id, volume, ignored) in controls {
            client.set_player_volume(id, volume);
            client.set_player_muted(id, ignored);
        }
    }
}

fn poll_voice_events() {
    let mut guard = voice_client().lock().expect("SVC client state poisoned");
    let mut disconnected = false;
    if let Some(client) = guard.as_ref() {
        while let Some(event) = client.poll_event() {
            match event {
                VoiceEvent::Connected => eprintln!("[svchorizon] Voice UDP authenticated and validated."),
                VoiceEvent::Disconnected(reason) => {
                    eprintln!("[svchorizon] Voice disconnected: {reason}");
                    disconnected = true;
                }
                VoiceEvent::AudioUnavailable(reason) => eprintln!("[svchorizon] Audio device warning: {reason}"),
            }
        }
    }
    if disconnected {
        guard.take();
        PTT_DOWN.store(false, Ordering::Relaxed);
    }
    drop(guard);
    if disconnected { start_reconnect(); }
}

pub fn on_mods(mods: &mut Vec<crate::game::native_hooks::NativeMod>) {
    mods.push(crate::game::native_hooks::NativeMod {
        id: "svchorizon".to_owned(),
        title: "SVC Horizon 0.1.0".to_owned(),
        description: "Simple Voice Chat native client".to_owned(),
        keys: vec![
            [
                "push_to_talk".to_owned(),
                "Push to talk".to_owned(),
                "key.keyboard.v".to_owned(),
            ],
            [
                "whisper_mode".to_owned(),
                "Hold to whisper".to_owned(),
                "key.keyboard.b".to_owned(),
            ],
            [
                "voice_activation".to_owned(),
                "Toggle voice activation".to_owned(),
                "key.keyboard.c".to_owned(),
            ],
            [
                "mute_microphone".to_owned(),
                "Toggle microphone mute".to_owned(),
                "key.keyboard.m".to_owned(),
            ],
            [
                "group_create".to_owned(),
                "Create a public voice group".to_owned(),
                "key.keyboard.g".to_owned(),
            ],
            [
                "group_next".to_owned(),
                "Select next available voice group".to_owned(),
                "key.keyboard.n".to_owned(),
            ],
            [
                "group_join".to_owned(),
                "Join selected public voice group".to_owned(),
                "key.keyboard.j".to_owned(),
            ],
            [
                "group_leave".to_owned(),
                "Leave current voice group".to_owned(),
                "key.keyboard.h".to_owned(),
            ],
            [
                "player_next".to_owned(),
                "Select next voice-chat player".to_owned(),
                "key.keyboard.p".to_owned(),
            ],
            [
                "player_ignore".to_owned(),
                "Toggle selected player's local mute".to_owned(),
                "key.keyboard.o".to_owned(),
            ],
            [
                "player_volume_down".to_owned(),
                "Lower selected player's volume".to_owned(),
                "key.keyboard.page.down".to_owned(),
            ],
            [
                "player_volume_up".to_owned(),
                "Raise selected player's volume".to_owned(),
                "key.keyboard.page.up".to_owned(),
            ],
            [
                "input_device_next".to_owned(),
                "Cycle microphone device".to_owned(),
                "key.keyboard.f6".to_owned(),
            ],
            [
                "output_device_next".to_owned(),
                "Cycle speaker device".to_owned(),
                "key.keyboard.f7".to_owned(),
            ],
            [
                "voice_menu".to_owned(),
                "Open the voice chat menu".to_owned(),
                "key.keyboard.z".to_owned(),
            ],
            [
                "toggle_recording".to_owned(),
                "Start or stop recording".to_owned(),
                "key.keyboard.r".to_owned(),
            ],
            [
                "toggle_noise_suppression".to_owned(),
                "Toggle noise suppression".to_owned(),
                "key.keyboard.x".to_owned(),
            ],
        ],
    });
}

/// Called by Horizon's stable native tick hook. All audio/network work stays
/// on worker threads; this only copies the current game pose and PTT state.
pub fn on_tick(game: &mut crate::game::Game<'_>) {
    poll_voice_events();

    // Game::leave_server clears Remote. Stop both workers when the Minecraft
    // connection ends so a transfer or return to title cannot leak a UDP socket.
    if game.remote.is_none() {
        // Advance the generation once per disconnect, not once per frame on
        // the title screen. This invalidates any asynchronous UDP connect
        // still finishing for the previous server.
        if REMOTE_ACTIVE.swap(false, Ordering::AcqRel) {
            SESSION_GENERATION.fetch_add(1, Ordering::AcqRel);
        }
        PTT_DOWN.store(false, Ordering::Relaxed);
        WHISPER_DOWN.store(false, Ordering::Relaxed);
        let old_client = voice_client().lock()
            .expect("SVC client state poisoned")
            .take();
        drop(old_client);
        *group_state().lock().expect("SVC group state poisoned") = GroupState::default();
        *local_player_uuid().lock().expect("SVC local player state poisoned") = None;
        *last_server_info().lock().expect("SVC reconnect state poisoned") = None;
        RECONNECTING.store(false, Ordering::Release);
        CONNECTING.store(false, Ordering::Release);
        GROUPS_ENABLED.store(false, Ordering::Release);
        MENU_OPEN.store(false, Ordering::Relaxed);
        server_host::shutdown();
        if let Some(recorder) = RECORDER.get().and_then(|slot| slot.lock().ok()).and_then(|mut slot| slot.take()) {
            let _ = recorder.finish_detached();
        }
        return;
    }
    REMOTE_ACTIVE.store(true, Ordering::Release);

    // A failed initial connect (bad endpoint, refused socket) must not leave
    // the session stuck with no client: retry through the reconnect path.
    if voice_client().lock().expect("SVC client state poisoned").is_none()
        && !RECONNECTING.load(Ordering::Acquire)
        && !CONNECTING.load(Ordering::Acquire)
        && last_server_info().lock().expect("SVC reconnect state poisoned").is_some()
    {
        start_reconnect();
    }

    let playing = game.native_playing();
    if !playing || !game.native_window() {
        PTT_DOWN.store(false, Ordering::Relaxed);
        WHISPER_DOWN.store(false, Ordering::Relaxed);
        VOICE_ACTIVATION_KEY_DOWN.store(false, Ordering::Relaxed);
        SELF_MUTE_KEY_DOWN.store(false, Ordering::Relaxed);
        GROUP_CREATE_DOWN.store(false, Ordering::Relaxed);
        GROUP_NEXT_DOWN.store(false, Ordering::Relaxed);
        GROUP_JOIN_DOWN.store(false, Ordering::Relaxed);
        GROUP_LEAVE_DOWN.store(false, Ordering::Relaxed);
        PLAYER_NEXT_DOWN.store(false, Ordering::Relaxed);
        PLAYER_IGNORE_DOWN.store(false, Ordering::Relaxed);
        PLAYER_VOL_DOWN.store(false, Ordering::Relaxed);
        PLAYER_VOL_UP.store(false, Ordering::Relaxed);
        INPUT_DEVICE_NEXT_DOWN.store(false, Ordering::Relaxed);
        OUTPUT_DEVICE_NEXT_DOWN.store(false, Ordering::Relaxed);
    }

    let player = game.native_player();
    let positions = game.remote.as_ref()
        .map(crate::game::remote::svchorizon_voice::player_positions)
        .unwrap_or_default();

    let guard = voice_client().lock().expect("SVC client state poisoned");
    let Some(client) = guard.as_ref() else { return; };

    if let Some(player) = player {
        client.set_listener(player.x, player.y, player.z, player.yaw);
    } else {
        client.clear_listener();
    }
    client.set_player_positions(positions);
    let muted = SELF_MUTED.load(Ordering::Relaxed);
    client.set_transmitting(PTT_DOWN.load(Ordering::Relaxed) && !muted && playing);
    client.set_whispering(WHISPER_DOWN.load(Ordering::Relaxed) && playing);
    client.set_voice_activation(VOICE_ACTIVATION.load(Ordering::Relaxed) && !muted, 0.025);
}

/// Push-to-talk is a hold binding. Releasing it always stops transmission.
pub fn on_key(game: &mut crate::game::Game<'_>, key: &str, pressed: bool) {
    if !game.native_on("svchorizon") || (!game.native_window() && pressed) {
        return;
    }
    let ptt_binding = game.native_mod_key("svchorizon", "push_to_talk", "key.keyboard.v");
    let whisper_binding = game.native_mod_key("svchorizon", "whisper_mode", "key.keyboard.b");
    if key == ptt_binding {
        let transmitting = pressed && game.native_playing();
        PTT_DOWN.store(transmitting, Ordering::Relaxed);
        if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
            client.set_transmitting(transmitting && !SELF_MUTED.load(Ordering::Relaxed));
        }
    } else if key == whisper_binding {
        let whispering = pressed && game.native_playing();
        WHISPER_DOWN.store(whispering, Ordering::Relaxed);
        if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
            client.set_whispering(whispering);
        }
    } else if key == game.native_mod_key("svchorizon", "voice_activation", "key.keyboard.c") {
        if pressed && !VOICE_ACTIVATION_KEY_DOWN.swap(true, Ordering::AcqRel) {
            let enabled = !VOICE_ACTIVATION.load(Ordering::Relaxed);
            VOICE_ACTIVATION.store(enabled, Ordering::Relaxed);
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_voice_activation(enabled && !SELF_MUTED.load(Ordering::Relaxed), 0.025);
            }
        } else if !pressed {
            VOICE_ACTIVATION_KEY_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "mute_microphone", "key.keyboard.m") {
        if pressed && !SELF_MUTE_KEY_DOWN.swap(true, Ordering::AcqRel) {
            let muted = !SELF_MUTED.load(Ordering::Relaxed);
            SELF_MUTED.store(muted, Ordering::Relaxed);
            if game.native_playing() {
                send_client_state(game, muted, false);
            }
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_transmitting(PTT_DOWN.load(Ordering::Relaxed) && !muted && game.native_playing());
                client.set_voice_activation(VOICE_ACTIVATION.load(Ordering::Relaxed) && !muted, 0.025);
            }
        } else if !pressed {
            SELF_MUTE_KEY_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_create", "key.keyboard.g") {
        if pressed && !GROUP_CREATE_DOWN.swap(true, Ordering::AcqRel)
            && game.native_playing() && GROUPS_ENABLED.load(Ordering::Acquire) {
            send_create_group(game);
        } else if !pressed {
            GROUP_CREATE_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_next", "key.keyboard.n") {
        if pressed && !GROUP_NEXT_DOWN.swap(true, Ordering::AcqRel) {
            let mut state = group_state().lock().expect("SVC group state poisoned");
            if !state.groups.is_empty() {
                for _ in 0..state.groups.len() {
                    state.selected = (state.selected + 1) % state.groups.len();
                    if !state.groups[state.selected].hidden { break; }
                }
            }
            state.wrong_password = false;
        } else if !pressed {
            GROUP_NEXT_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_join", "key.keyboard.j") {
        if pressed && !GROUP_JOIN_DOWN.swap(true, Ordering::AcqRel)
            && game.native_playing() && GROUPS_ENABLED.load(Ordering::Acquire) {
            let selected = {
                let state = group_state().lock().expect("SVC group state poisoned");
                state.groups.get(state.selected).cloned()
            };
            if let Some(group) = selected.filter(|group| !group.hidden) {
                send_join_group(game, &group);
            }
        } else if !pressed {
            GROUP_JOIN_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_leave", "key.keyboard.h") {
        if pressed && !GROUP_LEAVE_DOWN.swap(true, Ordering::AcqRel)
            && game.native_playing() && GROUPS_ENABLED.load(Ordering::Acquire) {
            send_leave_group(game);
        } else if !pressed {
            GROUP_LEAVE_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "player_next", "key.keyboard.p") {
        if pressed && !PLAYER_NEXT_DOWN.swap(true, Ordering::AcqRel) {
            let mut state = group_state().lock().expect("SVC player state poisoned");
            if !state.players.is_empty() {
                state.selected_player = (state.selected_player + 1) % state.players.len();
            }
        } else if !pressed {
            PLAYER_NEXT_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "player_ignore", "key.keyboard.o") {
        if pressed && !PLAYER_IGNORE_DOWN.swap(true, Ordering::AcqRel) {
            let control = {
                let mut state = group_state().lock().expect("SVC player state poisoned");
                let index = state.selected_player;
                state.players.get_mut(index).map(|player| {
                    player.ignored = !player.ignored;
                    (player.id, player.ignored)
                })
            };
            if let Some((id, ignored)) = control {
                if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                    client.set_player_muted(id, ignored);
                }
            }
        } else if !pressed {
            PLAYER_IGNORE_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "player_volume_down", "key.keyboard.page.down") {
        if pressed && !PLAYER_VOL_DOWN.swap(true, Ordering::AcqRel) {
            let control = {
                let mut state = group_state().lock().expect("SVC player state poisoned");
                let index = state.selected_player;
                state.players.get_mut(index).map(|player| {
                    player.volume = (player.volume - 0.1).clamp(0.0, 2.0);
                    (player.id, player.volume)
                })
            };
            if let Some((id, volume)) = control {
                if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                    client.set_player_volume(id, volume);
                }
            }
        } else if !pressed {
            PLAYER_VOL_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "player_volume_up", "key.keyboard.page.up") {
        if pressed && !PLAYER_VOL_UP.swap(true, Ordering::AcqRel) {
            let control = {
                let mut state = group_state().lock().expect("SVC player state poisoned");
                let index = state.selected_player;
                state.players.get_mut(index).map(|player| {
                    player.volume = (player.volume + 0.1).clamp(0.0, 2.0);
                    (player.id, player.volume)
                })
            };
            if let Some((id, volume)) = control {
                if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                    client.set_player_volume(id, volume);
                }
            }
        } else if !pressed {
            PLAYER_VOL_UP.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "input_device_next", "key.keyboard.f6") {
        if pressed && !INPUT_DEVICE_NEXT_DOWN.swap(true, Ordering::AcqRel) {
            let devices = VoiceClient::input_devices();
            if !devices.is_empty() {
                let step = INPUT_DEVICE_INDEX.fetch_add(1, Ordering::AcqRel);
                let slot = step % (devices.len() + 1);
                let selection = if slot == devices.len() { None } else { Some(devices[slot].clone()) };
                audio_device_preferences().lock().expect("SVC audio preferences poisoned").0 = selection.clone();
                if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                    client.set_input_device(selection.clone());
                }
                eprintln!("[svchorizon] Microphone: {}", selection.as_deref().unwrap_or("OS default"));
            }
        } else if !pressed {
            INPUT_DEVICE_NEXT_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "output_device_next", "key.keyboard.f7") {
        if pressed && !OUTPUT_DEVICE_NEXT_DOWN.swap(true, Ordering::AcqRel) {
            let devices = VoiceClient::output_devices();
            if !devices.is_empty() {
                let step = OUTPUT_DEVICE_INDEX.fetch_add(1, Ordering::AcqRel);
                let slot = step % (devices.len() + 1);
                let selection = if slot == devices.len() { None } else { Some(devices[slot].clone()) };
                audio_device_preferences().lock().expect("SVC audio preferences poisoned").1 = selection.clone();
                if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                    client.set_output_device(selection.clone());
                }
                eprintln!("[svchorizon] Speaker: {}", selection.as_deref().unwrap_or("OS default"));
            }
        } else if !pressed {
            OUTPUT_DEVICE_NEXT_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "voice_menu", "key.keyboard.z") {
        if pressed && !MENU_KEY_DOWN.swap(true, Ordering::AcqRel) && game.native_playing() {
            let opening = !menu_open();
            MENU_OPEN.store(opening, Ordering::Relaxed);
            if opening {
                open_menu_page(MENU_MAIN);
            }
        } else if !pressed {
            MENU_KEY_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "toggle_recording", "key.keyboard.r") {
        if pressed && game.native_playing() && !RECORDING_KEY_DOWN.swap(true, Ordering::AcqRel) {
            toggle_recording(game);
        } else if !pressed {
            RECORDING_KEY_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "toggle_noise_suppression", "key.keyboard.x") {
        if pressed && game.native_playing() && !NOISE_SUPPRESSION_KEY_DOWN.swap(true, Ordering::AcqRel) {
            let announced = voice_client().lock().ok().and_then(|guard| {
                let client = guard.as_ref()?;
                let enabled = !client.is_noise_suppression_enabled();
                client.set_noise_suppression(enabled);
                Some(enabled)
            });
            if let Some(enabled) = announced {
                game.native_chat(if enabled {
                    "SVC: Noise suppression on."
                } else {
                    "SVC: Noise suppression off."
                });
            }
        } else if !pressed {
            NOISE_SUPPRESSION_KEY_DOWN.store(false, Ordering::Release);
        }
    } else if menu_open() && game.native_playing() {
        handle_menu_key(game, key);
    }
}

static RECORDING_KEY_DOWN: AtomicBool = AtomicBool::new(false);
static NOISE_SUPPRESSION_KEY_DOWN: AtomicBool = AtomicBool::new(false);

/// Keyboard navigation of the voice chat menu overlay. The main page has
/// two columns like the official screen: the left icon strip (Up/Down
/// cycles it) and the settings rows; Left/Right move between them and
/// adjust the amplification sliders while one is selected.
fn handle_menu_key(game: &mut crate::game::Game<'_>, key: &str) {
    let page = MENU_PAGE.load(Ordering::Relaxed);
    let row_count = menu_row_count(page);
    match key {
        "key.keyboard.escape" => {
            if page == MENU_MAIN {
                MENU_OPEN.store(false, Ordering::Relaxed);
            } else {
                open_menu_page(MENU_MAIN);
            }
        }
        "key.keyboard.down" => match page {
            MENU_MAIN if MENU_FOCUS.load(Ordering::Relaxed) == 0 => {
                let _ = MENU_ICON.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some((value + 1) % MENU_ICON_COUNT)
                });
            }
            _ => {
                let _ = MENU_SELECTED.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some((value + 1) % row_count)
                });
            }
        },
        "key.keyboard.up" => match page {
            MENU_MAIN if MENU_FOCUS.load(Ordering::Relaxed) == 0 => {
                let _ = MENU_ICON.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some((value + MENU_ICON_COUNT - 1) % MENU_ICON_COUNT)
                });
            }
            _ => {
                let _ = MENU_SELECTED.fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                    Some((value + row_count - 1) % row_count)
                });
            }
        },
        "key.keyboard.enter" => activate_menu_row(game, page, 0),
        "key.keyboard.right" => match page {
            MENU_MAIN if MENU_FOCUS.load(Ordering::Relaxed) == 1
                && MENU_SELECTED.load(Ordering::Relaxed) <= 1 => adjust_amplification(game, 1),
            MENU_MAIN => MENU_FOCUS.store(1, Ordering::Relaxed),
            MENU_GROUPS | MENU_PLAYERS => activate_menu_row(game, page, 1),
            _ => {}
        },
        "key.keyboard.left" => match page {
            MENU_MAIN if MENU_FOCUS.load(Ordering::Relaxed) == 1
                && MENU_SELECTED.load(Ordering::Relaxed) <= 1 => adjust_amplification(game, -1),
            MENU_MAIN => MENU_FOCUS.store(0, Ordering::Relaxed),
            MENU_GROUPS | MENU_PLAYERS => activate_menu_row(game, page, -1),
            _ => {}
        },
        _ => {}
    }
}

/// The number of selectable rows on a menu page (8 rows fit the panel).
fn menu_row_count(page: usize) -> usize {
    match page {
        MENU_GROUPS => group_state().lock().map(|state| state.groups.len().min(8).max(1)).unwrap_or(1),
        MENU_PLAYERS => group_state().lock().map(|state| state.players.len().min(8).max(1)).unwrap_or(1),
        MENU_MIC_DEVICES => mic_device_list().lock().map(|devices| devices.len().min(7) + 1).unwrap_or(1),
        MENU_SPK_DEVICES => spk_device_list().lock().map(|devices| devices.len().min(7) + 1).unwrap_or(1),
        _ => MENU_MAIN_ROWS,
    }
}

/// Switches the menu to `page`, resetting the selection and refreshing the
/// device lists when needed.
fn open_menu_page(page: usize) {
    if matches!(page, MENU_MIC_DEVICES | MENU_SPK_DEVICES) {
        refresh_device_list(page);
    }
    MENU_PAGE.store(page, Ordering::Relaxed);
    MENU_SELECTED.store(0, Ordering::Relaxed);
    MENU_FOCUS.store(1, Ordering::Relaxed);
    MENU_ICON.store(0, Ordering::Relaxed);
}

const MENU_MAIN_ROWS: usize = 8;

/// Enter (direction 0) or Left/Right (-1/1) on a menu row.
fn activate_menu_row(game: &mut crate::game::Game<'_>, page: usize, direction: i32) {
    let selected = MENU_SELECTED.load(Ordering::Relaxed);
    match page {
        MENU_MAIN if MENU_FOCUS.load(Ordering::Relaxed) == 0 => activate_menu_icon(game),
        MENU_MAIN => match selected {
            0 => adjust_microphone_gain(game, direction),
            1 => adjust_speaker_gain(game, direction),
            2 => toggle_noise_suppression_setting(game),
            3 => toggle_voice_activation_setting(game),
            4 => open_menu_page(MENU_MIC_DEVICES),
            5 => open_menu_page(MENU_SPK_DEVICES),
            6 => open_menu_page(MENU_PLAYERS),
            _ => toggle_speakers(game),
        },
        MENU_GROUPS => {
            let state = group_state().lock().expect("SVC group state poisoned");
            let group = state.groups.get(selected).cloned();
            drop(state);
            if let Some(group) = group.filter(|group| !group.hidden) {
                if GROUPS_ENABLED.load(Ordering::Acquire) {
                    if direction < 0 {
                        send_leave_group(game);
                    } else {
                        send_join_group(game, &group);
                    }
                }
            }
        }
        MENU_PLAYERS => {
            let control = {
                let mut state = group_state().lock().expect("SVC group state poisoned");
                state.players.get_mut(selected).map(|player| {
                    if direction < 0 {
                        player.ignored = !player.ignored;
                        (player.id, player.ignored, player.volume)
                    } else {
                        player.volume = (player.volume + 0.1).clamp(0.0, 2.0);
                        (player.id, player.ignored, player.volume)
                    }
                })
            };
            if let Some((id, ignored, volume)) = control {
                if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                    client.set_player_muted(id, ignored);
                    client.set_player_volume(id, volume);
                }
            }
        }
        MENU_MIC_DEVICES | MENU_SPK_DEVICES if direction == 0 => {
            select_menu_device(game, page, selected);
        }
        _ => {}
    }
}

/// Enter on one of the main page's icon buttons: the official screen's left
/// column (microphone, hide icons, group, recording).
fn activate_menu_icon(game: &mut crate::game::Game<'_>) {
    match MENU_ICON.load(Ordering::Relaxed) {
        0 => toggle_microphone(game),
        1 => toggle_hide_icons(game),
        2 => open_menu_page(MENU_GROUPS),
        _ => toggle_recording(game),
    }
}

fn toggle_hide_icons(game: &mut crate::game::Game<'_>) {
    let hidden = !HIDE_ICONS.load(Ordering::Relaxed);
    HIDE_ICONS.store(hidden, Ordering::Relaxed);
    persist_setting(game, "hide_icons", if hidden { "on" } else { "off" });
}

/// Applies and persists a microphone amplification change (0.0-4.0);
/// Enter (direction 0) resets it to 100%, Left/Right step by 10%.
fn adjust_microphone_gain(game: &mut crate::game::Game<'_>, direction: i32) {
    let current = f32::from_bits(PERSISTED_MIC_GAIN.load(Ordering::Relaxed));
    let gain = if direction == 0 {
        1.0
    } else {
        (current + if direction < 0 { -0.1 } else { 0.1 }).clamp(0.0, 4.0)
    };
    PERSISTED_MIC_GAIN.store(gain.to_bits(), Ordering::Relaxed);
    persist_setting(game, "microphone_gain", &format!("{gain:.2}"));
    if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
        client.set_microphone_gain(gain);
    }
}

/// Enter (reset to 100%) or Left/Right (step 10%) on the speaker slider.
fn adjust_speaker_gain(game: &mut crate::game::Game<'_>, direction: i32) {
    let current = f32::from_bits(PERSISTED_SPEAKER_GAIN.load(Ordering::Relaxed));
    let gain = if direction == 0 {
        1.0
    } else {
        (current + if direction < 0 { -0.1 } else { 0.1 }).clamp(0.0, 4.0)
    };
    PERSISTED_SPEAKER_GAIN.store(gain.to_bits(), Ordering::Relaxed);
    persist_setting(game, "speaker_gain", &format!("{gain:.2}"));
    if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
        client.set_speaker_gain(gain);
    }
}

/// Left/Right on one of the two amplification rows: row 0 is the
/// microphone slider, row 1 the speaker slider.
fn adjust_amplification(game: &mut crate::game::Game<'_>, direction: i32) {
    match MENU_SELECTED.load(Ordering::Relaxed) {
        0 => adjust_microphone_gain(game, direction),
        _ => adjust_speaker_gain(game, direction),
    }
}

/// Enter on a device row: selects it (row 0 = OS default), keeps the
/// persisted preference and the F6/F7 cycling position in sync.
fn select_menu_device(game: &mut crate::game::Game<'_>, page: usize, row: usize) {
    let microphone = page == MENU_MIC_DEVICES;
    let slot = if microphone { mic_device_list() } else { spk_device_list() };
    let devices = slot.lock().map(|devices| devices.clone()).unwrap_or_default();
    let selection = if row == 0 { None } else { devices.get(row - 1).cloned() };
    {
        let mut preferences = audio_device_preferences().lock().expect("SVC audio preferences poisoned");
        if microphone { preferences.0 = selection.clone(); } else { preferences.1 = selection.clone(); }
    }
    if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
        if microphone {
            client.set_input_device(selection.clone());
        } else {
            client.set_output_device(selection.clone());
        }
    }
    let slot_index = match &selection {
        None => devices.len(),
        Some(name) => devices.iter().position(|device| device == name).unwrap_or(devices.len()),
    };
    if microphone {
        INPUT_DEVICE_INDEX.store(slot_index, Ordering::Relaxed);
    } else {
        OUTPUT_DEVICE_INDEX.store(slot_index, Ordering::Relaxed);
    }
    game.native_chat(&format!(
        "SVC: {} {}",
        if microphone { "Microphone" } else { "Speaker" },
        selection.as_deref().unwrap_or("set to the system default")
    ));
}

fn toggle_microphone(game: &mut crate::game::Game<'_>) {
    let muted = !SELF_MUTED.load(Ordering::Relaxed);
    SELF_MUTED.store(muted, Ordering::Relaxed);
    if game.native_playing() {
        send_client_state(game, muted, false);
    }
    if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
        client.set_transmitting(PTT_DOWN.load(Ordering::Relaxed) && !muted && game.native_playing());
        client.set_voice_activation(VOICE_ACTIVATION.load(Ordering::Relaxed) && !muted, 0.025);
    }
}

fn toggle_speakers(game: &mut crate::game::Game<'_>) {
    let disabled = !SPEAKERS_DISABLED.load(Ordering::Relaxed);
    SPEAKERS_DISABLED.store(disabled, Ordering::Relaxed);
    if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
        client.set_disabled(disabled);
    }
    let _ = game;
}

fn toggle_noise_suppression_setting(game: &mut crate::game::Game<'_>) {
    let enabled = {
        let guard = voice_client().lock().expect("SVC client state poisoned");
        let Some(client) = guard.as_ref() else { return };
        let enabled = !client.is_noise_suppression_enabled();
        client.set_noise_suppression(enabled);
        enabled
    };
    persist_setting(game, "noise_suppression", if enabled { "on" } else { "off" });
}

fn toggle_voice_activation_setting(game: &mut crate::game::Game<'_>) {
    let enabled = !VOICE_ACTIVATION.load(Ordering::Relaxed);
    VOICE_ACTIVATION.store(enabled, Ordering::Relaxed);
    if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
        client.set_voice_activation(enabled && !SELF_MUTED.load(Ordering::Relaxed), 0.025);
    }
}

pub fn on_hud(
    game: &crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
) {
    if !game.native_playing() || !game.native_on("svchorizon") { return; }
    if HIDE_ICONS.load(Ordering::Relaxed) {
        // The official client's "hide icons": no voice chat HUD at all,
        // though the config screen still opens.
        if menu_open() {
            draw_menu(game, batch);
        }
        return;
    }
    let guard = voice_client().lock().expect("SVC client state poisoned");
    let (label, color) = if SELF_MUTED.load(Ordering::Relaxed) {
        ("SVC: Microphone muted", 0xFF5555)
    } else if let Some(client) = guard.as_ref() {
        if !client.is_connected() && client.unanswered_auth_attempts() >= 15 {
            // Never mask an unreachable voice server behind "Transmitting":
            // packets are being dropped before they can reach anyone.
            if client.received_datagrams() > 0 {
                // The server IS answering; the reply is refused locally (the log
                // has the reason).
                ("SVC: Server replies but the reply was rejected (see log)", 0xFF5555)
            } else if client.icmp_rejections() > 0 {
                // The host replied with ICMP port-unreachable: reachable, but
                // nothing is listening on the voice UDP port.
                ("SVC: Voice port closed on server (nothing listening)", 0xFF5555)
            } else {
                // No reply and no rejection: the datagrams are silently dropped.
                ("SVC: Voice server unreachable (UDP dropped)", 0xFF5555)
            }
        } else if client.is_transmitting() && client.is_whispering() {
            ("SVC: Whispering", 0x55FF55)
        } else if client.is_transmitting() {
            ("SVC: Transmitting", 0x55FF55)
        } else if client.is_voice_activation_enabled() {
            ("SVC: Voice activation", 0x55FF55)
        } else if client.is_connected() {
            ("SVC: Connected", 0xFFFFFF)
        } else {
            ("SVC: Connecting", 0xFFCC55)
        }
    } else if game.remote.is_none() {
        ("SVC: Not on a server", 0xAAAAAA)
    } else if RECONNECTING.load(Ordering::Acquire) {
        ("SVC: Reconnecting", 0xFFCC55)
    } else if last_server_info().lock().expect("SVC reconnect state poisoned").is_some() {
        ("SVC: Connecting", 0xFFCC55)
    } else {
        ("SVC: No voice chat on this server", 0xAAAAAA)
    };
    game.native_text(batch, label, [8.0, 42.0], color, true);
    if let Some(client) = guard.as_ref()
        && !client.is_connected()
        && client.unanswered_auth_attempts() >= 15
    {
        // Show where voice is being sent so it can be compared with the
        // server's real address and voice port.
        let note = format!("-> {}", client.endpoint());
        let x = 8.0 + game.native_text_width(label) + 6.0;
        game.native_text(batch, &note, [x, 42.0], 0xAAAAAA, true);
    }
    let groups = group_state().lock().expect("SVC group state poisoned");
    let group_label = if !GROUPS_ENABLED.load(Ordering::Acquire) {
        "SVC Group: disabled by server".to_owned()
    } else if let Some(active) = groups.active {
        groups.groups.iter().find(|group| group.id == active)
            .map(|group| format!("SVC Group: {}", group.name))
            .unwrap_or_else(|| "SVC Group: joined".to_owned())
    } else if groups.wrong_password {
        "SVC Group: password required | /svchorizon group join <password>".to_owned()
    } else if let Some(group) = groups.groups.get(groups.selected).filter(|group| !group.hidden) {
        let lock = if group.has_password { " [locked]" } else { "" };
        let persistent = if group.persistent { " [persistent]" } else { "" };
        let kind = match group.group_type { 1 => "open", 2 => "isolated", _ => "normal" };
        format!("SVC Group: {}{}{} | {} ({}/{})", group.name, lock, persistent, kind, groups.selected + 1, groups.groups.len())
    } else {
        "SVC Group: no visible groups | G create, N select, J join, H leave".to_owned()
    };
    game.native_text(batch, &group_label, [8.0, 54.0], 0xFFFFFF, true);
    let player_label = if let Some(player) = groups.players.get(groups.selected_player) {
        let status = if player.ignored { "ignored" }
            else if player.force_muted || player.server_muted { "server-muted" }
            else if player.force_disabled || player.disabled { "voice-disabled" }
            else if player.disconnected { "disconnected" }
            else { "active" };
        format!("SVC Player: {} | {} | {:.0}%", player.name, status, player.volume * 100.0)
    } else {
        "SVC Player: none | P select, O ignore, PgUp/PgDn volume".to_owned()
    };
    game.native_text(batch, &player_label, [8.0, 66.0], 0xFFFFFF, true);
    let category_label = if groups.categories.is_empty() {
        "SVC Categories: none".to_owned()
    } else {
        let names = groups.categories.iter().take(3).map(|category| {
            let tab = category.tab_name.as_deref().or(category.tab.as_deref());
            let description = category.description.as_deref()
                .map(|value| value.chars().take(36).collect::<String>());
            match (tab, description) {
                (Some(tab), Some(description)) => format!("{} [{}: {}]", category.name, tab, description),
                (Some(tab), None) => format!("{} [{}]", category.name, tab),
                (None, Some(description)) => format!("{}: {}", category.name, description),
                (None, None) => category.name.clone(),
            }
        }).collect::<Vec<_>>().join(", ");
        let extra = groups.categories.len().saturating_sub(3);
        if extra > 0 { format!("SVC Categories: {names} +{extra}") }
        else { format!("SVC Categories: {names}") }
    };
    game.native_text(batch, &category_label, [8.0, 78.0], 0xAAAAFF, true);
    if let Some(client) = guard.as_ref() {
        let (input, output) = client.selected_audio_devices();
        game.native_text(batch, &format!("SVC Audio: Mic {} | Out {}",
            input.as_deref().unwrap_or("OS default"),
            output.as_deref().unwrap_or("OS default")), [8.0, 90.0], 0xAAAAAA, true);
    }

    draw_status_icons(game, batch, guard.as_ref());
    // Release the client lock before drawing anything else: `draw_group_hud`,
    // `draw_menu` and its row builders (`main_menu_rows`, `player_page_rows`)
    // lock `voice_client()` again, and a std Mutex is not reentrant. Holding
    // `guard` across them deadlocked the render thread: the game froze on the
    // first frame after the voice menu key (Z) opened the menu, and whenever
    // the player was in a group.
    drop(guard);
    draw_group_hud(game, batch, &groups);
    drop(groups);
    if menu_open() {
        draw_menu(game, batch);
    }
}

/// The official client's HUD icons (bottom-left, 16x16): the microphone when
/// transmitting/whispering/muted, the speaker when disabled, the disconnect
/// icon while the voice connection is down, and the group icon while in one.
fn draw_status_icons(
    game: &crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    client: Option<&VoiceClient>,
) {
    if !game.native_playing() {
        return;
    }
    let muted = SELF_MUTED.load(Ordering::Relaxed);
    let disconnected = client
        .map(|client| !client.is_connected())
        .unwrap_or(false);
    let disabled = SPEAKERS_DISABLED.load(Ordering::Relaxed);
    let (icon,) = if disconnected {
        (&icons::DISCONNECTED,)
    } else if disabled {
        (&icons::SPEAKER_OFF,)
    } else if muted {
        (&icons::MICROPHONE_OFF,)
    } else if client.is_some_and(|client| client.is_transmitting() && client.is_whispering()) {
        (&icons::MICROPHONE_WHISPER,)
    } else if client.is_some_and(|client| client.is_transmitting()) {
        (&icons::MICROPHONE,)
    } else {
        return;
    };
    // Above the hotbar's left edge, where the official client places them.
    let bottom = batch_height(batch) - 24.0;
    icons::draw(game, batch, icon, 4.0, bottom);
    if recorder_active() {
        icons::draw(game, batch, &icons::RECORD_BUTTON, 22.0, bottom);
    }
}

/// The HUD's drawing height in GUI pixels (icons anchor to the bottom);
/// `centered_origin(0, 0)` returns the screen centre, so twice its y is the
/// full height.
fn batch_height(batch: &mut horizon_client::hud::HudBatch) -> f32 {
    batch.centered_origin(0.0, 0.0).1 * 2.0
}

/// The group HUD (upstream's GroupChatManager): while in a group, its
/// members' heads above the hotbar with a talking indicator, like the
/// official mod.
fn draw_group_hud(
    game: &crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    groups: &GroupState,
) {
    let Some(active) = groups.active else { return };
    if !GROUPS_ENABLED.load(Ordering::Acquire) {
        return;
    }
    let Some(group) = groups.groups.iter().find(|group| group.id == active) else { return };
    let members: Vec<&NativePlayer> = groups
        .players
        .iter()
        .filter(|player| player.group == Some(active))
        .collect();
    let line_height = 14.0f32;
    let head_size = 8.0f32;
    let width = 132.0f32.max(
        game.native_text_width(&format!("Group: {}", group.name)) + 24.0,
    );
    let height = 16.0 + line_height * (members.len() + 1) as f32;
    game.native_fill(batch, [4.0, 108.0, width, height], 0x90000000);
    game.native_text(batch, &format!("Group: {}", group.name), [10.0, 112.0], 0x55FF55, true);
    for (index, member) in members.iter().enumerate() {
        let y = 112.0 + line_height * (index + 1) as f32;
        let talking = voice_client()
            .lock()
            .map(|client| client.as_ref().is_some_and(|client| client.is_talking(member.id)))
            .unwrap_or(false);
        // The member's head with a green frame while they talk, the way
        // the official group HUD highlights the speaker.
        draw_player_head(game, batch, member.id, &member.name, 10.0, y);
        if talking {
            let frame = 0xFF55FF55;
            game.native_fill(batch, [9.0, y - 1.0, head_size + 2.0, 1.0], frame);
            game.native_fill(batch, [9.0, y + head_size, head_size + 2.0, 1.0], frame);
            game.native_fill(batch, [9.0, y, 1.0, head_size], frame);
            game.native_fill(batch, [9.0 + head_size + 1.0, y, 1.0, head_size], frame);
        }
        let color = if member.ignored {
            0x777777
        } else if talking {
            0x55FF55
        } else {
            0xFFFFFF
        };
        game.native_text(batch, &member.name, [10.0 + head_size + 3.0, y], color, true);
    }
}

/// Draws a player's 8x8 face (base layer plus hat layer) at GUI-pixel
/// `(x, y)`, resolving the skin the way the tab list does: the game uploads
/// every known player's skin into the GUI atlas under `id:<uuid>` and
/// `name:<lowercase name>`, falling back to the UUID-parity default skin
/// (Steve/Alex and friends) while no real skin has arrived.
fn draw_player_head(
    game: &crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    id: [u8; 16],
    name: &str,
    x: f32,
    y: f32,
) {
    let atlas = &game.gui.atlas;
    let id128 = u128::from_be_bytes(id);
    let skin = atlas
        .object_skins
        .get(&format!("id:{id128:032x}"))
        .or_else(|| atlas.object_skins.get(&format!("name:{}", name.to_ascii_lowercase())))
        .filter(|path| atlas.sprites.contains_key(*path))
        .map_or(horizon_client::tab_list::default_skin(id128), String::as_str);
    let Some(rect) = atlas.sprites.get(skin).copied() else {
        return;
    };
    // A skin is a 64x64 texture: the face sits at (8, 8) and its hat
    // overlay at (40, 8), both 8x8 (the tab list's PlayerFaceExtractor).
    let (du, dv) = (rect.u1 - rect.u0, rect.v1 - rect.v0);
    let region = |u: f32, v: f32| horizon_client::hud::Rect {
        u0: rect.u0 + du * u / 64.0,
        v0: rect.v0 + dv * v / 64.0,
        u1: rect.u0 + du * (u + 8.0) / 64.0,
        v1: rect.v0 + dv * (v + 8.0) / 64.0,
        w: 8,
        h: 8,
    };
    batch.rect_uv(x, y, 8.0, 8.0, region(8.0, 8.0), 0xFFFF_FFFF);
    batch.rect_uv(x, y, 8.0, 8.0, region(40.0, 8.0), 0xFFFF_FFFF);
}

/// The voice chat menu overlay, a replica of the official client's
/// VoiceChatScreen: a centered panel with the vertical icon button strip on
/// the left (microphone, hide icons, group, recording), the amplification
/// sliders and device selectors on the right, and the group/volume pages
/// behind them.
fn draw_menu(game: &crate::game::Game<'_>, batch: &mut horizon_client::hud::HudBatch) {
    let page = MENU_PAGE.load(Ordering::Relaxed);
    let selected = MENU_SELECTED.load(Ordering::Relaxed);
    let panel_width = 248.0f32;
    let panel_height = 176.0f32;
    let (x, y) = batch.centered_origin(panel_width, panel_height);
    game.native_fill(batch, [x, y, panel_width, panel_height], 0xC0101018);
    game.native_fill(batch, [x, y, panel_width, 1.0], 0xFF50505A);
    game.native_fill(batch, [x, y + panel_height - 1.0, panel_width, 1.0], 0xFF50505A);
    game.native_fill(batch, [x, y, 1.0, panel_height], 0xFF50505A);
    game.native_fill(batch, [x + panel_width - 1.0, y, 1.0, panel_height], 0xFF50505A);

    let title = match page {
        MENU_GROUPS => "Voice Chat: Groups",
        MENU_PLAYERS => "Voice Chat: Adjust Volumes",
        MENU_MIC_DEVICES => "Voice Chat: Microphone",
        MENU_SPK_DEVICES => "Voice Chat: Speakers",
        _ => "Voice Chat",
    };
    let title_x = x + (panel_width - game.native_text_width(title)) / 2.0;
    game.native_text(batch, title, [title_x, y + 6.0], 0xFFFFFF, true);
    game.native_fill(batch, [x + 6.0, y + 17.0, panel_width - 12.0, 1.0], 0x50505A);

    match page {
        MENU_MAIN => {
            let focus = MENU_FOCUS.load(Ordering::Relaxed);
            let icon = MENU_ICON.load(Ordering::Relaxed);
            // The official screen's left icon column.
            let strip = [
                if SELF_MUTED.load(Ordering::Relaxed) { &icons::MICROPHONE_OFF } else { &icons::MICROPHONE },
                &icons::HIDE_BUTTON,
                &icons::GROUP,
                &icons::RECORD_BUTTON,
            ];
            let strip_x = x + 6.0;
            let mut strip_y = y + 22.0;
            for (index, art) in strip.iter().enumerate() {
                if index == icon && focus == 0 {
                    game.native_fill(batch, [strip_x - 1.0, strip_y - 1.0, 18.0, 18.0], 0x40FFFFFF);
                }
                icons::draw(game, batch, art, strip_x, strip_y);
                if index == 3 && recorder_active() {
                    // A red corner marks a running recording.
                    game.native_fill(batch, [strip_x + 12.0, strip_y + 12.0, 4.0, 4.0], 0xFFFF1313);
                }
                strip_y += 19.0;
            }

            // The settings rows to the right of the strip.
            let row_x = strip_x + 26.0;
            let row_width = panel_width - (row_x - x) - 8.0;
            let mut row_y = y + 22.0;
            for (index, (label, slider, value)) in main_menu_rows().into_iter().enumerate() {
                let highlighted = index == selected && focus == 1;
                match slider {
                    Some(ratio) => {
                        // Official widget layout: label above, bar below.
                        let color = if highlighted { 0xFFFF55 } else { 0xFFFFFF };
                        game.native_text(batch, &label, [row_x, row_y], color, true);
                        let value_width = game.native_text_width(&value);
                        let bar_width = row_width - value_width - 6.0;
                        let bar_y = row_y + 11.0;
                        game.native_fill(batch, [row_x, bar_y, bar_width, 5.0], 0xFF202028);
                        game.native_fill(
                            batch,
                            [row_x, bar_y, (bar_width * ratio.clamp(0.0, 1.0)).max(1.0), 5.0],
                            if highlighted { 0xFF55FF55 } else { 0xFF3FBF3F },
                        );
                        game.native_text(batch, &value, [row_x + bar_width + 6.0, row_y], 0xAAAAAA, true);
                        row_y += 21.0;
                    }
                    None => {
                        if highlighted {
                            game.native_fill(batch, [row_x - 2.0, row_y - 1.0, row_width + 4.0, 11.0], 0x40FFFFFF);
                        }
                        let color = if highlighted { 0xFFFF55 } else { 0xFFFFFF };
                        game.native_text(batch, &label, [row_x, row_y], color, true);
                        row_y += 12.0;
                    }
                }
            }
        }
        _ => {
            let rows = match page {
                MENU_GROUPS => group_page_rows(),
                MENU_PLAYERS => player_page_rows(),
                MENU_MIC_DEVICES | MENU_SPK_DEVICES => device_page_rows(page),
                _ => Vec::new(),
            };
            let mut row_y = y + 22.0;
            for (index, (label, active)) in rows.into_iter().enumerate().take(8) {
                let highlighted = index == selected;
                if highlighted {
                    game.native_fill(batch, [x + 4.0, row_y - 1.0, panel_width - 8.0, 11.0], 0x40FFFFFF);
                }
                let color = if !active {
                    0x888888
                } else if highlighted {
                    0xFFFF55
                } else {
                    0xFFFFFF
                };
                game.native_text(batch, &label, [x + 8.0, row_y], color, true);
                row_y += 12.0;
            }
        }
    }

    let hint = match page {
        MENU_MAIN => "Enter select | < > adjust | Esc close",
        MENU_GROUPS => "Enter join | Left leave | Esc back",
        MENU_PLAYERS => "Right vol+ | Left mute | Esc back",
        _ => "Enter select | Esc back",
    };
    let hint_x = x + (panel_width - game.native_text_width(hint)) / 2.0;
    game.native_text(batch, hint, [hint_x, y + panel_height - 12.0], 0xAAAAAA, true);
}

/// The groups page's rows.
fn group_page_rows() -> Vec<(String, bool)> {
    let state = group_state().lock().expect("SVC group state poisoned");
    if state.groups.is_empty() {
        vec![("No groups on this server".to_owned(), false)]
    } else {
        state
            .groups
            .iter()
            .map(|group| {
                let lock = if group.has_password { " [locked]" } else { "" };
                let kind = match group.group_type { 1 => " open", 2 => " isolated", _ => "" };
                (format!("{}{}{}", group.name, lock, kind), !group.hidden)
            })
            .collect()
    }
}

/// The volumes page's rows.
fn player_page_rows() -> Vec<(String, bool)> {
    let state = group_state().lock().expect("SVC group state poisoned");
    if state.players.is_empty() {
        vec![("No other players on voice".to_owned(), false)]
    } else {
        state
            .players
            .iter()
            .map(|player| {
                let talking = voice_client()
                    .lock()
                    .ok()
                    .as_ref()
                    .and_then(|guard| guard.as_ref())
                    .is_some_and(|client| client.is_talking(player.id));
                let marker = if talking { "> " } else { "  " };
                let flag = if player.ignored { " [muted]" } else { "" };
                (format!("{}{}{} {:.0}%", marker, player.name, flag, player.volume * 100.0), true)
            })
            .collect()
    }
}

/// The main page's right-hand rows: `(label, slider fraction, value text)`;
/// a `None` fraction is a plain action row.
fn main_menu_rows() -> Vec<(String, Option<f32>, String)> {
    let client = voice_client().lock().ok();
    let client = client.as_ref().and_then(|slot| slot.as_ref());
    let mic = f32::from_bits(PERSISTED_MIC_GAIN.load(Ordering::Relaxed));
    let speaker = f32::from_bits(PERSISTED_SPEAKER_GAIN.load(Ordering::Relaxed));
    let noise = if client.is_some_and(|client| client.is_noise_suppression_enabled()) { "on" } else { "OFF" };
    let activation = if client.is_some_and(|client| client.is_voice_activation_enabled()) { "on" } else { "off" };
    let speakers = if SPEAKERS_DISABLED.load(Ordering::Relaxed) { "ON" } else { "off" };
    vec![
        ("Microphone amplification".to_owned(), Some(mic / 4.0), format!("{:.0}%", mic * 100.0)),
        ("Speaker amplification".to_owned(), Some(speaker / 4.0), format!("{:.0}%", speaker * 100.0)),
        (format!("Noise suppression: {noise}"), None, String::new()),
        (format!("Voice activation: {activation}"), None, String::new()),
        ("Select microphone...".to_owned(), None, String::new()),
        ("Select speaker...".to_owned(), None, String::new()),
        ("Adjust volumes...".to_owned(), None, String::new()),
        (format!("Disable voice chat: {speakers}"), None, String::new()),
    ]
}

pub fn on_packet(
    game: &mut crate::game::Game<'_>,
    packet: &horizon_protocol::packets::play::Clientbound,
) {
    use horizon_protocol::packets::play::Clientbound;

    poll_voice_events();
    match packet {
        Clientbound::Login(_) => {
            // A new play session must never inherit the previous server's key.
            REMOTE_ACTIVE.store(true, Ordering::Release);
            SESSION_GENERATION.fetch_add(1, Ordering::AcqRel);
            voice_client().lock().expect("SVC client state poisoned").take();
            *group_state().lock().expect("SVC group state poisoned") = GroupState::default();
            *local_player_uuid().lock().expect("SVC local player state poisoned") = None;
            *last_server_info().lock().expect("SVC reconnect state poisoned") = None;
            RECONNECTING.store(false, Ordering::Release);
            CONNECTING.store(false, Ordering::Release);
            GROUPS_ENABLED.store(false, Ordering::Release);
            PTT_DOWN.store(false, Ordering::Relaxed);
            WHISPER_DOWN.store(false, Ordering::Relaxed);
            server_host::shutdown();
            if let Some(recorder) = RECORDER.get().and_then(|slot| slot.lock().ok()).and_then(|mut slot| slot.take()) {
                let _ = recorder.finish_detached();
            }
            if game.native_on("svchorizon") {
                // Bukkit/Paper only deliver payloads the client registered.
                send_channel_registration(game);
                send_secret_request(game);
            }
        }
        Clientbound::CustomPayload(payload) if payload.channel == SECRET && game.native_on("svchorizon") => {
            match decode_secret(&payload.data) {
                Some(info) => {
                    *local_player_uuid().lock().expect("SVC local player state poisoned") = Some(info.player);
                    GROUPS_ENABLED.store(info.groups, Ordering::Release);
                    RECORDING_ALLOWED.store(info.allow_recording, Ordering::Release);
                    load_persisted_settings(game);
                    send_client_state(game, SELF_MUTED.load(Ordering::Relaxed), false);
                    {
                        let mut state = group_state().lock().expect("SVC player state poisoned");
                        state.players.retain(|player| player.id != info.player);
                        if state.players.is_empty() { state.selected_player = 0; }
                        else { state.selected_player %= state.players.len(); }
                    }
                    // The official client sends voice to the remote address of the
                    // connection it actually made when voice_host is empty (after
                    // SRV redirects and DNS), not to the name the player typed,
                    // which can resolve to a different machine. Fall back to the
                    // typed address only if the socket address is unavailable.
                    // On a world this game hosts itself (integrated server), the
                    // connection is in memory and the voice server listens on
                    // localhost, so 127.0.0.1 is the correct host.
                    let typed_address = game.remote.as_ref()
                        .map(|remote| remote.session.address.clone())
                        .unwrap_or_default();
                    let peer_address = game.remote.as_ref()
                        .map(|remote| horizon_client::net::svchorizon_net::peer_address(&remote.session))
                        .unwrap_or_else(|| Err("no remote session".to_owned()));
                    let fallback_host = peer_address
                        .as_ref()
                        .ok()
                        .map(|peer| peer.to_string())
                        .or_else(|| {
                            game.has_singleplayer_server().then(|| "127.0.0.1".to_owned())
                        })
                        .unwrap_or_else(|| typed_address.clone());
                    eprintln!(
                        "[svchorizon] Secret received: voice host '{}' port {} keepalive {}ms groups {} (typed address: '{typed_address}', connected peer: {})",
                        if info.host.is_empty() { "<same as game server>" } else { info.host.as_str() },
                        info.port,
                        info.keep_alive_ms,
                        info.groups,
                        match &peer_address {
                            Ok(peer) => peer.to_string(),
                            Err(reason) => format!("<unavailable: {reason}; using the typed address>"),
                        }
                    );
                    *last_server_info().lock().expect("SVC reconnect state poisoned") =
                        Some((info.clone(), fallback_host.clone()));
                    let generation = SESSION_GENERATION.load(Ordering::Acquire);
                    CONNECTING.store(true, Ordering::Release);
                    let spawned = thread::Builder::new().name("svchorizon-connect".into()).spawn(move || {
                        match VoiceClient::connect(info, &fallback_host) {
                            Ok(client) => {
                                apply_player_controls(&client);
                                apply_audio_device_preferences(&client);
                                apply_client_settings(&client);
                                if SESSION_GENERATION.load(Ordering::Acquire) == generation {
                                    *voice_client().lock().expect("SVC client state poisoned") = Some(client);
                                    eprintln!("[svchorizon] Voice UDP/audio workers started.");
                                }
                            }
                            Err(error) => eprintln!("[svchorizon] Could not start voice connection: {error}"),
                        }
                        if SESSION_GENERATION.load(Ordering::Acquire) == generation {
                            CONNECTING.store(false, Ordering::Release);
                        }
                    });
                    if spawned.is_err() {
                        CONNECTING.store(false, Ordering::Release);
                    }
                }
                None => eprintln!("[svchorizon] Rejected malformed voicechat:secret payload."),
            }
        }
        Clientbound::CustomPayload(payload) if matches!(
            payload.channel.as_str(),
            "voicechat:joined_group" | "voicechat:add_group" | "voicechat:remove_group"
        ) && game.native_on("svchorizon") => {
            on_group_payload(&payload.channel, &payload.data);
        }
        Clientbound::CustomPayload(payload) if matches!(
            payload.channel.as_str(),
            "voicechat:states" | "voicechat:state" | "voicechat:remove_state"
        ) && game.native_on("svchorizon") => {
            on_player_payload(&payload.channel, &payload.data);
        }
        Clientbound::CustomPayload(payload) if matches!(
            payload.channel.as_str(),
            "voicechat:add_category" | "voicechat:remove_category"
        ) && game.native_on("svchorizon") => {
            on_category_payload(&payload.channel, &payload.data);
        }
        _ => {}
    }
}

/// The mod's buttons on Options > Mods (Horizon's native mod settings GUI).
/// Every click refreshes the labels, so the values show through.
pub fn on_settings(game: &mut crate::game::Game<'_>, mod_id: &str, buttons: &mut Vec<String>) {
    if mod_id != "svchorizon" { return; }
    let client = voice_client().lock().ok();
    let client = client.as_ref().and_then(|slot| slot.as_ref());
    let microphone_gain = PERSISTED_MIC_GAIN.load(Ordering::Relaxed);
    let gain = f32::from_bits(microphone_gain);
    let threshold = f32::from_bits(PERSISTED_ACTIVATION_THRESHOLD.load(Ordering::Relaxed));
    buttons.push(if SELF_MUTED.load(Ordering::Relaxed) {
        "Microphone: muted (click to unmute)".to_owned()
    } else {
        "Microphone: on (click to mute)".to_owned()
    });
    buttons.push(if SPEAKERS_DISABLED.load(Ordering::Relaxed) {
        "Speakers: disabled (click to enable)"
    } else {
        "Speakers: on (click to disable)"
    }.to_owned());
    buttons.push(if PERSISTED_NOISE_SUPPRESSION.load(Ordering::Relaxed) {
        "Noise suppression: on (click to turn off)".to_owned()
    } else {
        "Noise suppression: off (click to turn on)".to_owned()
    });
    buttons.push(if VOICE_ACTIVATION.load(Ordering::Relaxed) {
        "Voice activation: on (click to turn off)".to_owned()
    } else {
        "Voice activation: off (click to turn on)".to_owned()
    });
    buttons.push(format!("Microphone gain: {:.0}% (click for +25%)", gain * 100.0));
    let speaker = f32::from_bits(PERSISTED_SPEAKER_GAIN.load(Ordering::Relaxed));
    buttons.push(format!("Speaker gain: {:.0}% (click for +25%)", speaker * 100.0));
    buttons.push(format!("Activation threshold: {:.1}% (click for +0.5%)", threshold * 100.0));
    buttons.push(if recorder_active() {
        format!("Recording: {} (click to stop)", recording_elapsed_text())
    } else {
        "Recording: idle (click to start)".to_owned()
    });
    if client.is_none() {
        buttons.push("Voice chat: not connected here".to_owned());
    } else {
        buttons.push("Voice chat: press Z in game for the menu".to_owned());
    }
}

/// Handles one click on the Options > Mods settings page.
pub fn on_settings_click(game: &mut crate::game::Game<'_>, mod_id: &str, button: usize) {
    if mod_id != "svchorizon" { return; }
    match button {
        0 => toggle_microphone(game),
        1 => toggle_speakers(game),
        2 => {
            let enabled = !PERSISTED_NOISE_SUPPRESSION.load(Ordering::Relaxed);
            PERSISTED_NOISE_SUPPRESSION.store(enabled, Ordering::Relaxed);
            persist_setting(game, "noise_suppression", if enabled { "on" } else { "off" });
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_noise_suppression(enabled);
            }
        }
        3 => toggle_voice_activation_setting(game),
        4 => {
            let gain = (f32::from_bits(PERSISTED_MIC_GAIN.load(Ordering::Relaxed)) + 0.25).clamp(0.0, 4.0);
            PERSISTED_MIC_GAIN.store(gain.to_bits(), Ordering::Relaxed);
            persist_setting(game, "microphone_gain", &gain.to_string());
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_microphone_gain(gain);
            }
        }
        5 => {
            let gain = (f32::from_bits(PERSISTED_SPEAKER_GAIN.load(Ordering::Relaxed)) + 0.25).clamp(0.0, 4.0);
            PERSISTED_SPEAKER_GAIN.store(gain.to_bits(), Ordering::Relaxed);
            persist_setting(game, "speaker_gain", &gain.to_string());
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_speaker_gain(gain);
            }
        }
        6 => {
            let mut threshold = f32::from_bits(PERSISTED_ACTIVATION_THRESHOLD.load(Ordering::Relaxed)) + 0.005;
            if threshold > 0.2 { threshold = 0.005; }
            PERSISTED_ACTIVATION_THRESHOLD.store(threshold.to_bits(), Ordering::Relaxed);
            persist_setting(game, "activation_threshold", &threshold.to_string());
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_voice_activation(
                    VOICE_ACTIVATION.load(Ordering::Relaxed) && !SELF_MUTED.load(Ordering::Relaxed),
                    threshold,
                );
            }
        }
        7 => toggle_recording(game),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_official_category_metadata_and_optional_fields() {
        let mut bytes = Vec::new();
        write_utf(&mut bytes, "music");
        write_utf(&mut bytes, "Music");
        bytes.push(0); // no name translation key
        bytes.push(1); write_utf(&mut bytes, "Background music");
        bytes.push(0); // no description translation key
        bytes.push(0); // no icon
        bytes.push(1); write_utf(&mut bytes, "media");
        bytes.push(1); write_utf(&mut bytes, "Media");
        bytes.push(0); // no tab translation key
        let mut cursor = Cursor::new(&bytes);
        let category = decode_category(&mut cursor).unwrap();
        assert!(cursor.done());
        assert_eq!(category.id, "music");
        assert_eq!(category.name, "Music");
        assert_eq!(category.description.as_deref(), Some("Background music"));
        assert_eq!(category.tab.as_deref(), Some("media"));
        assert_eq!(category.tab_name.as_deref(), Some("Media"));
    }

    #[test]
    fn rejects_category_with_invalid_id_or_truncated_icon() {
        let mut invalid_id = Vec::new();
        write_utf(&mut invalid_id, "Bad-ID");
        let mut cursor = Cursor::new(&invalid_id);
        assert!(decode_category(&mut cursor).is_none());

        let mut truncated_icon = Vec::new();
        write_utf(&mut truncated_icon, "music");
        write_utf(&mut truncated_icon, "Music");
        truncated_icon.extend_from_slice(&[0, 0, 0, 1]);
        let mut cursor = Cursor::new(&truncated_icon);
        assert!(decode_category(&mut cursor).is_none());
    }

    #[test]
    fn encodes_group_create_and_password_join_payloads() {
        let create = create_group_payload("Friends", Some("s3cret phrase"), 2);
        let mut cursor = Cursor::new(&create);
        assert_eq!(cursor.string(24).as_deref(), Some("Friends"));
        assert_eq!(cursor.bool(), Some(true));
        assert_eq!(cursor.string(256).as_deref(), Some("s3cret phrase"));
        assert_eq!(cursor.i16(), Some(2));
        assert!(cursor.done());

        let join = join_group_payload([0xAB; 16], Some("pass"));
        let mut cursor = Cursor::new(&join);
        assert_eq!(cursor.uuid(), Some([0xAB; 16]));
        assert_eq!(cursor.bool(), Some(true));
        assert_eq!(cursor.string(256).as_deref(), Some("pass"));
        assert!(cursor.done());
    }

    #[test]
    fn parses_quoted_group_names_and_passwords() {
        let words = command_words(r#"svchorizon group create "Friends Group" "a password" open"#).unwrap();
        assert_eq!(words, vec!["svchorizon", "group", "create", "Friends Group", "a password", "open"]);
        assert!(command_words(r#"svchorizon group create "unfinished"#).is_none());
    }

    #[test]
    fn encodes_official_client_state_flags() {
        assert_eq!(client_state_payload(false, false), vec![0, 0]);
        assert_eq!(client_state_payload(true, false), vec![1, 0]);
        assert_eq!(client_state_payload(false, true), vec![0, 1]);
    }

    #[test]
    fn decodes_official_player_state_flags_and_optional_group() {
        let mut bytes = vec![0b0011_1111];
        bytes.extend_from_slice(&[0x11; 16]);
        write_utf(&mut bytes, "TestPlayer");
        bytes.extend_from_slice(&[0x22; 16]);
        let mut cursor = Cursor::new(&bytes);
        let player = decode_player_state(&mut cursor).unwrap();
        assert!(cursor.done());
        assert_eq!(player.id, [0x11; 16]);
        assert_eq!(player.name, "TestPlayer");
        assert!(player.server_muted);
        assert!(player.force_muted);
        assert!(player.disabled);
        assert!(player.force_disabled);
        assert!(player.disconnected);
        assert_eq!(player.group, Some([0x22; 16]));
        assert_eq!(player.volume, 1.0);
        assert!(!player.ignored);
    }

    #[test]
    fn registers_every_official_channel_before_requesting_the_secret() {
        let data = register_channels_payload();
        let channels: Vec<&str> = data.split(|byte| *byte == 0)
            .map(|channel| std::str::from_utf8(channel).unwrap()).collect();
        assert_eq!(channels.first().copied(), Some("voicechat:request_secret"));
        assert!(channels.contains(&"voicechat:secret"));
        assert!(channels.contains(&"voicechat:client_state"));
        assert!(channels.contains(&"voicechat:states"));
        assert!(channels.contains(&"voicechat:remove_state"));
        assert!(channels.contains(&"voicechat:joined_group"));
        assert!(channels.contains(&"voicechat:add_category"));
        assert_eq!(channels.len(), REGISTERED_CHANNELS.len());
    }

    #[test]
    fn decodes_secret_payload_in_2_6_27_field_order() {
        let mut bytes = vec![0x42; 16];
        bytes.extend_from_slice(&24454i32.to_be_bytes());
        bytes.extend_from_slice(&[0x11; 16]);
        bytes.push(0);
        bytes.extend_from_slice(&1200i32.to_be_bytes());
        bytes.extend_from_slice(&48.0f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(&1000i32.to_be_bytes());
        bytes.push(1);
        bytes.push(b"voice.example.test".len() as u8);
        bytes.extend_from_slice(b"voice.example.test");
        bytes.push(0);
        let info = decode_secret(&bytes).unwrap();
        assert_eq!(info.port, 24454);
        assert_eq!(info.mtu, 1200);
        assert_eq!(info.distance, 48.0);
        assert_eq!(info.host, "voice.example.test");
        assert!(info.groups);
    }

    #[test]
    fn rejects_truncated_or_trailing_payloads() {
        assert!(decode_secret(&[0; 8]).is_none());
        let mut bytes = vec![0x42; 16];
        bytes.extend_from_slice(&24454i32.to_be_bytes());
        bytes.extend_from_slice(&[0x11; 16]);
        bytes.push(0);
        bytes.extend_from_slice(&1200i32.to_be_bytes());
        bytes.extend_from_slice(&48.0f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(&1000i32.to_be_bytes());
        bytes.push(0); bytes.push(0); bytes.push(0);
        bytes.push(99);
        assert!(decode_secret(&bytes).is_none());
    }
}
