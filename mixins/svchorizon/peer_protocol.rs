/// The remote socket address of a TCP connection: the server (or SRV target)
/// the game really connected to, or the reason it cannot be read. Lives beside
/// `ConnWriter` because its stream is private to the protocol crate.
pub fn peer_address(writer: &super::ConnWriter) -> Result<std::net::SocketAddr, String> {
    match &writer.stream {
        super::Stream::Tcp(stream) => stream.peer_addr().map_err(|error| format!("peer_addr failed: {error}")),
        super::Stream::Memory(_) => Err("in-memory connection (integrated server)".to_owned()),
    }
}
