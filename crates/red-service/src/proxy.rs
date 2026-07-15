//! Forward-proxy tunneling for network connections (SOCKS5 / HTTP `CONNECT`).
//!
//! The sibling of [`crate::tunnel`], for reaching a database through a proxy rather
//! than an SSH jump host. Same contract: bind a local listener and, on each inbound
//! accept, open a *fresh* proxied stream to the real `host:port` and splice, so the
//! driver only ever dials `127.0.0.1:<port>` (see [`ConnectionConfig::local_dsn`])
//! and the pooled drivers can open connections repeatedly over the session's life.
//! A single piped socket would break on the second database connection.
//!
//! A proxy is a simpler forward than SSH: no host-key verification, no channel
//! multiplexing. We hand-roll both handshakes (RFC 1928 SOCKS5 + RFC 1929
//! user/pass auth, and HTTP `CONNECT`) rather than pull a dependency, matching how
//! `tunnel.rs` speaks russh directly and `redis_kv.rs` hand-rolls its decoders.
//! TLS *to the proxy itself* (an HTTPS proxy) is out of scope for v1.

use std::net::SocketAddr;

use red_core::{ProxyConfig, ProxyKind, RedError, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A live local port-forward through a proxy. Hold it for as long as the database
/// connection that rides it; dropping it aborts the accept loop.
pub(crate) struct Proxy {
    local_addr: SocketAddr,
    accept_loop: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept_loop.abort();
    }
}

impl Proxy {
    /// The local address the driver should dial. Always on `127.0.0.1`.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Bind a local listener and start forwarding to `(remote_host, remote_port)`
    /// through the configured proxy. Validates reachability of the proxy up front
    /// (a single handshake) so a bad proxy fails the connect rather than silently
    /// erroring on the first forwarded socket.
    pub(crate) async fn open(
        proxy: &ProxyConfig,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Proxy> {
        // Prove the proxy works before we report success: one connect + handshake,
        // then drop it. This turns a bad host / refused auth into a clean connect
        // error instead of a tunnel that accepts locally but fails every forward.
        probe(proxy, remote_host, remote_port).await?;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| RedError::Connect(format!("could not open local proxy port: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| RedError::Connect(format!("could not read local proxy port: {e}")))?;

        let proxy = proxy.clone();
        let remote_host = remote_host.to_string();
        let accept_loop = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break; // listener closed
                };
                let proxy = proxy.clone();
                let remote_host = remote_host.clone();
                tokio::spawn(async move {
                    match connect_through(&proxy, &remote_host, remote_port).await {
                        Ok(mut upstream) => {
                            let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
                        }
                        Err(e) => tracing::warn!("proxy forward failed: {e}"),
                    }
                });
            }
        });

        Ok(Proxy {
            local_addr,
            accept_loop,
        })
    }
}

/// Open one proxied stream to `(remote_host, remote_port)` (a fresh proxy TCP
/// connection + handshake per call), returning the connected stream ready to
/// splice. This is what each accepted local socket rides.
async fn connect_through(
    proxy: &ProxyConfig,
    remote_host: &str,
    remote_port: u16,
) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|e| RedError::Connect(format!("connect to proxy {}: {e}", proxy.host)))?;
    match proxy.kind {
        ProxyKind::Socks5 => socks5_handshake(&mut stream, proxy, remote_host, remote_port).await?,
        ProxyKind::HttpConnect => {
            http_connect(&mut stream, proxy, remote_host, remote_port).await?
        }
    }
    Ok(stream)
}

/// One throwaway handshake to validate the proxy at connect time.
async fn probe(proxy: &ProxyConfig, remote_host: &str, remote_port: u16) -> Result<()> {
    connect_through(proxy, remote_host, remote_port)
        .await
        .map(drop)
}

