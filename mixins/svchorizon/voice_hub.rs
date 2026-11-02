// The bridge between Horizon's integrated/LAN hub and the hosted Simple
// Voice Chat server. Compiled inside `lan/hub.rs` by the HEAD hook on
// `Hub::process_inbox`, so it can read the guests' private fields directly.
//
// The glue itself lives in the engine crate (`server_host`), because `lan/`
// is in the client's library crate while the mod's runtime module lives in
// the binary crate; the engine is the one dependency both share.
fn svchorizon_voice_hub(hub: &mut crate::lan::hub::Hub, guest_index: usize) {
    use horizon_protocol::packets::{configuration::CustomPayload, play};

    let Some(guest) = hub.guests.get(guest_index) else { return };
    if !guest.placed {
        return;
    }
    let sender_uuid = guest.uuid.to_be_bytes();

    // The world's roster as the voice server should see it this tick.
    let roster: Vec<svchorizon_engine::server_host::HubGuest> = hub
        .guests
        .iter()
        .filter(|guest| guest.placed)
        .map(|guest| svchorizon_engine::server_host::HubGuest {
            uuid: guest.uuid.to_be_bytes(),
            name: guest.name.clone(),
            position: guest.position,
            dimension: guest.dimension.clone(),
            spectator: guest.spectator(),
        })
        .collect();

    // This guest's voicechat payloads (still queued for the hub's own match,
    // which ignores them; peeking keeps the original behaviour intact).
    let payloads: Vec<(String, Vec<u8>)> = guest
        .inbox
        .iter()
        .filter_map(|(packet, _)| match packet {
            play::Serverbound::CustomPayload(payload)
                if payload.channel.starts_with("voicechat:") =>
            {
                Some((payload.channel.clone(), payload.data.clone()))
            }
            _ => None,
        })
        .collect();

    let replies = svchorizon_engine::server_host::hub_tick(roster, sender_uuid, payloads);

    for (target, channel, data) in replies {
        let recipient = hub
            .guests
            .iter()
            .find(|guest| guest.uuid.to_be_bytes() == target);
        if let Some(recipient) = recipient {
            let packet = play::Clientbound::CustomPayload(CustomPayload {
                channel,
                data,
            });
            recipient.sender.send(packet.encode());
        }
    }
}
