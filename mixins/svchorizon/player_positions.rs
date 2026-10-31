fn svchorizon_player_positions(game: &mut crate::game::Game<'_>) {
    let positions = game.remote.as_ref().map(|remote| {
        remote.level_players.iter().filter_map(|(entity_id, uuid)| {
            remote.entities.get(entity_id).map(|tracked| (uuid.to_be_bytes(), tracked.position))
        }).collect::<Vec<_>>()
    }).unwrap_or_default();
    crate::game::svchorizon::set_player_positions(positions);
}
