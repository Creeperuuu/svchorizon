fn svchorizon_native_hud(
    game: &mut crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
) {
    crate::game::svchorizon::on_hud(game, batch);
}
