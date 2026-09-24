use std::sync::{Arc, LazyLock};

use base64::{engine::general_purpose::STANDARD, Engine};
use fastwebsockets::{handshake, FragmentCollectorRead, WebSocket, WebSocketWrite};
use http_body_util::Empty;
use hyper::{body::Bytes, upgrade::Upgraded, Request};
use hyper_util::rt::{TokioExecutor, TokioIo};
use sha1::{Digest, Sha1};
use tokio::{
    io::{self, AsyncRead, AsyncWrite, ReadHalf, WriteHalf},
    net::TcpStream,
};
use tokio_rustls::{
    rustls::{crypto::ring, pki_types::ServerName, ClientConfig, RootCertStore},
    TlsConnector,
};
use url::{Host, Position, Url};
use webpki_roots::TLS_SERVER_ROOTS;

use super::Error;

/// Inbound half that assembles fragmented frames before session-level validation.
pub(super) type Reader = FragmentCollectorRead<ReadHalf<TokioIo<Upgraded>>>;

/// Outbound half; writes rely on the peer continuing to read rather than a local deadline.
pub(super) type Writer = WebSocketWrite<WriteHalf<TokioIo<Upgraded>>>;

/// Enough for a maximum-size Solana account encoded as base64, including its envelope.
pub(super) const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Shares TLS configuration across connections without initializing it for plain WebSockets.
static TLS: LazyLock<Result<TlsConnector, tokio_rustls::rustls::Error>> = LazyLock::new(|| {
    let config = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(RootCertStore::from_iter(TLS_SERVER_ROOTS.iter().cloned()))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
});

/// Opens a valid provider URL; the caller bounds connection setup with one deadline.
pub(super) async fn connect(url: &Url) -> Result<(Reader, Writer), Error> {
    let host = match url.host().ok_or(Error::Protocol("provider URL has no host"))? {
        Host::Ipv6(ip) => ip.to_string(),
        host => host.to_string(),
    };
    let port = url.port_or_known_default().ok_or(Error::Protocol("provider URL has no port"))?;
    let tcp = TcpStream::connect((host.as_str(), port)).await?;
    tcp.set_nodelay(true)?;
    let mut socket = if url.scheme() == "wss" {
        let tls = TLS.as_ref().map_err(|error| Error::Tls(error.clone()))?;
        upgrade(url, tls.connect(ServerName::try_from(host)?, tcp).await?).await?
    } else {
        upgrade(url, tcp).await?
    };
    // Keep all outbound frames on the session's writer, including control replies.
    socket.set_auto_pong(false);
    socket.set_auto_close(false);
    // This bounds individual frames; the session checks assembled messages separately.
    // fastwebsockets rejects lengths >= its limit, whereas MAX_MESSAGE is inclusive.
    socket.set_max_message_size(MAX_MESSAGE + 1);
    let (reader, writer) = socket.split(io::split);
    Ok((FragmentCollectorRead::new(reader), writer))
}

/// Upgrades an established stream, verifying the server key and rejecting unrequested extensions.
async fn upgrade<S>(url: &Url, stream: S) -> Result<WebSocket<TokioIo<Upgraded>>, Error>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let key = handshake::generate_key();
    let accept = STANDARD.encode(Sha1::digest(format!(
        "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    )));
    let request = Request::builder()
        .uri(&url[Position::BeforePath..Position::AfterQuery])
        .header("Host", &url[Position::BeforeHost..Position::AfterPort])
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Key", key)
        .header("Sec-WebSocket-Version", "13")
        .body(Empty::<Bytes>::new())?;
    let (socket, response) = handshake::client(&TokioExecutor::new(), request, stream).await?;
    // The helper checks the HTTP upgrade headers but not the key or unsolicited
    // extensions. Preserve the client handshake guarantees of the previous transport.
    let headers = response.headers();
    if headers.get("Sec-WebSocket-Accept").map(|value| value.as_bytes()) != Some(accept.as_bytes())
        || headers.contains_key("Sec-WebSocket-Extensions")
        || headers.contains_key("Sec-WebSocket-Protocol")
    {
        return Err(Error::Protocol("invalid WebSocket upgrade response"));
    }
    Ok(socket)
}
