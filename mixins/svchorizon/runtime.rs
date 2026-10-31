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
    fn i32(&mut self) -> Option<i32> { Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?)) }
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
        return;
    }
    REMOTE_ACTIVE.store(true, Ordering::Release);

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
    client.set_transmitting(PTT_DOWN.load(Ordering::Relaxed) && !muted && game.native_playing());
    client.set_whispering(WHISPER_DOWN.load(Ordering::Relaxed) && game.native_playing());
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
