// Observes the real Minecraft play-packet stream. The original packet handler
// still runs after this helper, so unrelated packets retain normal behavior.
fn svchorizon_handle_packet(
    game: &mut crate::game::Game<'_>,
    packet: &horizon_protocol::packets::play::Clientbound,
) {
    crate::game::svchorizon::on_packet(game, packet);
}
