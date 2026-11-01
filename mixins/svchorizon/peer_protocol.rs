/// The remote socket address of a TCP connection: the server (or SRV target)
/// the game really connected to. Lives beside `ConnWriter` because its stream
/// is private to the protocol crate.
pub fn peer_address(writer: &super::ConnWriter) -> Option<std::net::SocketAddr> {
    match &writer.stream {
        super::Stream::Tcp(stream) => stream.peer_addr().ok(),
        super::Stream::Memory(_) => None,
    }
}
