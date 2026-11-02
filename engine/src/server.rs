//! The Simple Voice Chat **server** protocol, for worlds Horizon itself
//! hosts (singleplayer's integrated server and Open-to-LAN). It speaks the
//! unmodified official wire (compatibility version 20, as shipped by the
//! `voicechat-bukkit-2.6.27` plugin), so official Fabric/Forge/NeoForge
//! clients and other Horizon builds can talk to a Horizon-hosted world
//! without any server-side plugin.
//!
//! Behaviour mirrors upstream `common/voice/server/Server.java` at the
//! 2.6.x wire: per-player secrets, source-address-bound connections,
//! keepalive with 10x timeout, ping echo, and microphone routing into
//! player/group/location sound packets by distance, whispering and group
//! membership. The Minecraft-side payloads (`voicechat:*` custom payloads)
//! are produced and consumed here; the mod's hub hook delivers them.

use crate::voice::{
    decode_packet_prefix, encode_packet, write_byte_array, write_varint, Packet, Reader,
    KEEP_ALIVE, MAGIC, MAX_DATAGRAM, MAX_OPUS, NONCE_BYTES, TAG_BYTES,
};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SVC_COMPATIBILITY_VERSION: i32 = 20;

/// How the hosted server is configured (upstream's server config defaults
/// for the values a dedicated server reads from its config file).
#[derive(Clone, Debug)]
pub struct HostedVoiceConfig {
    /// `voicechat_distance` (default 48).
    pub distance: f64,
    /// `whisper_distance` (default 16).
    pub whisper_distance: f64,
    /// `keep_alive` in milliseconds (default 1000).
    pub keepalive_ms: u64,
    /// MTU advertised in the secret payload (default 1024).
    pub mtu: i32,
    /// Whether groups are enabled (default true).
    pub groups: bool,
    /// Whether clients may record (default true).
    pub allow_recording: bool,
}

impl Default for HostedVoiceConfig {
    fn default() -> Self {
        Self {
            distance: 48.0,
            whisper_distance: 16.0,
            keepalive_ms: 1000,
            mtu: 1024,
            groups: true,
            allow_recording: true,
        }
    }
}

/// One player as the hub sees them, refreshed every server tick.
#[derive(Clone, Debug)]
pub struct RosterEntry {
    pub uuid: [u8; 16],
    pub name: String,
    pub position: [f64; 3],
    pub dimension: String,
    pub spectator: bool,
}

/// A group as clients created it (`Group` upstream).
#[derive(Clone, Debug)]
struct ServerGroup {
    id: [u8; 16],
    name: String,
    password: Option<String>,
    persistent: bool,
    group_type: i16,
}

/// The server's knowledge of one player's voice state (`PlayerState`).
#[derive(Clone, Debug, Default)]
struct PlayerState {
    name: String,
    muted: bool,
    disabled: bool,
    disconnected: bool,
    group: Option<[u8; 16]>,
}

/// A UDP voice connection (`ClientConnection`): bound to the first address
/// that authenticated, exactly like upstream.
#[derive(Clone)]
struct ClientConnection {
    uuid: [u8; 16],
    address: SocketAddr,
    last_keepalive: Instant,
}

#[derive(Default)]
struct Core {
    config: HostedVoiceConfig,
    socket: Option<UdpSocket>,
    /// player UUID -> 16-byte secret (`Server.secrets`).
    secrets: HashMap<[u8; 16], [u8; 16]>,
    /// Validated connections.
    connections: HashMap<[u8; 16], ClientConnection>,
    /// Authenticated but not yet connection-checked.
    unchecked: HashMap<[u8; 16], ClientConnection>,
    groups: HashMap<[u8; 16], ServerGroup>,
    states: HashMap<[u8; 16], PlayerState>,
    roster: HashMap<[u8; 16], RosterEntry>,
    /// (player uuid, channel, data) replies for the hub to deliver.
    outbound: VecDeque<([u8; 16], String, Vec<u8>)>,
    last_keepalive: Option<Instant>,
    last_feed: Option<Instant>,
    datagrams_received: u64,
    datagrams_sent: u64,
}

impl Core {
    fn keepalive_due(&self) -> bool {
        self.last_keepalive
            .is_none_or(|at| at.elapsed() >= Duration::from_millis(self.config.keepalive_ms))
    }
}

impl Core {
    /// Encrypts and sends one packet as the server would.
    fn send_udp(&mut self, address: SocketAddr, packet: &Packet, secret: &[u8; 16]) {
        let Some(plain) = encode_packet(packet) else { return };
        self.send_udp_raw(address, &plain, secret);
    }

