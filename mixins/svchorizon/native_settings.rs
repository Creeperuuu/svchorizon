// The mod's buttons on Options > Mods (Horizon's native mod settings page).
fn svchorizon_native_settings(
    game: &mut crate::game::Game<'_>,
    mod_id: &str,
    buttons: &mut Vec<String>,
) {
    crate::game::svchorizon::on_settings(game, mod_id, buttons);
}
