use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};
use svchorizon_engine::voice::{SvcProfile, VoiceClient, VoiceEvent, VoiceServerInfo};

// Compatibility numbers of the Simple Voice Chat wire generations this
// client speaks, probed ONE AT A TIME, most likely first. A server answers
// only the request whose number matches its own plugin build: every other
// number gets the player an incompatibility chat notice instead of a secret
// (upstream ServerVoiceEvents.getIncompatibleMessage), and a repeated
// matching request is silently ignored because the secret is generated once
// per login. Asking one number at a time also keeps the server's per-player
// compatibility flag on the generation that actually worked - upstream
// stores the number of the LAST request, so a stale extra probe would make
// the server drop this client's state and group packets.
//
// Compat 20 = Simple Voice Chat 2.6.x (the release line across Minecraft
// 1.12.2 up to and including 1.21.x/26.x servers, e.g. voicechat-bukkit
// 2.6.27). Compat 21 = Simple Voice Chat 2.7.x (connection UUID + nonce
// challenge UDP generation, currently released as 2.7.0-pre1). Compat 18 =
// Simple Voice Chat 2.5.x (the previous release line still running on
// servers that have not updated; wire-identical to compat 20). This is a
// property of the server's plugin build, not of one Minecraft version, so
// probing the whole list supports older and newer servers alike.
const REQUEST_SECRET: &str = "voicechat:request_secret";
const SECRET: &str = "voicechat:secret";
const REGISTER: &str = "minecraft:register";
/// Compatibility numbers to ask about, in probe order.
const PROBE_SEQUENCE: &[i32] = &[20, 21, 18];
/// How long to wait for an answer (secret or rejection notice) before the
/// next generation is probed.
const PROBE_TIMEOUT_MS: u64 = 4_000;
/// How long after the first probe a fully silent server is reported as
/// having no voice chat (probing continues in the background regardless).
const NO_VOICE_CHAT_AFTER_MS: u64 = 15_000;
/// The generation the outstanding request was sent for (0 = none).
static PENDING_PROBE: AtomicI32 = AtomicI32::new(0);
/// The generation the most recent request was sent for; late rejection
/// notices that arrive after the timeout are attributed to it.
static LAST_PROBED: AtomicI32 = AtomicI32::new(0);
/// Unix millis the outstanding request was sent at (also the send pacemaker).
static PENDING_AT_MS: AtomicU64 = AtomicU64::new(0);
/// Unix millis of the first probe of this login.
static FIRST_PROBE_MS: AtomicU64 = AtomicU64::new(0);
/// Bitmask of generations the server actively rejected (one bit per index in
/// PROBE_SEQUENCE); a rejected generation is never probed again this login,
/// which keeps the incompatibility chat notices to at most one per server.
static REJECTED_MASK: AtomicU32 = AtomicU32::new(0);
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
        if index > 0 {
            data.push(0);
        }
        data.extend_from_slice(channel.as_bytes());
    }
    data
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
static GROUPS_ENABLED: AtomicBool = AtomicBool::new(false);
static AUDIO_DEVICE_PREFS: OnceLock<Mutex<(Option<String>, Option<String>)>> = OnceLock::new();

fn audio_device_preferences() -> &'static Mutex<(Option<String>, Option<String>)> {
    AUDIO_DEVICE_PREFS.get_or_init(|| Mutex::new((None, None)))
}

fn apply_audio_device_preferences(client: &VoiceClient) {
    let preferences = audio_device_preferences()
        .lock()
        .expect("SVC audio preferences poisoned")
        .clone();
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
        state
            .players
            .iter()
            .map(|player| (player.id, player.volume, player.ignored))
            .collect::<Vec<_>>()
    };
    for (id, volume, ignored) in controls {
        client.set_player_volume(id, volume);
        client.set_player_muted(id, ignored);
    }
}

