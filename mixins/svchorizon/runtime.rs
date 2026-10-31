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

#[derive(Default)]
struct GroupState {
    groups: Vec<NativeGroup>,
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
    let mut state = group_state().lock().expect("SVC player state poisoned");
    match channel {
        "voicechat:states" => {
            let mut cursor = Cursor::new(bytes);
            let Some(count) = cursor.i32().and_then(|n| usize::try_from(n).ok()) else { return; };
            if count > 512 { return; }
            let old = state.players.clone();
            let mut players = Vec::with_capacity(count.min(128));
            for _ in 0..count {
                let Some(mut player) = decode_player_state(&mut cursor) else { return; };
                preserve_player_controls(&mut player, &old);
                if players.len() < 128 && !players.iter().any(|entry: &NativePlayer| entry.id == player.id) {
                    players.push(player);
                }
            }
            if !cursor.done() { return; }
            state.players = players;
            if state.players.is_empty() { state.selected_player = 0; }
            else { state.selected_player %= state.players.len(); }
        }
        "voicechat:state" => {
            let mut cursor = Cursor::new(bytes);
            let Some(mut player) = decode_player_state(&mut cursor) else { return; };
            if !cursor.done() { return; }
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
        return;
    }
    REMOTE_ACTIVE.store(true, Ordering::Release);

    let playing = game.native_playing();
    if !playing {
        PTT_DOWN.store(false, Ordering::Relaxed);
        WHISPER_DOWN.store(false, Ordering::Relaxed);
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
    if !game.native_on("svchorizon") || !game.native_window() {
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
            if let Some(client) = voice_client().lock().expect("SVC client state poisoned").as_ref() {
                client.set_transmitting(PTT_DOWN.load(Ordering::Relaxed) && !muted && game.native_playing());
                client.set_voice_activation(VOICE_ACTIVATION.load(Ordering::Relaxed) && !muted, 0.025);
            }
        } else if !pressed {
            SELF_MUTE_KEY_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_create", "key.keyboard.g") {
        if pressed && !GROUP_CREATE_DOWN.swap(true, Ordering::AcqRel) && game.native_playing() {
            send_create_group(game);
        } else if !pressed {
            GROUP_CREATE_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_next", "key.keyboard.n") {
        if pressed && !GROUP_NEXT_DOWN.swap(true, Ordering::AcqRel) {
            let mut state = group_state().lock().expect("SVC group state poisoned");
            if !state.groups.is_empty() {
                state.selected = (state.selected + 1) % state.groups.len();
            }
            state.wrong_password = false;
        } else if !pressed {
            GROUP_NEXT_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_join", "key.keyboard.j") {
        if pressed && !GROUP_JOIN_DOWN.swap(true, Ordering::AcqRel) && game.native_playing() {
            let selected = {
                let state = group_state().lock().expect("SVC group state poisoned");
                state.groups.get(state.selected).cloned()
            };
            if let Some(group) = selected { send_join_group(game, &group); }
        } else if !pressed {
            GROUP_JOIN_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_leave", "key.keyboard.h") {
        if pressed && !GROUP_LEAVE_DOWN.swap(true, Ordering::AcqRel) && game.native_playing() {
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
    let Some(client) = guard.as_ref() else { return; };
    let (label, color) = if SELF_MUTED.load(Ordering::Relaxed) {
        ("SVC: Microphone muted", 0xFF5555)
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
    };
    game.native_text(batch, label, [8.0, 42.0], color, true);
    let groups = group_state().lock().expect("SVC group state poisoned");
    let group_label = if let Some(active) = groups.active {
        groups.groups.iter().find(|group| group.id == active)
            .map(|group| format!("SVC Group: {}", group.name))
            .unwrap_or_else(|| "SVC Group: joined".to_owned())
    } else if groups.wrong_password {
        "SVC Group: password required (password entry not available yet)".to_owned()
    } else if let Some(group) = groups.groups.get(groups.selected) {
        let suffix = if group.has_password { " [locked]" } else { "" };
        format!("SVC Group: {}{} ({}/{})", group.name, suffix, groups.selected + 1, groups.groups.len())
    } else {
        "SVC Group: none | G create, N select, J join, H leave".to_owned()
    };
    let _group_metadata = (groups.groups.get(groups.selected).map(|g| (g.persistent, g.hidden, g.group_type)));
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
            PTT_DOWN.store(false, Ordering::Relaxed);
            WHISPER_DOWN.store(false, Ordering::Relaxed);
            if game.native_on("svchorizon") { send_secret_request(game); }
        }
        Clientbound::CustomPayload(payload) if payload.channel == SECRET && game.native_on("svchorizon") => {
            match decode_secret(&payload.data) {
                Some(info) => {
                    let fallback_host = game.remote.as_ref()
                        .map(|remote| remote.session.address.clone())
                        .unwrap_or_default();
                    let generation = SESSION_GENERATION.load(Ordering::Acquire);
                    let _ = thread::Builder::new().name("svchorizon-connect".into()).spawn(move || {
                        match VoiceClient::connect(info, &fallback_host) {
                            Ok(client) => {
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
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
