// One click on the mod's Options > Mods settings page.
fn svchorizon_native_settings_click(
    game: &mut crate::game::Game<'_>,
    mod_id: &str,
    button: usize,
) {
    crate::game::svchorizon::on_settings_click(game, mod_id, button);
}
