//! SSH jump-host tunneling for network connections (the `ssh -L` model).
//!
//! When a [`ConnectionConfig`] carries an [`SshConfig`], the service opens one of
//! these *before* the driver and points the driver at the tunnel's local port
//! instead of the real host. The drivers stay SSH-unaware; they just dial a
//! `127.0.0.1:<port>` DSN (see [`ConnectionConfig::local_dsn`]).
//!
//! The forward is **persistent**, not a single piped socket: a Postgres driver
//! grows a small fetch-connection pool and `mysql_async` keeps a connection pool,
//! so the database side opens connections repeatedly over the session's life. We
//! therefore bind a local listener and, on *each* inbound accept, open a fresh
//! `direct-tcpip` channel to the database host as seen from the jump host.
//!
//! Lifetime: a [`Tunnel`] is owned by the session it serves and torn down with it.
//! Dropping it aborts the accept loop, which drops the SSH handle and closes the
//! session; in-flight forwarded sockets then error out and finish on their own.
//!
//! Host keys are verified against `~/.ssh/known_hosts`, fail-closed: an unknown
//! host or a changed key fails the connect with a clear, user-correctable message
//! rather than trusting blindly.

use std::net::SocketAddr;
use std::sync::Arc;

use red_core::{RedError, Result, SshAuth, SshConfig};
use russh::keys::ssh_key;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// A live local port-forward through an SSH jump host. Hold it for as long as the
/// database connection that rides it; dropping it tears the tunnel down.
pub(crate) struct Tunnel {
    local_addr: SocketAddr,
    /// The accept loop. Owns the SSH handle, so aborting it (on drop) closes the
    /// SSH session.
    accept_loop: JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept_loop.abort();
    }
}

/// Why a server's host key was rejected, surfaced from the connect [`Handler`]
/// through `russh`'s `H::Error` channel so we can render a precise message.
#[derive(Debug)]
enum HandlerError {
    /// A `russh` protocol/transport error during the handshake.
    Ssh(russh::Error),
    /// The host isn't in `~/.ssh/known_hosts`. Carries its fingerprint (to show)
    /// and the OpenSSH-encoded key (to append to known_hosts if the user trusts it).
    HostUnknown {
        host: String,
        port: u16,
        fingerprint: String,
        key: String,
    },
    /// The host *is* in `known_hosts` but the key differs: a possible MITM.
    HostMismatch { host: String },
    /// `known_hosts` couldn't be read for some other reason.
    Other(String),
}

impl From<russh::Error> for HandlerError {
    fn from(e: russh::Error) -> Self {
        HandlerError::Ssh(e)
    }
}

/// Verifies the server's host key against `~/.ssh/known_hosts` during the SSH
/// handshake. Carries the target host/port so the lookup can be keyed correctly.
struct Handler {
    host: String,
    port: u16,
}

impl russh::client::Handler for Handler {
    type Error = HandlerError;

    async fn check_server_key(
        &mut self,
        key: &ssh_key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        match russh::keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => Err(HandlerError::HostUnknown {
                host: self.host.clone(),
                port: self.port,
                fingerprint: key.fingerprint(Default::default()).to_string(),
                // OpenSSH-encoded so it can be appended to known_hosts later; on
                // the rare encode error, fall back to empty (no trust offer).
                key: key.to_openssh().unwrap_or_default(),
            }),
            Err(russh::keys::Error::KeyChanged { .. }) => Err(HandlerError::HostMismatch {
                host: self.host.clone(),
            }),
            Err(e) => Err(HandlerError::Other(e.to_string())),
        }
    }
}

impl Tunnel {
    /// The local address the driver should dial. Always on `127.0.0.1`.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Open the SSH session, authenticate, bind a local listener, and start
    /// forwarding to `(remote_host, remote_port)` as reached *from the jump host*.
    pub(crate) async fn open(
        ssh: &SshConfig,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Tunnel> {
        let config = Arc::new(russh::client::Config::default());
        let handler = Handler {
            host: ssh.host.clone(),
            port: ssh.port,
        };
        let mut handle = russh::client::connect(config, (ssh.host.as_str(), ssh.port), handler)
            .await
            .map_err(map_handler_err)?;

        authenticate(&mut handle, ssh).await?;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| RedError::Connect(format!("could not open local tunnel port: {e}")))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| RedError::Connect(format!("could not read local tunnel port: {e}")))?;

        let remote_host = remote_host.to_string();
        let local_port = local_addr.port() as u32;
        let accept_loop = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break; // listener closed
                };
                let channel = match handle
                    .channel_open_direct_tcpip(
                        remote_host.clone(),
                        remote_port as u32,
                        "127.0.0.1",
                        local_port,
                    )
                    .await
                {
                    Ok(channel) => channel,
                    Err(e) => {
                        tracing::warn!("ssh tunnel: opening forward channel failed: {e}");
                        continue;
                    }
                };
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                });
            }
        });

        Ok(Tunnel {
            local_addr,
            accept_loop,
        })
    }
}

