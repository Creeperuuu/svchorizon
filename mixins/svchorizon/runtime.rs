use std::{sync::{Mutex, OnceLock, atomic::{AtomicBool, AtomicU64, Ordering}}, thread};
use svchorizon_engine::voice::{VoiceClient, VoiceEvent, VoiceServerInfo};

const SVC_COMPATIBILITY_VERSION: i32 = 20;
const REQUEST_SECRET: &str = "voicechat:request_secret";
const SECRET: &str = "voicechat:secret";

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
static GROUPS_ENABLED: AtomicBool = AtomicBool::new(false);

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
            match VoiceClient::connect(info.clone(), &fallback_host) {
                Ok(client) => {
                    apply_player_controls(&client);
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
        secret, port, player, codec, mtu, distance, keep_alive_ms,
        groups, host, allow_recording,
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

fn send_create_group(game: &mut crate::game::Game<'_>) {
    let mut data = Vec::new();
    write_utf(&mut data, "Horizon");
    data.push(0); // no password
    data.extend_from_slice(&0i16.to_be_bytes()); // Group.Type.NORMAL
    send_group_payload(game, "voicechat:create_group", data);
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

fn preserve_player_controls(player: &mut NativePlayer, old: &[NativePlayer]) {
    if let Some(previous) = old.iter().find(|entry| entry.id == player.id) {
        player.volume = previous.volume;
        player.ignored = previous.ignored;
    }
}

fn on_player_payload(channel: &str, bytes: &[u8]) {
    let local_id = *local_player_uuid().lock().expect("SVC local player state poisoned");
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
        GROUPS_ENABLED.store(false, Ordering::Release);
        return;
    }
    REMOTE_ACTIVE.store(true, Ordering::Release);

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
    }
}

pub fn on_hud(
    game: &crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
) {
    if !game.native_playing() || !game.native_on("svchorizon") { return; }
    let guard = voice_client().lock().expect("SVC client state poisoned");
    let (label, color) = if SELF_MUTED.load(Ordering::Relaxed) {
        ("SVC: Microphone muted", 0xFF5555)
    } else if let Some(client) = guard.as_ref() {
        if client.is_transmitting() && client.is_whispering() {
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
    } else if RECONNECTING.load(Ordering::Acquire) {
        ("SVC: Reconnecting", 0xFFCC55)
    } else {
        ("SVC: Connecting", 0xFFCC55)
    };
    game.native_text(batch, label, [8.0, 42.0], color, true);
    let groups = group_state().lock().expect("SVC group state poisoned");
    let group_label = if !GROUPS_ENABLED.load(Ordering::Acquire) {
        "SVC Group: disabled by server".to_owned()
    } else if let Some(active) = groups.active {
        groups.groups.iter().find(|group| group.id == active)
            .map(|group| format!("SVC Group: {}", group.name))
            .unwrap_or_else(|| "SVC Group: joined".to_owned())
    } else if groups.wrong_password {
        "SVC Group: password required (password entry not available yet)".to_owned()
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
        let names = groups.categories.iter().take(3)
            .map(|category| category.name.as_str()).collect::<Vec<_>>().join(", ");
        let extra = groups.categories.len().saturating_sub(3);
        if extra > 0 { format!("SVC Categories: {names} +{extra}") }
        else { format!("SVC Categories: {names}") }
    };
    game.native_text(batch, &category_label, [8.0, 78.0], 0xAAAAFF, true);
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
            GROUPS_ENABLED.store(false, Ordering::Release);
            PTT_DOWN.store(false, Ordering::Relaxed);
            WHISPER_DOWN.store(false, Ordering::Relaxed);
            if game.native_on("svchorizon") { send_secret_request(game); }
        }
        Clientbound::CustomPayload(payload) if payload.channel == SECRET && game.native_on("svchorizon") => {
            match decode_secret(&payload.data) {
                Some(info) => {
                    *local_player_uuid().lock().expect("SVC local player state poisoned") = Some(info.player);
                    GROUPS_ENABLED.store(info.groups, Ordering::Release);
                    send_client_state(game, SELF_MUTED.load(Ordering::Relaxed), false);
                    {
                        let mut state = group_state().lock().expect("SVC player state poisoned");
                        state.players.retain(|player| player.id != info.player);
                        if state.players.is_empty() { state.selected_player = 0; }
                        else { state.selected_player %= state.players.len(); }
                    }
                    let fallback_host = game.remote.as_ref()
                        .map(|remote| remote.session.address.clone())
                        .unwrap_or_default();
                    *last_server_info().lock().expect("SVC reconnect state poisoned") =
                        Some((info.clone(), fallback_host.clone()));
                    let generation = SESSION_GENERATION.load(Ordering::Acquire);
                    let _ = thread::Builder::new().name("svchorizon-connect".into()).spawn(move || {
                        match VoiceClient::connect(info, &fallback_host) {
                            Ok(client) => {
                                apply_player_controls(&client);
                                if SESSION_GENERATION.load(Ordering::Acquire) == generation {
                                    *voice_client().lock().expect("SVC client state poisoned") = Some(client);
                                    eprintln!("[svchorizon] Voice UDP/audio workers started.");
                                }
                            }
                            Err(error) => eprintln!("[svchorizon] Could not start voice connection: {error}"),
                        }
                    });
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
    fn parses_secret_payload_in_upstream_field_order() {
        let mut bytes = vec![0x42; 16];
        bytes.extend_from_slice(&24454i32.to_be_bytes());
        bytes.extend_from_slice(&[0x11; 16]);
        bytes.push(0);
        bytes.extend_from_slice(&1200i32.to_be_bytes());
        bytes.extend_from_slice(&48.0f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(&1000i32.to_be_bytes());
        bytes.push(1);
        bytes.push(19);
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
