// The HUD hook runs every frame while the world is drawn. `corners` is part
// of the hook's contract (the mod pages' corner widgets); this mod draws no
// corner, so it only forwards the batch.
fn svchorizon_native_hud(
    game: &mut crate::game::Game<'_>,
    batch: &mut horizon_client::hud::HudBatch,
    corners: &mut horizon_client::extras_hud::Corners,
) {
    let _ = corners;
    crate::game::svchorizon::on_hud(game, batch);
}
