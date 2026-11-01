/// The address this session's TCP connection actually reached, after Minecraft's
/// SRV redirect and DNS resolution. The official Simple Voice Chat client sends
/// voice to this host when the server's `voice_host` is empty, not to the name
/// the player typed (`Session::address`), which can resolve elsewhere.
pub fn peer_address(session: &super::Session) -> Option<std::net::SocketAddr> {
    let writer = session.writer.lock().ok()?;
    horizon_protocol::connection::svchorizon_peer::peer_address(writer.as_ref()?)
}
