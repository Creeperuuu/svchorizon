fn svchorizon_native_key(
    game: &mut crate::game::Game<'_>,
    key: &str,
    pressed: bool,
) {
    crate::game::svchorizon::on_key(game, key, pressed);
}
