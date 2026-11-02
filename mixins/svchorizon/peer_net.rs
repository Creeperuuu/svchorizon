/// The address this session's TCP connection actually reached, after Minecraft's
/// SRV redirect and DNS resolution, or the reason it is not available. The
/// official Simple Voice Chat client sends voice to this host when the server's
/// `voice_host` is empty, not to the name the player typed (`Session::address`),
/// which can resolve elsewhere.
pub fn peer_address(session: &super::Session) -> Result<std::net::SocketAddr, String> {
    let writer = session.writer.lock().map_err(|_| "connection writer lock poisoned".to_owned())?;
    let writer = writer.as_ref().ok_or_else(|| "no open connection writer".to_owned())?;
    horizon_protocol::connection::svchorizon_peer::peer_address(writer)
}
