fn svchorizon_native_commands(
    game: &mut crate::game::Game<'_>,
    commands: &mut Vec<&'static str>,
) {
    crate::game::svchorizon::on_commands(game, commands);
}