fn start_reconnect() {
    if RECONNECTING.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some((info, fallback_host)) = last_server_info()
        .lock()
        .expect("SVC reconnect state poisoned")
        .clone()
    else {
        RECONNECTING.store(false, Ordering::Release);
        return;
    };
    let generation = SESSION_GENERATION.load(Ordering::Acquire);
    let spawned = thread::Builder::new()
        .name("svchorizon-reconnect".into())
        .spawn(move || {
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
                        apply_audio_device_preferences(&client);
                        if SESSION_GENERATION.load(Ordering::Acquire) == generation
                            && REMOTE_ACTIVE.load(Ordering::Acquire)
                        {
                            let mut slot =
                                voice_client().lock().expect("SVC client state poisoned");
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
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }
    fn bool(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn optional_string(&mut self, max_chars: usize) -> Option<Option<String>> {
        if self.bool()? {
            Some(Some(self.string(max_chars)?))
        } else {
            Some(None)
        }
    }
    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn uuid(&mut self) -> Option<[u8; 16]> {
        self.take(16)?.try_into().ok()
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(u64::from_be_bytes(
            self.take(8)?.try_into().ok()?,
        )))
    }
    fn varint(&mut self) -> Option<usize> {
        let mut out = 0usize;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 4 && b & 0xf0 != 0 {
                return None;
            }
            out |= usize::from(b & 0x7f).checked_shl(7 * i)?;
            if b & 0x80 == 0 {
                return Some(out);
            }
        }
        None
    }
    fn string(&mut self, max_chars: usize) -> Option<String> {
        let len = self.varint()?;
        if len > max_chars.checked_mul(4)? {
            return None;
        }
        let value = std::str::from_utf8(self.take(len)?).ok()?;
        if value.chars().count() > max_chars {
            return None;
        }
        Some(value.to_owned())
    }
    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

/// Decodes the SecretPacket without a generation hint, trying the 2.6.x
/// layout first (which also covers 2.5.x) and then the 2.7.x layout.
fn decode_secret_for_generation(bytes: &[u8], expected: i32) -> Option<VoiceServerInfo> {
    let mut order: Vec<SvcProfile> = Vec::new();
    if let Some(profile) = SvcProfile::from_compatibility_version(expected) {
        order.push(profile);
    }
    if !order.contains(&SvcProfile::Compat20) {
        order.push(SvcProfile::Compat20);
    }
    if !order.contains(&SvcProfile::Compat21) {
        order.push(SvcProfile::Compat21);
    }
    order
        .iter()
        .find_map(|profile| parse_secret_layout(bytes, *profile))
}

/// Parses the SecretPacket in the field order of `profile`'s wire generation.
fn parse_secret_layout(bytes: &[u8], profile: SvcProfile) -> Option<VoiceServerInfo> {
    let mut c = Cursor::new(bytes);
    let secret: [u8; 16] = c.take(16)?.try_into().ok()?;
    match profile {
        // Compat-20/18 (Simple Voice Chat 2.5.x/2.6.x) SecretPacket.toBytes
        // order: secret, port, player, codec, mtu, distance, keepalive,
        // groups, host, recording. There is no server-assigned connection
        // UUID in this wire generation: the secret doubles as the UUID that
        // keys every UDP datagram we send, so the engine derives it from the
        // secret bytes.
        SvcProfile::Compat20 | SvcProfile::Compat18 => {
            finish_secret(&mut c, secret, profile, [0u8; 16])
        }
        // Compat-21 (Simple Voice Chat 2.7.x) SecretPacket.toBytes order
        // adds a server-assigned connection UUID between the secret and the
        // port: secret, connectionId, port, player, codec, mtu, distance,
        // keepalive, groups, host, recording. That UUID replaces the player
        // UUID inside every UDP envelope this generation.
        SvcProfile::Compat21 => {
            let connection_id = c.uuid()?;
            finish_secret(&mut c, secret, profile, connection_id)
        }
    }
}

/// Decodes the SecretPacket tail shared by the wire generations, starting
/// at the port field, and validates the trailing metadata.
fn finish_secret(
    c: &mut Cursor<'_>,
    secret: [u8; 16],
    profile: svchorizon_engine::voice::SvcProfile,
    connection_id: [u8; 16],
) -> Option<VoiceServerInfo> {
    let port = c.i32()?;
    let port = u16::try_from(port).ok().filter(|p| *p != 0)?;
    let player = c.uuid()?;
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
        profile,
        connection_id,
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

/// Resets the serial probe state; runs on every login and disconnect.
fn reset_probe_state() {
    PENDING_PROBE.store(0, Ordering::Release);
    LAST_PROBED.store(0, Ordering::Release);
    PENDING_AT_MS.store(0, Ordering::Release);
    FIRST_PROBE_MS.store(0, Ordering::Release);
    REJECTED_MASK.store(0, Ordering::Release);
}

fn rejection_bit(generation: i32) -> Option<u32> {
    PROBE_SEQUENCE
        .iter()
        .position(|candidate| *candidate == generation)
        .map(|index| 1 << index)
}

/// The next compatibility number to ask about: the first entry of
/// PROBE_SEQUENCE the server has not rejected yet, starting after the
/// generation of the most recent request. None once every generation was
/// rejected - the server runs a Simple Voice Chat build this client cannot
/// speak.
fn next_probe_generation() -> Option<i32> {
    let start = PROBE_SEQUENCE
        .iter()
        .position(|candidate| *candidate == LAST_PROBED.load(Ordering::Acquire))
        .map_or(0, |index| (index + 1) % PROBE_SEQUENCE.len());
    let mask = REJECTED_MASK.load(Ordering::Acquire);
    for offset in 0..PROBE_SEQUENCE.len() {
        let index = (start + offset) % PROBE_SEQUENCE.len();
        if mask & (1 << index) == 0 {
            return Some(PROBE_SEQUENCE[index]);
        }
    }
    None
}

fn all_generations_rejected() -> bool {
    next_probe_generation().is_none()
}

fn send_secret_request(game: &mut crate::game::Game<'_>) {
    use horizon_protocol::packets::{configuration::CustomPayload, play::Serverbound};
    let Some(generation) = next_probe_generation() else {
        return;
    };
    game.send(Serverbound::CustomPayload(CustomPayload {
        channel: REQUEST_SECRET.to_owned(),
        data: generation.to_be_bytes().to_vec(),
    }));
    let now = now_millis();
    if FIRST_PROBE_MS.load(Ordering::Acquire) == 0 {
        FIRST_PROBE_MS.store(now, Ordering::Release);
    }
    PENDING_PROBE.store(generation, Ordering::Release);
    LAST_PROBED.store(generation, Ordering::Release);
    PENDING_AT_MS.store(now, Ordering::Release);
    eprintln!("[svchorizon] Probing Simple Voice Chat compatibility version {generation}");
}

/// Marks the outstanding generation as rejected: the server answered the
/// request with its incompatibility chat notice instead of a secret.
fn reject_pending_generation() {
    let pending = PENDING_PROBE.swap(0, Ordering::AcqRel);
    let generation = if pending != 0 {
        pending
    } else {
        LAST_PROBED.load(Ordering::Acquire)
    };
    if generation == 0 {
        return;
    }
    if let Some(bit) = rejection_bit(generation) {
        REJECTED_MASK.fetch_or(bit, Ordering::AcqRel);
    }
    eprintln!(
        "[svchorizon] Server rejected Simple Voice Chat compatibility version {generation} \
         (incompatible_version notice) - trying the next generation"
    );
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
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
    if let Some(password) = password {
        write_utf(&mut data, password);
    }
    data.extend_from_slice(&group_type.to_be_bytes());
    data
}

fn join_group_payload(group_id: [u8; 16], password: Option<&str>) -> Vec<u8> {
    let mut data = group_id.to_vec();
    data.push(u8::from(password.is_some()));
    if let Some(password) = password {
        write_utf(&mut data, password);
    }
    data
}

fn send_create_group_custom(
    game: &mut crate::game::Game<'_>,
    name: &str,
    password: Option<&str>,
    group_type: i16,
) {
    send_group_payload(
        game,
        "voicechat:create_group",
        create_group_payload(name, password, group_type),
    );
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
        game.native_chat(
            "SVC: This group requires a password. Use /svchorizon group join <password>.",
        );
        return;
    }
    send_group_payload(
        game,
        "voicechat:set_group",
        join_group_payload(group.id, password),
    );
}

fn send_join_group(game: &mut crate::game::Game<'_>, group: &NativeGroup) {
    if group.has_password {
        group_state()
            .lock()
            .expect("SVC group state poisoned")
            .wrong_password = true;
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
    if input.len() > 1024 {
        return None;
    }
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
                if words.len() > 8 {
                    return None;
                }
            }
        } else {
            current.push(ch);
        }
        if current.chars().count() > 256 {
            return None;
        }
    }
    if escaped || quoted {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    (words.len() <= 8).then_some(words)
}

pub fn on_command(game: &mut crate::game::Game<'_>, command: &str, ran: &mut bool) {
    if !game.native_on("svchorizon") {
        return;
    }
    let Some(words) = command_words(command) else {
        return;
    };
    if words.first().map(String::as_str) != Some("svchorizon") {
        return;
    }
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
            let names = state
                .groups
                .iter()
                .filter(|g| !g.hidden)
                .map(|g| {
                    if g.has_password {
                        format!("{} [locked]", g.name)
                    } else {
                        g.name.clone()
                    }
                })
                .collect::<Vec<_>>();
            let message = if names.is_empty() {
                "SVC: No visible groups.".to_owned()
            } else {
                format!("SVC groups: {}", names.join(", "))
            };
            drop(state);
            game.native_chat(&message);
        }
        Some("join") => {
            let selected = {
                let state = group_state().lock().expect("SVC group state poisoned");
                state
                    .groups
                    .get(state.selected)
                    .filter(|g| !g.hidden)
                    .cloned()
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
                game.native_chat(
                    "Usage: /svchorizon group create <name> [password] [normal|open|isolated]",
                );
                return;
            };
            let password = words
                .get(4)
                .filter(|p| p.as_str() != "-")
                .map(String::as_str);
            let kind = words.get(5).map(String::as_str).unwrap_or("normal");
            let group_type = match kind {
                "normal" => 0,
                "open" => 1,
                "isolated" => 2,
                _ => {
                    game.native_chat("SVC: Group type must be normal, open, or isolated.");
                    return;
                }
            };
            let invalid_name = name
                .chars()
                .next()
                .is_none_or(|c| c.is_control() || c.is_whitespace())
                || name.chars().any(|c| c.is_control());
            // Upstream's CreateGroupPacket writes both name and password with
            // MAX_GROUP_NAME_LENGTH (24); JoinGroupPacket allows 256. A create
            // password above 24 would abort the official client's write.
            if invalid_name
                || name.chars().count() > 24
                || password.is_some_and(|p| p.chars().count() > 24)
            {
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
            let Some(wrong_password) = cursor.bool() else {
                return;
            };
            if !cursor.done() {
                return;
            }
            state.active = group_id.filter(|id| *id != [0; 16]);
            state.wrong_password = wrong_password;
        }
        "voicechat:add_group" => {
            let Some(id) = cursor.uuid() else {
                return;
            };
            let Some(name) = cursor.string(24) else {
                return;
            };
            let Some(has_password) = cursor.bool() else {
                return;
            };
            let Some(persistent) = cursor.bool() else {
                return;
            };
            let Some(hidden) = cursor.bool() else {
                return;
            };
            let Some(group_type) = cursor.i16() else {
                return;
            };
            if !cursor.done() {
                return;
            }
            let group = NativeGroup {
                id,
                name,
                has_password,
                persistent,
                hidden,
                group_type,
            };
            if let Some(existing) = state.groups.iter_mut().find(|entry| entry.id == id) {
                *existing = group;
            } else if state.groups.len() < 128 {
                state.groups.push(group);
                if state.groups.len() == 1 {
                    state.selected = 0;
                }
            }
            if !state.groups.is_empty() {
                state.selected %= state.groups.len();
            }
        }
        "voicechat:remove_group" => {
            let Some(id) = cursor.uuid() else {
                return;
            };
            if !cursor.done() {
                return;
            }
            state.groups.retain(|group| group.id != id);
            if state.groups.is_empty() {
                state.selected = 0;
            } else {
                state.selected %= state.groups.len();
            }
            if state.active == Some(id) {
                state.active = None;
            }
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
    Some(NativeCategory {
        id,
        name,
        description,
        tab,
        tab_name,
    })
}

fn on_category_payload(channel: &str, bytes: &[u8]) {
    let mut state = group_state().lock().expect("SVC category state poisoned");
    let mut cursor = Cursor::new(bytes);
    match channel {
        "voicechat:add_category" => {
            let Some(category) = decode_category(&mut cursor) else {
                return;
            };
            if !cursor.done() {
                return;
            }
            if let Some(existing) = state
                .categories
                .iter_mut()
                .find(|entry| entry.id == category.id)
            {
                *existing = category;
            } else if state.categories.len() < 64 {
                state.categories.push(category);
            }
        }
        "voicechat:remove_category" => {
            let Some(id) = cursor.string(16) else {
                return;
            };
            if !cursor.done() {
                return;
            }
            state.categories.retain(|category| category.id != id);
        }
        _ => {}
    }
}

fn decode_player_state(cursor: &mut Cursor<'_>) -> Option<NativePlayer> {
    let flags = cursor.u8()?;
    let id = cursor.uuid()?;
    let name = cursor.string(256)?;
    let group = if flags & (1 << 5) != 0 {
        Some(cursor.uuid()?)
    } else {
        None
    };
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
    let local_id = *local_player_uuid()
        .lock()
        .expect("SVC local player state poisoned");
    let mut state = group_state().lock().expect("SVC player state poisoned");
    let mut reset_players = Vec::new();
    match channel {
        "voicechat:states" => {
            let mut cursor = Cursor::new(bytes);
            let Some(count) = cursor.i32().and_then(|n| usize::try_from(n).ok()) else {
                return;
            };
            if count > 512 {
                return;
            }
            let old = state.players.clone();
            let mut players = Vec::with_capacity(count.min(128));
            for _ in 0..count {
                let Some(mut player) = decode_player_state(&mut cursor) else {
                    return;
                };
                if Some(player.id) == local_id {
                    continue;
                }
                preserve_player_controls(&mut player, &old);
                if players.len() < 128
                    && !players
                        .iter()
                        .any(|entry: &NativePlayer| entry.id == player.id)
                {
                    players.push(player);
                }
            }
            if !cursor.done() {
                return;
            }
            reset_players = old
                .iter()
                .filter(|previous| !players.iter().any(|player| player.id == previous.id))
                .map(|previous| previous.id)
                .collect();
            state.players = players;
            if state.players.is_empty() {
                state.selected_player = 0;
            } else {
                state.selected_player %= state.players.len();
            }
        }
        "voicechat:state" => {
            let mut cursor = Cursor::new(bytes);
            let Some(mut player) = decode_player_state(&mut cursor) else {
                return;
            };
            if !cursor.done() {
                return;
            }
            if Some(player.id) == local_id {
                return;
            }
            preserve_player_controls(&mut player, &state.players);
            if let Some(existing) = state.players.iter_mut().find(|entry| entry.id == player.id) {
                *existing = player;
            } else if state.players.len() < 128 {
                state.players.push(player);
            }
        }
        "voicechat:remove_state" => {
            let mut cursor = Cursor::new(bytes);
            let Some(id) = cursor.uuid() else {
                return;
            };
            if !cursor.done() {
                return;
            }
            state.players.retain(|player| player.id != id);
            if state.players.is_empty() {
                state.selected_player = 0;
            } else {
                state.selected_player %= state.players.len();
            }
            drop(state);
            if let Some(client) = voice_client()
                .lock()
                .expect("SVC client state poisoned")
                .as_ref()
            {
                client.set_player_volume(id, 1.0);
                client.set_player_muted(id, false);
            }
            return;
        }
        _ => return,
    }
    let controls = state
        .players
        .iter()
        .map(|p| (p.id, p.volume, p.ignored))
        .collect::<Vec<_>>();
    drop(state);
    if let Some(client) = voice_client()
        .lock()
        .expect("SVC client state poisoned")
        .as_ref()
    {
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
                VoiceEvent::Connected => {
                    eprintln!("[svchorizon] Voice UDP authenticated and validated.")
                }
                VoiceEvent::Disconnected(reason) => {
                    eprintln!("[svchorizon] Voice disconnected: {reason}");
                    disconnected = true;
                }
                VoiceEvent::AudioUnavailable(reason) => {
                    eprintln!("[svchorizon] Audio device warning: {reason}")
                }
            }
        }
    }
    if disconnected {
        guard.take();
        PTT_DOWN.store(false, Ordering::Relaxed);
    }
    drop(guard);
    if disconnected {
        start_reconnect();
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
        let old_client = voice_client()
            .lock()
            .expect("SVC client state poisoned")
            .take();
        drop(old_client);
        *group_state().lock().expect("SVC group state poisoned") = GroupState::default();
        *local_player_uuid()
            .lock()
            .expect("SVC local player state poisoned") = None;
        *last_server_info()
            .lock()
            .expect("SVC reconnect state poisoned") = None;
        RECONNECTING.store(false, Ordering::Release);
        GROUPS_ENABLED.store(false, Ordering::Release);
        reset_probe_state();
        return;
    }
    REMOTE_ACTIVE.store(true, Ordering::Release);

    // Serial generation probe: ask about one compatibility number at a
    // time and move on after a timeout or the server's rejection notice
    // (handled in the SystemChat arm). This repeats while no secret has
    // arrived at all, so a dropped first request (channel registration
    // race) recovers without a manual rejoin.
    if game.native_on("svchorizon")
        && voice_client()
            .lock()
            .expect("SVC client state poisoned")
            .is_none()
        && last_server_info()
            .lock()
            .expect("SVC reconnect state poisoned")
            .is_none()
    {
        let now = now_millis();
        let pending = PENDING_PROBE.load(Ordering::Acquire);
        let pending_expired = pending != 0
            && now.saturating_sub(PENDING_AT_MS.load(Ordering::Acquire)) >= PROBE_TIMEOUT_MS;
        if pending_expired {
            PENDING_PROBE.store(0, Ordering::Release);
        }
        // Once the quick detection window is over, slow the background
        // probing down: a server that was going to answer has answered.
        let pacing = if now.saturating_sub(FIRST_PROBE_MS.load(Ordering::Acquire))
            >= NO_VOICE_CHAT_AFTER_MS
        {
            10_000
        } else {
            0
        };
        if (pending == 0 || pending_expired)
            && now.saturating_sub(PENDING_AT_MS.load(Ordering::Acquire)) >= pacing
            && next_probe_generation().is_some()
        {
            send_secret_request(game);
        }
    }

    // A failed initial connect (bad endpoint, refused socket) must not leave
    // the session stuck with no client: retry through the reconnect path.
    if voice_client()
        .lock()
        .expect("SVC client state poisoned")
        .is_none()
        && !RECONNECTING.load(Ordering::Acquire)
        && last_server_info()
            .lock()
            .expect("SVC reconnect state poisoned")
            .is_some()
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
    let positions = game
        .remote
        .as_ref()
        .map(crate::game::remote::svchorizon_voice::player_positions)
        .unwrap_or_default();

    let guard = voice_client().lock().expect("SVC client state poisoned");
    let Some(client) = guard.as_ref() else {
        return;
    };

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
        if let Some(client) = voice_client()
            .lock()
            .expect("SVC client state poisoned")
            .as_ref()
        {
            client.set_transmitting(transmitting && !SELF_MUTED.load(Ordering::Relaxed));
        }
    } else if key == whisper_binding {
        let whispering = pressed && game.native_playing();
        WHISPER_DOWN.store(whispering, Ordering::Relaxed);
        if let Some(client) = voice_client()
            .lock()
            .expect("SVC client state poisoned")
            .as_ref()
        {
            client.set_whispering(whispering);
        }
    } else if key == game.native_mod_key("svchorizon", "voice_activation", "key.keyboard.c") {
        if pressed && !VOICE_ACTIVATION_KEY_DOWN.swap(true, Ordering::AcqRel) {
            let enabled = !VOICE_ACTIVATION.load(Ordering::Relaxed);
            VOICE_ACTIVATION.store(enabled, Ordering::Relaxed);
            if let Some(client) = voice_client()
                .lock()
                .expect("SVC client state poisoned")
                .as_ref()
            {
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
            if let Some(client) = voice_client()
                .lock()
                .expect("SVC client state poisoned")
                .as_ref()
            {
                client.set_transmitting(
                    PTT_DOWN.load(Ordering::Relaxed) && !muted && game.native_playing(),
                );
                client.set_voice_activation(
                    VOICE_ACTIVATION.load(Ordering::Relaxed) && !muted,
                    0.025,
                );
            }
        } else if !pressed {
            SELF_MUTE_KEY_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_create", "key.keyboard.g") {
        if pressed
            && !GROUP_CREATE_DOWN.swap(true, Ordering::AcqRel)
            && game.native_playing()
            && GROUPS_ENABLED.load(Ordering::Acquire)
        {
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
                    if !state.groups[state.selected].hidden {
                        break;
                    }
                }
            }
            state.wrong_password = false;
        } else if !pressed {
            GROUP_NEXT_DOWN.store(false, Ordering::Release);
        }
    } else if key == game.native_mod_key("svchorizon", "group_join", "key.keyboard.j") {
        if pressed
            && !GROUP_JOIN_DOWN.swap(true, Ordering::AcqRel)
            && game.native_playing()
            && GROUPS_ENABLED.load(Ordering::Acquire)
        {
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
        if pressed
            && !GROUP_LEAVE_DOWN.swap(true, Ordering::AcqRel)
            && game.native_playing()
            && GROUPS_ENABLED.load(Ordering::Acquire)
        {
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
                if let Some(client) = voice_client()
                    .lock()
                    .expect("SVC client state poisoned")
                    .as_ref()
                {
                    client.set_player_muted(id, ignored);
                }
            }
        } else if !pressed {
            PLAYER_IGNORE_DOWN.store(false, Ordering::Release);
        }
    } else if key
        == game.native_mod_key("svchorizon", "player_volume_down", "key.keyboard.page.down")
    {
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
                if let Some(client) = voice_client()
                    .lock()
                    .expect("SVC client state poisoned")
                    .as_ref()
                {
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
                if let Some(client) = voice_client()
                    .lock()
                    .expect("SVC client state poisoned")
                    .as_ref()
                {
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
                let selection = if slot == devices.len() {
                    None
                } else {
                    Some(devices[slot].clone())
                };
                audio_device_preferences()
                    .lock()
                    .expect("SVC audio preferences poisoned")
                    .0 = selection.clone();
                if let Some(client) = voice_client()
                    .lock()
                    .expect("SVC client state poisoned")
                    .as_ref()
                {
                    client.set_input_device(selection.clone());
                }
                eprintln!(
                    "[svchorizon] Microphone: {}",
                    selection.as_deref().unwrap_or("OS default")
                );
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
                let selection = if slot == devices.len() {
                    None
                } else {
                    Some(devices[slot].clone())
                };
                audio_device_preferences()
                    .lock()
                    .expect("SVC audio preferences poisoned")
                    .1 = selection.clone();
                if let Some(client) = voice_client()
                    .lock()
                    .expect("SVC client state poisoned")
                    .as_ref()
                {
                    client.set_output_device(selection.clone());
                }
                eprintln!(
                    "[svchorizon] Speaker: {}",
                    selection.as_deref().unwrap_or("OS default")
                );
            }
        } else if !pressed {
            OUTPUT_DEVICE_NEXT_DOWN.store(false, Ordering::Release);
        }
    }
}

