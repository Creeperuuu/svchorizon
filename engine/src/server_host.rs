//! The hosted-voice glue that Horizon's integrated/LAN hub calls. Lives in
//! the engine crate because `lan/hub.rs` is in the client's *library* crate
//! while the mod's runtime module is in its *binary* crate; both depend on
//! the engine, so this is the one place both sides can reach.
//!
//! The hub's HEAD hook (`voice_hub.rs` in the bundle) extracts every placed
//! guest's roster entry and queued `voicechat:*` payloads and calls
//! [`hub_tick`] once per guest per tick; the returned payloads are delivered
//! back over the guests' Minecraft connections.

use crate::server::{HostedVoiceConfig, HostedVoiceServer, RosterEntry};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

static HOSTED_SERVER: OnceLock<Mutex<Option<Arc<HostedVoiceServer>>>> = OnceLock::new();

fn hosted_server() -> &'static Mutex<Option<Arc<HostedVoiceServer>>> {
    HOSTED_SERVER.get_or_init(|| Mutex::new(None))
}

/// One hub guest as the voice server sees it.
#[derive(Clone, Debug)]
pub struct HubGuest {
    pub uuid: [u8; 16],
    pub name: String,
    pub position: [f64; 3],
    pub dimension: String,
    pub spectator: bool,
}

/// Per-guest-per-tick entry point. Starts the server on the first call
/// (a random UDP port, like upstream's integrated servers), refreshes the
/// roster, feeds one guest's payloads, and returns the payload replies.
pub fn hub_tick(
    roster: Vec<HubGuest>,
    sender: [u8; 16],
    payloads: Vec<(String, Vec<u8>)>,
) -> Vec<([u8; 16], String, Vec<u8>)> {
    let server = {
        let mut slot = hosted_server().lock().expect("svchorizon hosted server poisoned");
        if slot.is_none() {
            match HostedVoiceServer::start(HostedVoiceConfig::default()) {
                Ok(server) => {
                    let port = server.port();
                    *slot = Some(Arc::new(server));
                    eprintln!("[svchorizon] Hosted voice chat server started on UDP port {port} (singleplayer/LAN).");
                }
                Err(error) => {
                    eprintln!("[svchorizon] Hosted voice server failed to start: {error}");
                    return Vec::new();
                }
            }
        }
        slot.as_ref().expect("just started").clone()
    };
    let entries = roster
        .into_iter()
        .map(|guest| RosterEntry {
            uuid: guest.uuid,
            name: guest.name,
            position: guest.position,
            dimension: guest.dimension,
            spectator: guest.spectator,
        })
        .collect();
    server.update_roster(entries);
    for (channel, data) in payloads {
        server.handle_payload(sender, &channel, &data);
    }
    server.take_outbound()
}

/// Whether a hosted voice server is running for the current world.
pub fn is_running() -> bool {
    hosted_server()
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

/// Stops the hosted voice server (world closed). Returns the UDP port it
/// used, for the log.
pub fn shutdown() {
    if let Some(server) = hosted_server().lock().expect("svchorizon hosted server poisoned").take() {
        eprintln!("[svchorizon] Hosted voice chat server stopped.");
        server.shutdown();
    }
}

/// Guest UUIDs currently holding a roster entry (diagnostics).
#[allow(dead_code)]
pub fn roster_uuids() -> Vec<[u8; 16]> {
    hosted_server()
        .lock()
        .ok()
        .map(|slot| {
            slot.as_ref()
                .map(|server| server.roster_snapshot())
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Silences the unused-import warning when only part of the API is used.
#[allow(unused)]
fn _type_witness() -> HashMap<[u8; 16], ()> {
    HashMap::new()
}