/// Authenticate `handle` per the configured [`SshAuth`] mode. A `false` result
/// from the server (rather than a transport error) maps to [`RedError::Auth`], so
/// the connect path treats it as fatal and stops retrying.
async fn authenticate(handle: &mut russh::client::Handle<Handler>, ssh: &SshConfig) -> Result<()> {
    let ok = match &ssh.auth {
        SshAuth::Password => handle
            .authenticate_password(ssh.user.clone(), ssh.password.clone())
            .await
            .map_err(map_ssh_err)?
            .success(),
        SshAuth::Key { path } => {
            let passphrase = (!ssh.passphrase.is_empty()).then_some(ssh.passphrase.as_str());
            let key = russh::keys::load_secret_key(path, passphrase)
                .map_err(|e| RedError::Auth(format!("could not load SSH key {path}: {e}")))?;
            let key = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
            handle
                .authenticate_publickey(ssh.user.clone(), key)
                .await
                .map_err(map_ssh_err)?
                .success()
        }
        SshAuth::Agent => authenticate_agent(handle, &ssh.user).await?,
    };
    if ok {
        Ok(())
    } else {
        Err(RedError::Auth("SSH authentication failed".into()))
    }
}

/// Try every identity a running ssh-agent offers, in order, until one is accepted.
async fn authenticate_agent(
    handle: &mut russh::client::Handle<Handler>,
    user: &str,
) -> Result<bool> {
    // ssh-agent transport is platform-specific: a Unix socket via `$SSH_AUTH_SOCK`
    // on Unix, the OpenSSH named pipe on Windows. Only the connect differs; the
    // identity walk below is generic over the agent's stream type.
    #[cfg(unix)]
    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| RedError::Auth(format!("no SSH agent available: {e}")))?;
    #[cfg(windows)]
    let mut agent =
        russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
            .await
            .map_err(|e| RedError::Auth(format!("no SSH agent available: {e}")))?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| RedError::Auth(format!("SSH agent error: {e}")))?;
    if identities.is_empty() {
        return Err(RedError::Auth("SSH agent has no identities loaded".into()));
    }
    for id in identities {
        let pubkey = id.public_key().into_owned();
        if let Ok(result) = handle
            .authenticate_publickey_with(user.to_string(), pubkey, None, &mut agent)
            .await
            && result.success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Map a `russh` transport error to a transient [`RedError::Connect`].
fn map_ssh_err(e: russh::Error) -> RedError {
    RedError::Connect(format!("SSH error: {e}"))
}

/// Map the connect-time [`HandlerError`] onto a `RedError`. Host-key problems are
/// `Auth` (fatal, user must act); transport problems are transient `Connect`.
fn map_handler_err(e: HandlerError) -> RedError {
    match e {
        HandlerError::Ssh(e) => RedError::Connect(format!("SSH connection failed: {e}")),
        HandlerError::Other(msg) => RedError::Connect(format!("SSH known_hosts error: {msg}")),
        HandlerError::HostUnknown {
            host,
            port,
            fingerprint,
            key,
        } => RedError::SshHostUnknown {
            host,
            port,
            fingerprint,
            key,
        },
        HandlerError::HostMismatch { host } => RedError::Auth(format!(
            "SSH host key for {host} does not match ~/.ssh/known_hosts: possible \
             man-in-the-middle. Connection refused."
        )),
    }
}

/// Append a server's host key to `~/.ssh/known_hosts`: the "trust this host"
/// action behind an unknown-host connect failure. `key` is the OpenSSH-encoded
/// public key carried by [`RedError::SshHostUnknown`].
pub(crate) fn trust_host(host: &str, port: u16, key: &str) -> Result<()> {
    let key = ssh_key::PublicKey::from_openssh(key)
        .map_err(|e| RedError::Connect(format!("malformed SSH host key: {e}")))?;
    russh::keys::known_hosts::learn_known_hosts(host, port, &key)
        .map_err(|e| RedError::Connect(format!("could not update ~/.ssh/known_hosts: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway Ed25519 host key for the in-process SSH server below. Generated
    /// for tests only; never used anywhere real.
    const TEST_HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACD8Wa/iX3qLQqdMR/aXzOAmutfnI1aLE/oUJYkLuqZZlAAAAJBjOv+EYzr/
hAAAAAtzc2gtZWQyNTUxOQAAACD8Wa/iX3qLQqdMR/aXzOAmutfnI1aLE/oUJYkLuqZZlA
AAAEAsgG66AZ1coZRS0N1OW3YSKfVp76vXarHs06agqv8p5/xZr+JfeotCp0xH9pfM4Ca6
1+cjVosT+hQliQu6plmUAAAACHJlZC10ZXN0AQIDBAU=
-----END OPENSSH PRIVATE KEY-----
";

    /// The in-process SSH server side: accept any password and forward each
    /// direct-tcpip channel to the address the client asked for (our echo server).
    struct TestServer;

    impl russh::server::Handler for TestServer {
        type Error = russh::Error;

        async fn auth_password(
            &mut self,
            _user: &str,
            _password: &str,
        ) -> std::result::Result<russh::server::Auth, Self::Error> {
            Ok(russh::server::Auth::Accept)
        }

        async fn channel_open_direct_tcpip(
            &mut self,
            channel: russh::Channel<russh::server::Msg>,
            host: &str,
            port: u32,
            _originator_address: &str,
            _originator_port: u32,
            _session: &mut russh::server::Session,
        ) -> std::result::Result<bool, Self::Error> {
            let addr = format!("{host}:{port}");
            tokio::spawn(async move {
                if let Ok(mut target) = tokio::net::TcpStream::connect(addr).await {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
                }
            });
            Ok(true)
        }
    }

    #[tokio::test]
    #[ignore = "spins up an in-process SSH server; run with --ignored"]
    async fn forwards_bytes_through_the_tunnel() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};

        // 1. An echo "database" the tunnel will forward to.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 || sock.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        // 2. The SSH server, keyed with our fixture host key.
        let host_key = russh::keys::decode_secret_key(TEST_HOST_KEY, None).unwrap();
        let server_pub = host_key.public_key().clone();
        let server_config = Arc::new(russh::server::Config {
            keys: vec![host_key],
            ..Default::default()
        });
        let ssh_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_port = ssh_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = ssh_listener.accept().await
                && let Ok(session) =
                    russh::server::run_stream(server_config, stream, TestServer).await
            {
                let _ = session.await;
            }
        });

        // 3. Trust the server's key by pointing HOME at a temp known_hosts.
        let home = std::env::temp_dir().join(format!("red-ssh-test-{}", std::process::id()));
        let known_hosts = home.join(".ssh").join("known_hosts");
        let _ = std::fs::remove_file(&known_hosts);
        russh::keys::known_hosts::learn_known_hosts_path(
            "127.0.0.1",
            ssh_port,
            &server_pub,
            &known_hosts,
        )
        .unwrap();
        // SAFETY: the only test mutating HOME, and it's `#[ignore]` (run alone).
        #[allow(
            unsafe_code,
            reason = "isolated #[ignore] test mutates HOME single-threaded"
        )]
        unsafe {
            std::env::set_var("HOME", &home)
        };

        // 4. Open the tunnel and round-trip bytes through the forward.
        let ssh = SshConfig {
            host: "127.0.0.1".into(),
            port: ssh_port,
            user: "tester".into(),
            auth: SshAuth::Password,
            password: "hunter2".into(),
            passphrase: String::new(),
        };
        let tunnel = Tunnel::open(&ssh, "127.0.0.1", echo_port)
            .await
            .expect("tunnel opens");

        let mut client = TcpStream::connect(tunnel.local_addr()).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping", "bytes round-trip through the SSH tunnel");
    }

    #[test]
    fn unknown_host_carries_trust_payload() {
        // An unknown host maps to the structured `SshHostUnknown` (not plain
        // `Auth`) so the UI can offer "trust & retry" with the fingerprint + key.
        let unknown = map_handler_err(HandlerError::HostUnknown {
            host: "bastion".into(),
            port: 2222,
            fingerprint: "SHA256:abc".into(),
            key: "ssh-ed25519 AAAA...".into(),
        });
        match unknown {
            RedError::SshHostUnknown {
                host,
                port,
                fingerprint,
                key,
            } => {
                assert_eq!(host, "bastion");
                assert_eq!(port, 2222);
                assert_eq!(fingerprint, "SHA256:abc");
                assert_eq!(key, "ssh-ed25519 AAAA...");
            }
            other => panic!("expected SshHostUnknown, got {other:?}"),
        }
    }

    #[test]
    fn changed_host_key_is_a_fatal_auth_error() {
        // A *changed* key is a hard stop (possible MITM): fatal `Auth`, no trust.
        let mismatch = map_handler_err(HandlerError::HostMismatch {
            host: "bastion".into(),
        });
        assert!(matches!(mismatch, RedError::Auth(_)));
        assert!(mismatch.to_string().contains("man-in-the-middle"));
    }

    #[test]
    fn transport_problems_are_transient_connect_errors() {
        // A handshake/transport failure is retryable, so it must NOT be `Auth`.
        let err = map_handler_err(HandlerError::Other("known_hosts unreadable".into()));
        assert!(matches!(err, RedError::Connect(_)));
    }
}