pub fn on_hud(game: &crate::game::Game<'_>, batch: &mut horizon_client::hud::HudBatch) {
    if !game.native_playing() || !game.native_on("svchorizon") {
        return;
    }
    let guard = voice_client().lock().expect("SVC client state poisoned");
    let (label, color) = if SELF_MUTED.load(Ordering::Relaxed) {
        ("SVC: Microphone muted", 0xFF5555)
    } else if let Some(client) = guard.as_ref() {
        if !client.is_connected() && client.unanswered_auth_attempts() >= 15 {
            // Packets are going out but no authenticated datagram is coming
            // back. This is neither proof of a blocked server nor of a
            // blocked client: it means no UDP answer arrived yet.
            ("SVC: Voice server not responding", 0xFF5555)
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
    } else if all_generations_rejected() {
        // Every generation this client speaks was answered with the
        // server's incompatibility notice: the server runs Simple Voice
        // Chat, but a wire generation this client cannot speak.
        ("SVC: Server voice chat version incompatible", 0xFF5555)
    } else if last_server_info()
        .lock()
        .expect("SVC reconnect state poisoned")
        .is_some()
    {
        ("SVC: Connecting", 0xFFCC55)
    } else {
        let first_probe = FIRST_PROBE_MS.load(Ordering::Acquire);
        let detecting =
            first_probe == 0 || now_millis().saturating_sub(first_probe) < NO_VOICE_CHAT_AFTER_MS;
        if detecting {
            ("SVC: Detecting voice chat", 0xFFCC55)
        } else {
            ("SVC: No voice chat on this server", 0xAAAAAA)
        }
    };
    game.native_text(batch, label, [8.0, 42.0], color, true);
    let groups = group_state().lock().expect("SVC group state poisoned");
    let group_label = if !GROUPS_ENABLED.load(Ordering::Acquire) {
        "SVC Group: disabled by server".to_owned()
    } else if let Some(active) = groups.active {
        groups
            .groups
            .iter()
            .find(|group| group.id == active)
            .map(|group| format!("SVC Group: {}", group.name))
            .unwrap_or_else(|| "SVC Group: joined".to_owned())
    } else if groups.wrong_password {
        "SVC Group: password required | /svchorizon group join <password>".to_owned()
    } else if let Some(group) = groups
        .groups
        .get(groups.selected)
        .filter(|group| !group.hidden)
    {
        let lock = if group.has_password { " [locked]" } else { "" };
        let persistent = if group.persistent {
            " [persistent]"
        } else {
            ""
        };
        let kind = match group.group_type {
            1 => "open",
            2 => "isolated",
            _ => "normal",
        };
        format!(
            "SVC Group: {}{}{} | {} ({}/{})",
            group.name,
            lock,
            persistent,
            kind,
            groups.selected + 1,
            groups.groups.len()
        )
    } else {
        "SVC Group: no visible groups | G create, N select, J join, H leave".to_owned()
    };
    game.native_text(batch, &group_label, [8.0, 54.0], 0xFFFFFF, true);
    let player_label = if let Some(player) = groups.players.get(groups.selected_player) {
        let status = if player.ignored {
            "ignored"
        } else if player.force_muted || player.server_muted {
            "server-muted"
        } else if player.force_disabled || player.disabled {
            "voice-disabled"
        } else if player.disconnected {
            "disconnected"
        } else {
            "active"
        };
        format!(
            "SVC Player: {} | {} | {:.0}%",
            player.name,
            status,
            player.volume * 100.0
        )
    } else {
        "SVC Player: none | P select, O ignore, PgUp/PgDn volume".to_owned()
    };
    game.native_text(batch, &player_label, [8.0, 66.0], 0xFFFFFF, true);
    let category_label = if groups.categories.is_empty() {
        "SVC Categories: none".to_owned()
    } else {
        let names = groups
            .categories
            .iter()
            .take(3)
            .map(|category| {
                let tab = category.tab_name.as_deref().or(category.tab.as_deref());
                let description = category
                    .description
                    .as_deref()
                    .map(|value| value.chars().take(36).collect::<String>());
                match (tab, description) {
                    (Some(tab), Some(description)) => {
                        format!("{} [{}: {}]", category.name, tab, description)
                    }
                    (Some(tab), None) => format!("{} [{}]", category.name, tab),
                    (None, Some(description)) => format!("{}: {}", category.name, description),
                    (None, None) => category.name.clone(),
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let extra = groups.categories.len().saturating_sub(3);
        if extra > 0 {
            format!("SVC Categories: {names} +{extra}")
        } else {
            format!("SVC Categories: {names}")
        }
    };
    game.native_text(batch, &category_label, [8.0, 78.0], 0xAAAAFF, true);
    if let Some(client) = guard.as_ref() {
        let (input, output) = client.selected_audio_devices();
        game.native_text(
            batch,
            &format!(
                "SVC Audio: Mic {} | Out {}",
                input.as_deref().unwrap_or("OS default"),
                output.as_deref().unwrap_or("OS default")
            ),
            [8.0, 90.0],
            0xAAAAAA,
            true,
        );
    }
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
            voice_client()
                .lock()
                .expect("SVC client state poisoned")
                .take();
            *group_state().lock().expect("SVC group state poisoned") = GroupState::default();
            *local_player_uuid()
                .lock()
                .expect("SVC local player state poisoned") = None;
            *last_server_info()
                .lock()
                .expect("SVC reconnect state poisoned") = None;
            RECONNECTING.store(false, Ordering::Release);
            GROUPS_ENABLED.store(false, Ordering::Release);
            PTT_DOWN.store(false, Ordering::Relaxed);
            WHISPER_DOWN.store(false, Ordering::Relaxed);
            if game.native_on("svchorizon") {
                // Bukkit/Paper only deliver payloads the client registered.
                // The probe state is per server: a different server may run
                // another SVC generation, so start the probe round from the
                // beginning on every join.
                reset_probe_state();
                send_channel_registration(game);
                send_secret_request(game);
            }
        }
        Clientbound::CustomPayload(payload)
            if payload.channel == SECRET && game.native_on("svchorizon") =>
        {
            // Decode expecting the layout of the generation this login is
            // currently probing, falling back to the other layouts.
            match decode_secret_for_generation(
                &payload.data,
                PENDING_PROBE.load(Ordering::Acquire),
            ) {
                Some(info) => {
                    PENDING_PROBE.store(0, Ordering::Release);
                    eprintln!(
                        "[svchorizon] Secret received ({}): voice host '{}' port {} keepalive {}ms groups {}",
                        info.profile.label(),
                        if info.host.is_empty() {
                            "<same as game server>"
                        } else {
                            info.host.as_str()
                        },
                        info.port,
                        info.keep_alive_ms,
                        info.groups
                    );
                    *local_player_uuid()
                        .lock()
                        .expect("SVC local player state poisoned") = Some(info.player);
                    GROUPS_ENABLED.store(info.groups, Ordering::Release);
                    send_client_state(game, SELF_MUTED.load(Ordering::Relaxed), false);
                    {
                        let mut state = group_state().lock().expect("SVC player state poisoned");
                        state.players.retain(|player| player.id != info.player);
                        if state.players.is_empty() {
                            state.selected_player = 0;
                        } else {
                            state.selected_player %= state.players.len();
                        }
                    }
                    let fallback_host = game
                        .remote
                        .as_ref()
                        .map(|remote| remote.session.address.clone())
                        .unwrap_or_default();
                    eprintln!("[svchorizon] Game address: '{fallback_host}'");
                    *last_server_info()
                        .lock()
                        .expect("SVC reconnect state poisoned") =
                        Some((info.clone(), fallback_host.clone()));
                    let generation = SESSION_GENERATION.load(Ordering::Acquire);
                    let _ = thread::Builder::new()
                        .name("svchorizon-connect".into())
                        .spawn(move || match VoiceClient::connect(info, &fallback_host) {
                            Ok(client) => {
                                apply_player_controls(&client);
                                apply_audio_device_preferences(&client);
                                if SESSION_GENERATION.load(Ordering::Acquire) == generation {
                                    *voice_client().lock().expect("SVC client state poisoned") =
                                        Some(client);
                                    eprintln!("[svchorizon] Voice UDP/audio workers started.");
                                }
                            }
                            Err(error) => {
                                eprintln!("[svchorizon] Could not start voice connection: {error}")
                            }
                        });
                }
                None => {
                    eprintln!(
                        "[svchorizon] Rejected malformed voicechat:secret payload ({} bytes, head {:02x?}).",
                        payload.data.len(),
                        &payload.data[..payload.data.len().min(8)]
                    );
                }
            }
        }
        Clientbound::SystemChat(payload)
            if !payload.overlay && game.native_on("svchorizon") =>
        {
            // The official plugin answers a request_secret whose
            // compatibility number does not match its build with this chat
            // notice instead of a secret
            // (ServerVoiceEvents.getIncompatibleMessage). Horizon has no
            // Simple Voice Chat language table, so the translatable key
            // arrives verbatim; use it as a definitive rejection signal for
            // the pending generation and move on to the next one right away.
            if PENDING_PROBE.load(Ordering::Acquire) != 0
                && horizon_client::scoreboard::tag_to_json(&payload.content)
                    .to_string()
                    .contains("message.voicechat.incompatible_version")
            {
                reject_pending_generation();
                if next_probe_generation().is_some() {
                    send_secret_request(game);
                }
            }
        }
        Clientbound::CustomPayload(payload)
            if matches!(
                payload.channel.as_str(),
                "voicechat:joined_group" | "voicechat:add_group" | "voicechat:remove_group"
            ) && game.native_on("svchorizon") =>
        {
            on_group_payload(&payload.channel, &payload.data);
        }
        Clientbound::CustomPayload(payload)
            if matches!(
                payload.channel.as_str(),
                "voicechat:states" | "voicechat:state" | "voicechat:remove_state"
            ) && game.native_on("svchorizon") =>
        {
            on_player_payload(&payload.channel, &payload.data);
        }
        Clientbound::CustomPayload(payload)
            if matches!(
                payload.channel.as_str(),
                "voicechat:add_category" | "voicechat:remove_category"
            ) && game.native_on("svchorizon") =>
        {
            on_category_payload(&payload.channel, &payload.data);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use svchorizon_engine::voice::SVC_PROFILES;

    #[test]
    fn decodes_official_category_metadata_and_optional_fields() {
        let mut bytes = Vec::new();
        write_utf(&mut bytes, "music");
        write_utf(&mut bytes, "Music");
        bytes.push(0); // no name translation key
        bytes.push(1);
        write_utf(&mut bytes, "Background music");
        bytes.push(0); // no description translation key
        bytes.push(0); // no icon
        bytes.push(1);
        write_utf(&mut bytes, "media");
        bytes.push(1);
        write_utf(&mut bytes, "Media");
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
        let words =
            command_words(r#"svchorizon group create "Friends Group" "a password" open"#).unwrap();
        assert_eq!(
            words,
            vec![
                "svchorizon",
                "group",
                "create",
                "Friends Group",
                "a password",
                "open"
            ]
        );
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
        let channels: Vec<&str> = data
            .split(|byte| *byte == 0)
            .map(|channel| std::str::from_utf8(channel).unwrap())
            .collect();
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
    fn decodes_secret_payload_in_2_6_x_field_order() {
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
        let info = decode_secret_for_generation(&bytes, 0).unwrap();
        assert_eq!(info.profile, SvcProfile::Compat20);
        assert_eq!(info.profile.compatibility_version(), 20);
        assert_eq!(info.player, [0x11; 16]);
        assert_eq!(info.connection_id, [0; 16]);
        assert_eq!(info.port, 24454);
        assert_eq!(info.mtu, 1200);
        assert_eq!(info.distance, 48.0);
        assert_eq!(info.host, "voice.example.test");
        assert!(info.groups);
    }

    #[test]
    fn decodes_secret_payload_in_2_7_x_field_order() {
        // Compat-21 (Simple Voice Chat 2.7.x) inserts the server-assigned
        // connection UUID right after the secret; everything after the port
        // matches the 2.6.x layout.
        let mut bytes = vec![0x42; 16];
        bytes.extend_from_slice(&[0x77; 16]); // connectionId
        bytes.extend_from_slice(&24454i32.to_be_bytes());
        bytes.extend_from_slice(&[0x11; 16]);
        bytes.push(1);
        bytes.extend_from_slice(&1200i32.to_be_bytes());
        bytes.extend_from_slice(&48.0f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(&1000i32.to_be_bytes());
        bytes.push(1);
        bytes.push(0); // empty voice host
        bytes.push(0);
        let info = decode_secret_for_generation(&bytes, 0).unwrap();
        assert_eq!(info.profile, SvcProfile::Compat21);
        assert_eq!(info.profile.compatibility_version(), 21);
        assert_eq!(info.connection_id, [0x77; 16]);
        assert_eq!(info.player, [0x11; 16]);
        assert_eq!(info.port, 24454);
        assert_eq!(info.host, "");
    }

    #[test]
    fn rejects_truncated_or_trailing_payloads() {
        assert!(decode_secret_for_generation(&[0; 8], 0).is_none());
        let mut bytes = vec![0x42; 16];
        bytes.extend_from_slice(&24454i32.to_be_bytes());
        bytes.extend_from_slice(&[0x11; 16]);
        bytes.push(0);
        bytes.extend_from_slice(&1200i32.to_be_bytes());
        bytes.extend_from_slice(&48.0f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(&1000i32.to_be_bytes());
        bytes.push(0);
        bytes.push(0);
        bytes.push(0);
        bytes.push(99);
        assert!(decode_secret_for_generation(&bytes, 0).is_none());
        // The same trailing-garbage rejection for the 2.7.x layout.
        let mut bytes21 = vec![0x42; 16];
        bytes21.extend_from_slice(&[0x77; 16]);
        bytes21.extend_from_slice(&24454i32.to_be_bytes());
        bytes21.extend_from_slice(&[0x11; 16]);
        bytes21.push(0);
        bytes21.extend_from_slice(&1200i32.to_be_bytes());
        bytes21.extend_from_slice(&48.0f64.to_bits().to_be_bytes());
        bytes21.extend_from_slice(&1000i32.to_be_bytes());
        bytes21.push(0);
        bytes21.push(0);
        bytes21.push(0);
        bytes21.push(99);
        assert!(decode_secret_for_generation(&bytes21, 0).is_none());
    }

    #[test]
    fn probes_every_supported_compatibility_generation() {
        // The request payload is exactly the 4-byte big-endian compatibility
        // number, and the generations are probed one at a time, most likely
        // first: the released 2.6.x line, then the 2.7.x pre-release, then
        // the 2.5.x line on servers that never updated.
        assert_eq!(20i32.to_be_bytes(), [0, 0, 0, 20]);
        assert_eq!(21i32.to_be_bytes(), [0, 0, 0, 21]);
        assert_eq!(18i32.to_be_bytes(), [0, 0, 0, 18]);
        assert_eq!(
            SVC_PROFILES
                .iter()
                .map(|p| p.compatibility_version())
                .collect::<Vec<_>>(),
            vec![20, 21, 18]
        );
    }

    #[test]
    fn rejection_notices_remove_a_generation_from_the_probe_round() {
        // Walk the probe state the way the packet handlers drive it: 20 is
        // rejected by the incompatibility notice, then 21, then a compat-18
        // server answers with a secret.
        reset_probe_state();
        assert_eq!(next_probe_generation(), Some(20));
        PENDING_PROBE.store(20, Ordering::Release);
        LAST_PROBED.store(20, Ordering::Release);
        reject_pending_generation();
        assert_eq!(next_probe_generation(), Some(21));
        PENDING_PROBE.store(21, Ordering::Release);
        LAST_PROBED.store(21, Ordering::Release);
        reject_pending_generation();
        assert_eq!(next_probe_generation(), Some(18));
        // A late notice after the timeout is attributed to the last probe.
        PENDING_PROBE.store(0, Ordering::Release);
        LAST_PROBED.store(18, Ordering::Release);
        reject_pending_generation();
        assert_eq!(next_probe_generation(), None);
        assert!(all_generations_rejected());
        // And a fresh login starts the round from the beginning again.
        reset_probe_state();
        assert_eq!(next_probe_generation(), Some(20));
        assert!(!all_generations_rejected());
    }

    #[test]
    fn decodes_secret_payload_for_a_compat_18_server() {
        // 2.5.x (compatibility 18) sends the same field order as 2.6.x; the
        // expected-generation hint only decides which profile is recorded.
        let mut bytes = vec![0x42; 16];
        bytes.extend_from_slice(&24454i32.to_be_bytes());
        bytes.extend_from_slice(&[0x11; 16]);
        bytes.push(0);
        bytes.extend_from_slice(&1200i32.to_be_bytes());
        bytes.extend_from_slice(&48.0f64.to_bits().to_be_bytes());
        bytes.extend_from_slice(&1000i32.to_be_bytes());
        bytes.push(1);
        bytes.push(0); // empty voice host
        bytes.push(0);
        let info = decode_secret_for_generation(&bytes, 18).unwrap();
        assert_eq!(info.profile, SvcProfile::Compat18);
        assert_eq!(info.profile.compatibility_version(), 18);
        assert_eq!(info.player, [0x11; 16]);
        assert_eq!(info.port, 24454);
        // Without a hint the shared layout still decodes (recorded as the
        // compat-20 generation), and a compat-21 hint falls back to it too.
        assert_eq!(
            decode_secret_for_generation(&bytes, 0).unwrap().profile,
            SvcProfile::Compat20
        );
        assert_eq!(
            decode_secret_for_generation(&bytes, 21).unwrap().profile,
            SvcProfile::Compat20
        );
    }
}
