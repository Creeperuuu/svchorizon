fn svchorizon_native_command(
    game: &mut crate::game::Game<'_>,
    command: &str,
    ran: &mut bool,
) {
    crate::game::svchorizon::on_command(game, command, ran);
}