/// The SOCKS5 client handshake (RFC 1928) with optional user/pass auth (RFC 1929),
/// then a `CONNECT` to the target by domain name. Leaves `stream` connected to the
/// target on success.
async fn socks5_handshake(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    remote_host: &str,
    remote_port: u16,
) -> Result<()> {
    let use_auth = !proxy.user.is_empty() || !proxy.password.is_empty();
    // Greeting: version 5, then the methods we support (no-auth, and user/pass
    // when credentials are configured), so the proxy picks one it accepts.
    if use_auth {
        stream.write_all(&[0x05, 0x02, 0x00, 0x02]).await
    } else {
        stream.write_all(&[0x05, 0x01, 0x00]).await
    }
    .map_err(socks_err)?;

    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(socks_err)?;
    if reply[0] != 0x05 {
        return Err(RedError::Connect("proxy is not SOCKS5".into()));
    }
    match reply[1] {
        0x00 => {} // no auth required
        0x02 => socks5_userpass_auth(stream, proxy).await?,
        0xFF => {
            return Err(RedError::Auth(
                "SOCKS5 proxy rejected our auth methods".into(),
            ));
        }
        other => {
            return Err(RedError::Connect(format!(
                "SOCKS5 proxy chose unsupported auth method {other:#x}"
            )));
        }
    }

    // CONNECT request: VER=5, CMD=CONNECT(1), RSV=0, ATYP=domain(3), then the
    // length-prefixed host and the 2-byte big-endian port.
    let host = remote_host.as_bytes();
    if host.len() > 255 {
        return Err(RedError::Connect(
            "target host name too long for SOCKS5".into(),
        ));
    }
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    req.extend_from_slice(host);
    req.extend_from_slice(&remote_port.to_be_bytes());
    stream.write_all(&req).await.map_err(socks_err)?;

    // Reply: VER, REP, RSV, ATYP, BND.ADDR, BND.PORT. Read the fixed head, then
    // consume the bound address whose length depends on ATYP.
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await.map_err(socks_err)?;
    if head[1] != 0x00 {
        return Err(RedError::Connect(format!(
            "SOCKS5 CONNECT failed: {}",
            socks_reply_message(head[1])
        )));
    }
    let addr_len = match head[3] {
        0x01 => 4,                               // IPv4
        0x04 => 16,                              // IPv6
        0x03 => read_u8(stream).await? as usize, // domain: 1-byte length prefix
        other => {
            return Err(RedError::Connect(format!(
                "SOCKS5 reply had unknown address type {other:#x}"
            )));
        }
    };
    let mut scratch = vec![0u8; addr_len + 2]; // address + 2-byte port
    stream.read_exact(&mut scratch).await.map_err(socks_err)?;
    Ok(())
}

/// RFC 1929 username/password sub-negotiation, after the proxy selects method 2.
async fn socks5_userpass_auth(stream: &mut TcpStream, proxy: &ProxyConfig) -> Result<()> {
    let user = proxy.user.as_bytes();
    let pass = proxy.password.as_bytes();
    if user.len() > 255 || pass.len() > 255 {
        return Err(RedError::Auth("SOCKS5 credentials too long".into()));
    }
    let mut msg = vec![0x01, user.len() as u8];
    msg.extend_from_slice(user);
    msg.push(pass.len() as u8);
    msg.extend_from_slice(pass);
    stream.write_all(&msg).await.map_err(socks_err)?;

    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await.map_err(socks_err)?;
    if reply[1] != 0x00 {
        return Err(RedError::Auth(
            "SOCKS5 proxy rejected the credentials".into(),
        ));
    }
    Ok(())
}

