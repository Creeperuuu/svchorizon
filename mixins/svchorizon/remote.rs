/// Access to Remote's tracked player map for spatial voice playback.
/// This module is injected inside remote.rs so it can read the private entity
/// and player UUID maps without exposing them through Horizon's public API.
pub(in crate::game) fn player_positions(
    remote: &super::Remote,
) -> Vec<([u8; 16], [f64; 3])> {
    remote.level_players.iter().filter_map(|(entity_id, uuid)| {
        remote.entities.get(entity_id)
            .map(|tracked| (uuid.to_be_bytes(), tracked.position))
    }).collect()
}