    fn send_udp_raw(&mut self, address: SocketAddr, plain: &[u8], secret: &[u8; 16]) {
        let Some(socket) = &self.socket else { return };
        let mut nonce = [0u8; NONCE_BYTES];
        if SystemRandom::new().fill(&mut nonce).is_err() {
            return;
        }
        let Ok(key) = UnboundKey::new(&AES_128_GCM, secret) else { return };
        let key = LessSafeKey::new(key);
        let mut encrypted = plain.to_vec();
        if key
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut encrypted)
            .is_err()
        {
            return;
        }
        // Server datagrams: magic, then the VarInt-prefixed IV || ciphertext.
        let mut body = Vec::with_capacity(NONCE_BYTES + encrypted.len());
        body.extend_from_slice(&nonce);
        body.extend_from_slice(&encrypted);
        let mut datagram = vec![MAGIC];
        write_byte_array(&mut datagram, &body);
        if socket.send_to(&datagram, address).is_ok() {
            self.datagrams_sent += 1;
        }
    }

    fn queue_payload(&mut self, player: [u8; 16], channel: &str, data: Vec<u8>) {
        if self.outbound.len() < 4096 {
            self.outbound.push_back((player, channel.to_owned(), data));
        }
    }

    /// The players to tell about state changes: everyone holding a secret.
    fn voice_players(&self) -> Vec<[u8; 16]> {
        self.secrets.keys().copied().collect()
    }

    /// `SecretPacket.toBytes` for compatibility 20: secret, port, player,
    /// codec, mtu, distance, keepalive, groups, host, recording.
    fn secret_payload(&self, player: [u8; 16]) -> Option<Vec<u8>> {
        let secret = self.secrets.get(&player)?;
        let port = self.socket.as_ref()?.local_addr().ok()?.port();
        let mut data = Vec::new();
        data.extend_from_slice(secret);
        data.extend_from_slice(&(port as i32).to_be_bytes());
        data.extend_from_slice(&player);
        data.push(0); // codec: Opus
        data.extend_from_slice(&self.config.mtu.to_be_bytes());
        data.extend_from_slice(&self.config.distance.to_bits().to_be_bytes());
        data.extend_from_slice(&(self.config.keepalive_ms as i32).to_be_bytes());
        data.push(u8::from(self.config.groups));
        write_utf(&mut data, "");
        data.push(u8::from(self.config.allow_recording));
        Some(data)
    }

    /// `PlayerStatePacket.toBytes`: flags byte, UUID, name, optional group.
    fn state_payload(&self, state: &PlayerState, uuid: [u8; 16]) -> Vec<u8> {
        let mut flags = 0u8;
        flags |= u8::from(state.muted);
        flags |= u8::from(state.disabled) << 2;
        flags |= u8::from(state.disconnected) << 4;
        if state.group.is_some() {
            flags |= 1 << 5;
        }
        let mut data = vec![flags];
        data.extend_from_slice(&uuid);
        write_utf(&mut data, &state.name);
        if let Some(group) = state.group {
            data.extend_from_slice(&group);
        }
        data
    }

    fn broadcast_states(&mut self) {
        let uuids: Vec<[u8; 16]> = self.states.keys().copied().collect();
        let mut data = Vec::new();
        data.extend_from_slice(&(uuids.len() as i32).to_be_bytes());
        for uuid in &uuids {
            let state = self.states.get(uuid).cloned().unwrap_or_default();
            data.extend_from_slice(&self.state_payload(&state, *uuid));
        }
        for player in self.voice_players() {
            self.queue_payload(player, "voicechat:states", data.clone());
        }
    }

    fn broadcast_state(&mut self, uuid: [u8; 16]) {
        let Some(state) = self.states.get(&uuid).cloned() else { return };
        let data = self.state_payload(&state, uuid);
        for player in self.voice_players() {
            self.queue_payload(player, "voicechat:state", data.clone());
        }
    }

    fn group_payload(&self, group: &ServerGroup) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&group.id);
        write_utf(&mut data, &group.name);
        data.push(u8::from(group.password.is_some()));
        data.push(u8::from(group.persistent));
        data.push(0); // hidden
        data.extend_from_slice(&group.group_type.to_be_bytes());
        data
    }

    fn broadcast_group(&mut self, group: &ServerGroup) {
        let data = self.group_payload(group);
        for player in self.voice_players() {
            self.queue_payload(player, "voicechat:add_group", data.clone());
        }
    }

    fn joined_group_payload(has_group: bool, group: Option<[u8; 16]>, wrong_password: bool) -> Vec<u8> {
        let mut data = Vec::new();
        data.push(u8::from(has_group));
        if has_group {
            data.extend_from_slice(&group.unwrap_or([0; 16]));
        }
        data.push(u8::from(wrong_password));
        data
    }

    /// `Server.generateNewSecret`: one secret per login session; a second
    /// request is refused while one exists, so a duplicate request can never
    /// bind a second socket.
    fn ensure_secret(&mut self, player: [u8; 16], name: &str) -> bool {
        if self.secrets.contains_key(&player) {
            return false;
        }
        let mut secret = [0u8; 16];
        if SystemRandom::new().fill(&mut secret).is_err() {
            return false;
        }
        self.secrets.insert(player, secret);
        self.states.entry(player).or_insert_with(|| PlayerState {
            name: name.to_owned(),
            ..PlayerState::default()
        });
        true
    }

    /// `processMicPacket`: route one microphone frame by group and range.
    fn route_mic(&mut self, sender: [u8; 16], packet: &Packet) {
        let Packet::Mic { opus, sequence, whispering } = packet else { return };
        if opus.len() > MAX_OPUS {
            return;
        }
        let Some(sender_state) = self.states.get(&sender).cloned() else { return };
        let receivers: Vec<[u8; 16]> = self.connections.keys().copied().collect();
        if let Some(group_id) = sender_state.group {
            // Group members hear the sender statically.
            if let Some(group) = self.groups.get(&group_id).cloned() {
                let group_packet = encode_packet(&Packet::GroupSound {
                    channel: sender,
                    sender,
                    opus: opus.clone(),
                    sequence: *sequence,
                    category: None,
                });
                if let Some(group_packet) = group_packet {
                    for receiver in &receivers {
                        if *receiver == sender {
                            continue;
                        }
                        let Some(state) = self.states.get(receiver) else { continue };
                        if state.group != Some(group_id) {
                            continue;
                        }
                        self.send_sound(receiver, &group_packet);
                    }
                }
                // OPEN groups also broadcast to nearby players.
                if group.group_type == 1 {
                    self.route_proximity(
                        sender, opus, *sequence, *whispering, Some(group_id), &receivers,
                    );
                }
            }
            return;
        }
        self.route_proximity(sender, opus, *sequence, *whispering, None, &receivers);
    }

    /// `processProximityPacket`: player sound packets to everyone in range
    /// who is not shielded by group membership.
    fn route_proximity(
        &mut self,
        sender: [u8; 16],
        opus: &[u8],
        sequence: i64,
        whispering: bool,
        sender_group: Option<[u8; 16]>,
        receivers: &[[u8; 16]],
    ) {
        let Some(sender_roster) = self.roster.get(&sender).cloned() else { return };
        let distance = if whispering {
            self.config.whisper_distance
        } else {
            self.config.distance
        } as f32;
        let broadcast_range = (self.config.distance + 1.0).max(f64::from(distance));
        let Some(packet) = encode_packet(&Packet::PlayerSound {
            channel: sender,
            sender,
            opus: opus.to_vec(),
            sequence,
            distance,
            whispering,
            category: None,
        }) else { return };
        for receiver in receivers {
            if *receiver == sender {
                continue;
            }
            let Some(state) = self.states.get(receiver).cloned() else { continue };
            if sender_group.is_some() && state.group == sender_group {
                continue;
            }
            if let Some(group_id) = state.group
                && self.groups.get(&group_id).is_some_and(|group| group.group_type == 2)
            {
                // Isolated groups hear nothing from outside.
                continue;
            }
            let Some(receiver_roster) = self.roster.get(receiver).cloned() else { continue };
            if receiver_roster.dimension != sender_roster.dimension {
                continue;
            }
            let dx = receiver_roster.position[0] - sender_roster.position[0];
            let dy = receiver_roster.position[1] - sender_roster.position[1];
            let dz = receiver_roster.position[2] - sender_roster.position[2];
            if dx * dx + dy * dy + dz * dz > broadcast_range * broadcast_range {
                continue;
            }
            self.send_sound(receiver, &packet);
        }
    }

    /// `sendSoundPacket`: receiver-side gates, then the UDP send.
    fn send_sound(&mut self, receiver: &[u8; 16], packet: &[u8]) {
        let blocked = self
            .states
            .get(receiver)
            .is_some_and(|state| state.disabled || state.disconnected);
        if blocked {
            return;
        }
        let Some(connection) = self.connections.get(receiver).cloned() else { return };
        let Some(secret) = self.secrets.get(receiver).copied() else { return };
        self.send_udp_raw(connection.address, packet, &secret);
    }

    /// One received, decrypted client datagram (`ProcessThread`'s dispatch).
    fn handle_datagram(&mut self, datagram: &[u8], source: SocketAddr) {
        self.datagrams_received += 1;
        let mut reader = Reader::new(datagram);
        if reader.u8() != Some(MAGIC) {
            return;
        }
        let Some(player) = reader.array16() else {
            return;
        };
        let Some(secret) = self.secrets.get(&player).copied() else {
            return;
        };
        let Some(encrypted) = reader.byte_array(MAX_DATAGRAM) else {
            return;
        };
        if encrypted.len() < NONCE_BYTES + TAG_BYTES {
            return;
        }
        let nonce: [u8; NONCE_BYTES] = encrypted[..NONCE_BYTES].try_into().unwrap();
        let mut plaintext = encrypted[NONCE_BYTES..].to_vec();
        let Ok(key) = UnboundKey::new(&AES_128_GCM, &secret) else { return };
        let key = LessSafeKey::new(key);
        let Ok(opened) =
            key.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut plaintext)
        else {
            return;
        };
        let Some((packet, _)) = decode_packet_prefix(opened) else {
            return;
        };
        match packet {
            Packet::Authenticate { player: auth_player, secret: auth_secret } => {
                if auth_player != player || self.secrets.get(&player) != Some(&auth_secret) {
                    return;
                }
                // An existing connection keeps its bound address; only a
                // first authentication creates one (upstream's branch).
                let existing = self
                    .unchecked
                    .get(&player)
                    .or_else(|| self.connections.get(&player))
                    .cloned();
                let address = match existing {
                    Some(connection) => connection.address,
                    None => {
                        let connection = ClientConnection {
                            uuid: player,
                            address: source,
                            last_keepalive: Instant::now(),
                        };
                        let address = connection.address;
                        self.unchecked.insert(player, connection);
                        address
                    }
                };
                self.send_udp(address, &Packet::AuthenticateAck, &secret);
            }
            Packet::ConnectionCheck => {
                // Bound by source address like upstream getUnconnectedSender.
                let known = self
                    .unchecked
                    .iter()
                    .find(|(_, connection)| connection.address == source)
                    .map(|(uuid, connection)| (*uuid, connection.address))
                    .or_else(|| {
                        self.connections
                            .iter()
                            .find(|(_, connection)| connection.address == source)
                            .map(|(uuid, connection)| (*uuid, connection.address))
                    });
                if let Some((uuid, address)) = known {
                    if let Some(connection) = self.unchecked.remove(&uuid) {
                        self.connections.insert(uuid, connection);
                    } else if let Some(connection) = self.connections.get_mut(&uuid) {
                        connection.last_keepalive = Instant::now();
                    }
                    let secret = self.secrets.get(&uuid).copied().unwrap_or(secret);
                    self.send_udp(address, &Packet::ConnectionCheckAck, &secret);
                }
            }
            Packet::KeepAlive => {
                if let Some(connection) = self.connections.get_mut(&player) {
                    if connection.address == source {
                        connection.last_keepalive = Instant::now();
                    }
                }
            }
            Packet::Ping { id, timestamp } => {
                if self
                    .connections
                    .get(&player)
                    .is_some_and(|connection| connection.address == source)
                {
                    self.send_udp(source, &Packet::Ping { id, timestamp }, &secret);
                }
            }
            Packet::Mic { .. } => {
                let connected = self
                    .connections
                    .get(&player)
                    .is_some_and(|connection| connection.address == source);
                if connected {
                    self.route_mic(player, &packet);
                }
            }
            _ => {}
        }
    }

    /// Keepalives and timeouts (`sendKeepAlives`).
    fn keepalive_tick(&mut self) {
        let keepalive = Duration::from_millis(self.config.keepalive_ms);
        if !self.keepalive_due() {
            return;
        }
        self.last_keepalive = Some(Instant::now());
        let timeout = keepalive * 10;
        let expired: Vec<[u8; 16]> = self
            .connections
            .iter()
            .filter(|(_, connection)| connection.last_keepalive.elapsed() >= timeout)
            .map(|(uuid, _)| *uuid)
            .collect();
        let expired_count = expired.len();
        for uuid in expired {
            self.connections.remove(&uuid);
            self.unchecked.remove(&uuid);
            // The secret dies with the connection: the client re-requests.
            self.secrets.remove(&uuid);
            if let Some(state) = self.states.get_mut(&uuid) {
                state.disconnected = true;
                state.group = None;
            }
        }
        if expired_count > 0 {
            self.broadcast_states();
        }
        let connections: Vec<(SocketAddr, [u8; 16])> = self
            .connections
            .values()
            .map(|connection| (connection.address, connection.uuid))
            .collect();
        for (address, uuid) in connections {
            if let Some(secret) = self.secrets.get(&uuid).copied() {
                self.send_udp(address, &Packet::KeepAlive, &secret);
            }
        }
    }

    /// A `voicechat:*` payload from the hub (the plugin's payload handlers).
    fn handle_payload(&mut self, player: [u8; 16], channel: &str, data: &[u8]) {
        self.last_feed = Some(Instant::now());
        match channel {
            "voicechat:request_secret" => {
                if data.len() < 4
                    || i32::from_be_bytes(data[..4].try_into().unwrap()) != SVC_COMPATIBILITY_VERSION
                {
                    // A client of another generation gets no secret, exactly
                    // like the official server (it sends a chat message).
                    return;
                }
                let name = self
                    .roster
                    .get(&player)
                    .map(|entry| entry.name.clone())
                    .unwrap_or_else(|| "Player".to_owned());
                self.ensure_secret(player, &name);
                if let Some(state) = self.states.get_mut(&player) {
                    state.disconnected = false;
                }
                if let Some(payload) = self.secret_payload(player) {
                    self.queue_payload(player, "voicechat:secret", payload);
                    self.broadcast_states();
                }
            }
            "voicechat:client_state" => {
                if data.len() < 2 {
                    return;
                }
                let Some(state) = self.states.get_mut(&player) else { return };
                state.muted = data[0] != 0;
                state.disabled = data[1] != 0;
                self.broadcast_state(player);
            }
            "voicechat:create_group" => {
                if !self.config.groups {
                    return;
                }
                let Some(request) = parse_create_group(data) else { return };
                let mut id = [0u8; 16];
                if SystemRandom::new().fill(&mut id).is_err() {
                    return;
                }
                let group = ServerGroup {
                    id,
                    name: request.name,
                    password: request.password,
                    persistent: false,
                    group_type: request.group_type,
                };
                self.groups.insert(group.id, group.clone());
                if let Some(state) = self.states.get_mut(&player) {
                    state.group = Some(group.id);
                }
                self.broadcast_group(&group);
                self.queue_payload(
                    player,
                    "voicechat:joined_group",
                    Self::joined_group_payload(true, Some(group.id), false),
                );
                self.broadcast_states();
            }
            "voicechat:set_group" => {
                if !self.config.groups {
                    return;
                }
                let Some(request) = parse_join_group(data) else { return };
                let Some(group) = self.groups.get(&request.group).cloned() else { return };
                let matches_password = match (&group.password, request.password) {
                    (None, _) => true,
                    (Some(expected), Some(given)) => expected == &given,
                    (Some(_), None) => false,
                };
                if !matches_password {
                    self.queue_payload(
                        player,
                        "voicechat:joined_group",
                        Self::joined_group_payload(false, None, true),
                    );
                    return;
                }
                if let Some(state) = self.states.get_mut(&player) {
                    state.group = Some(group.id);
                }
                self.queue_payload(
                    player,
                    "voicechat:joined_group",
                    Self::joined_group_payload(true, Some(group.id), false),
                );
                self.broadcast_states();
            }
            "voicechat:leave_group" => {
                if let Some(state) = self.states.get_mut(&player) {
                    state.group = None;
                }
                self.queue_payload(
                    player,
                    "voicechat:joined_group",
                    Self::joined_group_payload(false, None, false),
                );
                self.broadcast_states();
                self.prune_empty_groups();
            }
            _ => {}
        }
    }

    /// Non-persistent groups vanish when their last member leaves
    /// (`GroupManager`), and the clients are told.
    fn prune_empty_groups(&mut self) {
        let empty: Vec<[u8; 16]> = self
            .groups
            .iter()
            .filter(|(id, group)| {
                !group.persistent
                    && !self.states.iter().any(|(_, state)| state.group == Some(**id))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in empty {
            self.groups.remove(&id);
            for player in self.voice_players() {
                let mut data = Vec::new();
                data.extend_from_slice(&id);
                self.queue_payload(player, "voicechat:remove_group", data.clone());
            }
        }
    }

    /// `Server.onPlayerLoggedOut`: forget the player.
    fn remove_player(&mut self, player: [u8; 16]) {
        self.connections.remove(&player);
        self.unchecked.remove(&player);
        self.secrets.remove(&player);
        self.roster.remove(&player);
        self.states.remove(&player);
        let mut data = Vec::new();
        data.extend_from_slice(&player);
        for other in self.voice_players() {
            self.queue_payload(other, "voicechat:remove_state", data.clone());
        }
        self.prune_empty_groups();
    }
}

struct CreateGroupRequest {
    name: String,
    password: Option<String>,
    group_type: i16,
}

/// `CreateGroupPacket.readFromBytes`: name, optional password, type.
fn parse_create_group(data: &[u8]) -> Option<CreateGroupRequest> {
    let mut reader = Reader::new(data);
    let name = reader.string(24)?;
    let password = if reader.boolean()? { Some(reader.string(24)?) } else { None };
    let group_type = reader.i16()?;
    Some(CreateGroupRequest { name, password, group_type })
}

struct JoinGroupRequest {
    group: [u8; 16],
    password: Option<String>,
}

/// `JoinGroupPacket.readFromBytes`: UUID, optional password.
fn parse_join_group(data: &[u8]) -> Option<JoinGroupRequest> {
    let mut reader = Reader::new(data);
    let group = reader.array16()?;
    let password = if reader.boolean()? { Some(reader.string(256)?) } else { None };
    Some(JoinGroupRequest { group, password })
}

fn write_utf(out: &mut Vec<u8>, value: &str) {
    write_varint(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

/// A bound hosted voice server. The hub hook drives `update_roster` and
/// `handle_payload` from the server thread; the UDP loop runs on its own
/// thread and stops itself when the hub stops feeding (world closed).
pub struct HostedVoiceServer {
    core: Arc<Mutex<Core>>,
    stop: Arc<AtomicBool>,
    port: u16,
}

impl HostedVoiceServer {
    /// Binds the voice UDP port (a random free one, like upstream's
    /// integrated servers) and starts the datagram loop.
    pub fn start(config: HostedVoiceConfig) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("[::]:0").or_else(|_| UdpSocket::bind("0.0.0.0:0"))?;
        socket.set_nonblocking(true)?;
        let port = socket.local_addr()?.port();
        let core = Arc::new(Mutex::new(Core {
            config,
            socket: Some(socket),
            ..Core::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_core = Arc::clone(&core);
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("svchorizon-voice-server".into())
            .spawn(move || {
                let mut buffer = [0u8; MAX_DATAGRAM + 64];
                loop {
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(mut core) = thread_core.lock() else { break };
                    // The hub feeds this server every tick; a world close
                    // stops the feed, and the server follows.
                    if core
                        .last_feed
                        .is_none_or(|feed| feed.elapsed() > Duration::from_secs(5))
                    {
                        break;
                    }
                    while let Some(socket) = core.socket.as_ref() {
                        match socket.recv_from(&mut buffer) {
                            Ok((size, source)) => core.handle_datagram(&buffer[..size], source),
                            Err(_) => break,
                        }
                    }
                    core.keepalive_tick();
                    drop(core);
                    std::thread::sleep(Duration::from_millis(5));
                }
                if let Ok(mut core) = thread_core.lock() {
                    core.socket.take();
                }
            })?;
        Ok(Self { core, stop, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The hub's per-tick roster refresh; also the watchdog's feed.
    pub fn update_roster(&self, entries: Vec<RosterEntry>) {
        if let Ok(mut core) = self.core.lock() {
            core.last_feed = Some(Instant::now());
            let fresh: Vec<[u8; 16]> = entries.iter().map(|entry| entry.uuid).collect();
            core.roster = entries.into_iter().map(|entry| (entry.uuid, entry)).collect();
            // Guests who left the world lose their voice state.
            let departed: Vec<[u8; 16]> = core
                .states
                .keys()
                .copied()
                .filter(|uuid| !fresh.contains(uuid))
                .collect();
            for uuid in departed {
                core.remove_player(uuid);
            }
        }
    }

    /// Delivers one `voicechat:*` payload a guest sent.
    pub fn handle_payload(&self, player: [u8; 16], channel: &str, data: &[u8]) {
        if let Ok(mut core) = self.core.lock() {
            core.handle_payload(player, channel, data);
        }
    }

    /// Payload replies waiting for the hub to deliver over the guests'
    /// Minecraft connections.
    pub fn take_outbound(&self) -> Vec<([u8; 16], String, Vec<u8>)> {
        let Ok(mut core) = self.core.lock() else { return Vec::new() };
        core.outbound.drain(..).collect()
    }

    /// Diagnostics for the host's HUD: is anyone on voice?
    pub fn connected_players(&self) -> usize {
        self.core.lock().map(|core| core.connections.len()).unwrap_or(0)
    }

    /// Stops the loop and closes the socket.
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for HostedVoiceServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A datagram the client sends: magic, raw player UUID, then the
    /// VarInt-prefixed `IV || AES-GCM(plaintext) || tag`.
    fn client_datagram(player: &[u8; 16], secret: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let nonce = [9u8; NONCE_BYTES];
        let key = LessSafeKey::new(UnboundKey::new(&AES_128_GCM, secret).unwrap());
        let mut sealed = plain.to_vec();
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut sealed)
            .unwrap();
        let mut body = nonce.to_vec();
        body.extend_from_slice(&sealed);
        let mut datagram = vec![MAGIC];
        datagram.extend_from_slice(player);
        write_byte_array(&mut datagram, &body);
        datagram
    }

    fn core_with_roster() -> Core {
        let mut core = Core {
            config: HostedVoiceConfig::default(),
            socket: Some(UdpSocket::bind("127.0.0.1:0").unwrap()),
            ..Core::default()
        };
        core.roster.insert(
            [1; 16],
            RosterEntry {
                uuid: [1; 16],
                name: "Alice".into(),
                position: [0.0; 3],
                dimension: "minecraft:overworld".into(),
                spectator: false,
            },
        );
        core.roster.insert(
            [2; 16],
            RosterEntry {
                uuid: [2; 16],
                name: "Bob".into(),
                position: [1.0; 3],
                dimension: "minecraft:overworld".into(),
                spectator: false,
            },
        );
        core
    }

    #[test]
    fn secret_payload_decodes_in_compat_20_field_order() {
        let mut core = core_with_roster();
        core.socket = Some(UdpSocket::bind("127.0.0.1:0").unwrap());
        assert!(core.ensure_secret([1; 16], "Alice"));
        let payload = core.secret_payload([1; 16]).unwrap();
        let mut reader = Reader::new(&payload);
        let secret = reader.array16().unwrap();
        let port = reader.i32().unwrap();
        let player = reader.array16().unwrap();
        let codec = reader.u8().unwrap();
        let mtu = reader.i32().unwrap();
        let distance = reader.f64().unwrap();
        let keepalive = reader.i32().unwrap();
        let groups = reader.boolean().unwrap();
        let host = reader.string(32767).unwrap();
        let recording = reader.boolean().unwrap();
        assert!(reader.done());
        assert_eq!(secret, core.secrets[&[1; 16]]);
        assert_eq!(player, [1; 16]);
        assert_eq!(codec, 0);
        assert_eq!(mtu, 1024);
        assert_eq!(distance, 48.0);
        assert_eq!(keepalive, 1000);
        assert!(groups);
        assert!(host.is_empty());
        assert!(recording);
        assert_ne!(port, 0);
    }

    #[test]
    fn a_second_secret_request_reuses_the_same_secret() {
        let mut core = core_with_roster();
        assert!(core.ensure_secret([1; 16], "Alice"));
        let first = *core.secrets.get(&[1; 16]).unwrap();
        assert!(!core.ensure_secret([1; 16], "Alice"));
        assert_eq!(*core.secrets.get(&[1; 16]).unwrap(), first);
    }

    #[test]
    fn authenticate_is_answered_and_binds_the_source_address() {
        let mut core = core_with_roster();
        core.ensure_secret([1; 16], "Alice");
        let secret = *core.secrets.get(&[1; 16]).unwrap();
        let plain = {
            let mut out = vec![crate::voice::AUTHENTICATE];
            out.extend_from_slice(&[1; 16]);
            out.extend_from_slice(&secret);
            out
        };
        let source: SocketAddr = "127.0.0.1:45001".parse().unwrap();
        core.handle_datagram(&client_datagram(&[1; 16], &secret, &plain), source);
        assert_eq!(core.unchecked.len(), 1);
        assert_eq!(core.unchecked.get(&[1; 16]).unwrap().address, source);
        // One outbound UDP datagram: the AuthenticateAck to that address.
        assert_eq!(core.datagrams_sent, 1);
    }

    #[test]
    fn connection_check_validates_and_connection_check_ack_is_sent() {
        let mut core = core_with_roster();
        core.ensure_secret([1; 16], "Alice");
        let secret = *core.secrets.get(&[1; 16]).unwrap();
        let source: SocketAddr = "127.0.0.1:45002".parse().unwrap();
        let auth = {
            let mut out = vec![crate::voice::AUTHENTICATE];
            out.extend_from_slice(&[1; 16]);
            out.extend_from_slice(&secret);
            out
        };
        core.handle_datagram(&client_datagram(&[1; 16], &secret, &auth), source);
        core.handle_datagram(&client_datagram(&[1; 16], &secret, &[crate::voice::CONNECTION_CHECK]), source);
        assert!(core.connections.contains_key(&[1; 16]));
        assert!(core.unchecked.is_empty());
        assert_eq!(core.datagrams_sent, 2);
    }

    #[test]
    fn mic_frames_route_to_connected_peers_in_range() {
        let mut core = core_with_roster();
        for player in [[1; 16], [2; 16]] {
            core.ensure_secret(player, "player");
        }
        let source: SocketAddr = "127.0.0.1:45003".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:45004".parse().unwrap();
        let authenticate = |player: [u8; 16], secret: [u8; 16]| {
            let mut out = vec![crate::voice::AUTHENTICATE];
            out.extend_from_slice(&player);
            out.extend_from_slice(&secret);
            out
        };
        core.handle_datagram(
            &client_datagram(&[1; 16], &core.secrets[&[1; 16]], &authenticate([1; 16], core.secrets[&[1; 16]])),
            source,
        );
        core.handle_datagram(
            &client_datagram(&[2; 16], &core.secrets[&[2; 16]], &authenticate([2; 16], core.secrets[&[2; 16]])),
            peer,
        );
        core.handle_datagram(&client_datagram(&[1; 16], &core.secrets[&[1; 16]], &[crate::voice::CONNECTION_CHECK]), source);
        core.handle_datagram(&client_datagram(&[2; 16], &core.secrets[&[2; 16]], &[crate::voice::CONNECTION_CHECK]), peer);
        assert_eq!(core.connections.len(), 2);

        // Alice transmits 20 ms of Opus payload; Bob is 1 block away.
        let sent_before = core.datagrams_sent;
        let mic = {
            let mut out = vec![crate::voice::MIC];
            write_byte_array(&mut out, &[7u8; 64]);
            out.extend_from_slice(&1i64.to_be_bytes());
            out.push(0);
            out
        };
        core.handle_datagram(&client_datagram(&[1; 16], &core.secrets[&[1; 16]], &mic), source);
        // Exactly one sound packet: to Bob.
        assert_eq!(core.datagrams_sent - sent_before, 1);
    }

    #[test]
    fn group_join_with_wrong_password_reports_it() {
        let mut core = core_with_roster();
        core.ensure_secret([1; 16], "Alice");
        let create = {
            let mut data = Vec::new();
            write_utf(&mut data, "Friends");
            data.push(1);
            write_utf(&mut data, "s3cret");
            data.extend_from_slice(&0i16.to_be_bytes());
            data
        };
        core.handle_payload([1; 16], "voicechat:create_group", &create);
        let group_id = *core.states.get(&[1; 16]).unwrap().group.as_ref().unwrap();
        // Bob joins with the wrong password.
        core.ensure_secret([2; 16], "Bob");
        let join = {
            let mut data = Vec::new();
            data.extend_from_slice(&group_id);
            data.push(1);
            write_utf(&mut data, "wrong");
            data
        };
        core.handle_payload([2; 16], "voicechat:set_group", &join);
        assert!(core.states.get(&[2; 16]).unwrap().group.is_none());
        let replies: Vec<_> = core
            .outbound
            .iter()
            .filter(|(player, channel, _)| *player == [2; 16] && channel == "voicechat:joined_group")
            .collect();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].2, vec![0, 1]); // no group, wrong password
        // Bob joins correctly.
        let join_ok = {
            let mut data = Vec::new();
            data.extend_from_slice(&group_id);
            data.push(1);
            write_utf(&mut data, "s3cret");
            data
        };
        core.handle_payload([2; 16], "voicechat:set_group", &join_ok);
        assert_eq!(core.states.get(&[2; 16]).unwrap().group, Some(group_id));
    }

    #[test]
    fn client_state_updates_broadcast_single_state_payloads() {
        let mut core = core_with_roster();
        core.ensure_secret([1; 16], "Alice");
        core.ensure_secret([2; 16], "Bob");
        core.outbound.clear();
        core.handle_payload([1; 16], "voicechat:client_state", &[1, 0]);
        let state = core.states.get(&[1; 16]).unwrap();
        assert!(state.muted && !state.disabled);
        let updates: Vec<_> = core
            .outbound
            .iter()
            .filter(|(_, channel, _)| channel == "voicechat:state")
            .collect();
        // One update per voice player (Alice and Bob both hold secrets).
        assert_eq!(updates.len(), 2);
        // Flags byte: muted = bit 0.
        assert_eq!(updates[0].2[0] & 1, 1);
    }

    #[test]
    fn a_compat_21_client_gets_no_secret() {
        let mut core = core_with_roster();
        core.handle_payload([1; 16], "voicechat:request_secret", &21i32.to_be_bytes());
        assert!(core.secrets.is_empty());
        assert!(core.outbound.is_empty());
    }

    #[test]
    fn departed_players_lose_their_state() {
        let mut core = core_with_roster();
        core.ensure_secret([1; 16], "Alice");
        core.ensure_secret([2; 16], "Bob");
        core.last_feed = Some(Instant::now());
        // Bob joins the world's roster; Alice is gone.
        core.roster.remove(&[1; 16]);
        core.roster.insert(
            [2; 16],
            RosterEntry {
                uuid: [2; 16],
                name: "Bob".into(),
                position: [0.0; 3],
                dimension: "minecraft:overworld".into(),
                spectator: false,
            },
        );
        let departed: Vec<[u8; 16]> = core
            .states
            .keys()
            .copied()
            .filter(|uuid| !core.roster.contains_key(uuid))
            .collect();
        for uuid in departed {
            core.remove_player(uuid);
        }
        assert!(core.states.contains_key(&[2; 16]));
        assert!(!core.states.contains_key(&[1; 16]));
    }
}