/// The HTTP `CONNECT` handshake: request a tunnel to `host:port`, expect a `2xx`,
/// and consume the response headers so `stream` is left at the tunnelled body.
async fn http_connect(
    stream: &mut TcpStream,
    proxy: &ProxyConfig,
    remote_host: &str,
    remote_port: u16,
) -> Result<()> {
    let authority = format!("{remote_host}:{remote_port}");
    let mut req = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n"
    );
    if !proxy.user.is_empty() || !proxy.password.is_empty() {
        let token = basic_auth(&proxy.user, &proxy.password);
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.map_err(http_err)?;

    // Read until the end of the header block (CRLFCRLF), byte by byte: the tunnel
    // body follows immediately, so we must not over-read into it.
    let mut headers = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = stream.read(&mut byte).await.map_err(http_err)?;
        if n == 0 {
            return Err(RedError::Connect("proxy closed during CONNECT".into()));
        }
        headers.push(byte[0]);
        if headers.ends_with(b"\r\n\r\n") {
            break;
        }
        if headers.len() > 8192 {
            return Err(RedError::Connect("proxy CONNECT response too large".into()));
        }
    }

    let status_line = headers
        .split(|&b| b == b'\n')
        .next()
        .map(|l| String::from_utf8_lossy(l).trim().to_string())
        .unwrap_or_default();
    // "HTTP/1.1 200 Connection established" — accept any 2xx.
    let ok = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code));
    if !ok {
        return Err(RedError::Connect(format!(
            "HTTP proxy CONNECT refused: {status_line}"
        )));
    }
    Ok(())
}

/// Read a single byte (the SOCKS5 domain-address length prefix).
async fn read_u8(stream: &mut TcpStream) -> Result<u8> {
    let mut b = [0u8; 1];
    stream.read_exact(&mut b).await.map_err(socks_err)?;
    Ok(b[0])
}

/// Base64-encode `user:password` for an HTTP Basic `Proxy-Authorization` header.
/// Hand-rolled (a dependency would be overkill for one 4-line encoder).
fn basic_auth(user: &str, password: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = format!("{user}:{password}");
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Human message for a SOCKS5 reply code (RFC 1928 section 6).
fn socks_reply_message(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

/// A transport error during the SOCKS handshake -> transient `Connect`.
fn socks_err(e: std::io::Error) -> RedError {
    RedError::Connect(format!("SOCKS5 proxy error: {e}"))
}

/// A transport error during the HTTP CONNECT handshake -> transient `Connect`.
fn http_err(e: std::io::Error) -> RedError {
    RedError::Connect(format!("HTTP proxy error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_matches_rfc_examples() {
        // The classic RFC 7617 example.
        assert_eq!(
            basic_auth("Aladdin", "open sesame"),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
        assert_eq!(basic_auth("user", "pass"), "dXNlcjpwYXNz");
    }

    /// A minimal in-process SOCKS5 server that accepts no-auth, honours one
    /// domain-name CONNECT, and splices to a local echo server; proves the client
    /// handshake round-trips real bytes end to end.
    #[tokio::test]
    async fn socks5_forward_round_trips_bytes() {
        // Echo "database".
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        // SOCKS5 server: greeting -> no-auth; CONNECT -> dial the echo server. It
        // loops, since `Proxy::open` probes once (a throwaway handshake) before the
        // real forwarded connection arrives.
        let socks = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socks_addr = socks.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = socks.accept().await {
                tokio::spawn(async move {
                    let mut greeting = [0u8; 3];
                    c.read_exact(&mut greeting).await.unwrap();
                    c.write_all(&[0x05, 0x00]).await.unwrap(); // no auth
                    // CONNECT header VER CMD RSV ATYP.
                    let mut head = [0u8; 4];
                    c.read_exact(&mut head).await.unwrap();
                    assert_eq!(head[3], 0x03, "client uses domain addressing");
                    let len = read_u8(&mut c).await.unwrap() as usize;
                    let mut name = vec![0u8; len + 2]; // host + port
                    c.read_exact(&mut name).await.unwrap();
                    // Success reply with an empty IPv4 bound address.
                    c.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                        .await
                        .unwrap();
                    let mut target = TcpStream::connect(echo_addr).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut c, &mut target).await;
                });
            }
        });

        let proxy = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: socks_addr.port(),
            user: String::new(),
            password: String::new(),
        };
        let forward = Proxy::open(&proxy, "localhost", echo_addr.port())
            .await
            .expect("proxy opens");

        let mut client = TcpStream::connect(forward.local_addr()).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping", "bytes round-trip through the SOCKS5 forward");
    }
}
