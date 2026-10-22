//! Port forwards over an open SSH session: local (-L) and dynamic SOCKS5 (-D) listeners on
//! this machine, and remote (-R) listeners the server opens for us.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::Channel;
use russh::client::{Handle, Msg};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::Client;

/// One port forward, as `ssh -L`, `-R` or `-D` sets it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Forward {
    /// Listens on `bind:port` here; the server connects to `host:host_port`. An empty `bind`
    /// means 127.0.0.1, port 0 picks a free port.
    Local {
        bind: String,
        port: u16,
        host: String,
        host_port: u16,
    },
    /// The server listens on `bind:port`; this machine connects to `host:host_port`. An empty
    /// `bind` means the server's loopback, port 0 lets the server pick.
    Remote {
        bind: String,
        port: u16,
        host: String,
        host_port: u16,
    },
    /// A SOCKS5 proxy on `bind:port` here; the server connects wherever its clients ask.
    Dynamic { bind: String, port: u16 },
}

/// Where a remote forward's connections go, by the port the server listens on.
pub(crate) struct RemoteTarget {
    port: u32,
    host: String,
    host_port: u16,
}

/// The remote forwards of one session, read by its handler when the server opens a channel.
pub(crate) type Remotes = Arc<Mutex<Vec<RemoteTarget>>>;

/// The `host:port` a remote forward on server port `port` connects to.
pub(crate) fn remote_target(remotes: &Remotes, port: u32) -> Option<(String, u16)> {
    let remotes = remotes.lock().unwrap_or_else(|e| e.into_inner());
    remotes
        .iter()
        .find(|r| r.port == port)
        .map(|r| (r.host.clone(), r.host_port))
}

/// Starts every forward. Returns one result per forward (the listening port, or why it
/// failed) and the listener tasks, which the caller aborts when the session ends.
pub(crate) async fn start(
    handle: &Arc<Handle<Client>>,
    forwards: &[Forward],
    remotes: &Remotes,
) -> (Vec<Result<u16, String>>, Vec<JoinHandle<()>>) {
    let mut results = Vec::with_capacity(forwards.len());
    let mut tasks = Vec::new();
    for forward in forwards {
        let result = match forward {
            Forward::Local {
                bind,
                port,
                host,
                host_port,
            } => listen(bind, *port, handle, Some((host.clone(), *host_port))).await,
            Forward::Dynamic { bind, port } => listen(bind, *port, handle, None).await,
            Forward::Remote {
                bind,
                port,
                host,
                host_port,
            } => remote(handle, remotes, bind, *port, host, *host_port)
                .await
                .map(|port| (port, None)),
        };
        results.push(result.map(|(port, task)| {
            tasks.extend(task);
            port
        }));
    }
    (results, tasks)
}

/// Binds a local listener and serves it on a task. `target` None = SOCKS5.
async fn listen(
    bind: &str,
    port: u16,
    handle: &Arc<Handle<Client>>,
    target: Option<(String, u16)>,
) -> Result<(u16, Option<JoinHandle<()>>), String> {
    let bind = if bind.is_empty() { "127.0.0.1" } else { bind };
    let listener = TcpListener::bind((bind, port)).await.map_err(describe)?;
    let port = listener.local_addr().map_err(describe)?.port();
    let task = tokio::spawn(accept(listener, handle.clone(), target));
    Ok((port, Some(task)))
}

async fn accept(listener: TcpListener, handle: Arc<Handle<Client>>, target: Option<(String, u16)>) {
    loop {
        let (mut socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                // Usually one aborted connection; the pause keeps a lasting error from spinning.
                tracing::debug!(%err, "forward accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let (handle, target) = (handle.clone(), target.clone());
        tokio::spawn(async move {
            let socks = target.is_none();
            let (host, port) = match target {
                Some(target) => target,
                None => match socks_request(&mut socket).await {
                    Ok(target) => target,
                    Err(err) => {
                        tracing::debug!(%err, "SOCKS request refused");
                        return;
                    }
                },
            };
            let opened = handle
                .channel_open_direct_tcpip(
                    host.as_str(),
                    u32::from(port),
                    peer.ip().to_string(),
                    u32::from(peer.port()),
                )
                .await;
            match opened {
                Ok(channel) => {
                    if !socks || socks_reply(&mut socket, SOCKS_OK).await.is_ok() {
                        pipe(socket, channel).await;
                    }
                }
                Err(err) => {
                    tracing::debug!(%err, %host, port, "server could not open the forward");
                    if socks {
                        _ = socks_reply(&mut socket, SOCKS_FAILED).await;
                    }
                }
            }
        });
    }
}

/// Asks the server to listen and records where its connections go.
async fn remote(
    handle: &Handle<Client>,
    remotes: &Remotes,
    bind: &str,
    port: u16,
    host: &str,
    host_port: u16,
) -> Result<u16, String> {
    let bind = if bind.is_empty() { "localhost" } else { bind };
    let record = |port: u32| {
        remotes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(RemoteTarget {
                port,
                host: host.to_owned(),
                host_port,
            });
    };
    // A fixed port is recorded first: the server may send a connection right after it agrees.
    if port != 0 {
        record(u32::from(port));
    }
    match handle.tcpip_forward(bind, u32::from(port)).await {
        Ok(assigned) if port == 0 => {
            record(assigned);
            Ok(u16::try_from(assigned).unwrap_or_default())
        }
        Ok(_) => Ok(port),
        Err(err) => {
            if port != 0 {
                remotes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|r| r.port != u32::from(port));
            }
            Err(match err {
                russh::Error::RequestDenied => "refused by the server".into(),
                err => err.to_string(),
            })
        }
    }
}

/// Serves one connection the server opened for a remote forward.
pub(crate) fn serve_remote(channel: Channel<Msg>, host: String, port: u16) {
    tokio::spawn(async move {
        match TcpStream::connect((host.as_str(), port)).await {
            Ok(socket) => pipe(socket, channel).await,
            Err(err) => {
                tracing::debug!(%err, %host, port, "remote forward target unreachable");
                _ = channel.close().await;
            }
        }
    });
}

/// Copies both ways until either side closes.
async fn pipe(mut socket: TcpStream, channel: Channel<Msg>) {
    let mut stream = channel.into_stream();
    _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
}

/// Short reasons for the host editor.
fn describe(err: io::Error) -> String {
    match err.kind() {
        io::ErrorKind::AddrInUse => "port in use".into(),
        io::ErrorKind::PermissionDenied => "port not allowed".into(),
        io::ErrorKind::AddrNotAvailable => "address not available".into(),
        _ => err.to_string(),
    }
}

const SOCKS_OK: u8 = 0;
const SOCKS_FAILED: u8 = 1;
const SOCKS_BAD_COMMAND: u8 = 7;
const SOCKS_BAD_ADDRESS: u8 = 8;

/// Reads a SOCKS5 CONNECT request (no authentication) and returns where to connect.
async fn socks_request(socket: &mut TcpStream) -> io::Result<(String, u16)> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let mut head = [0u8; 2];
    socket.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(invalid("not a SOCKS5 client"));
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    socket.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        socket.write_all(&[5, 0xff]).await?;
        return Err(invalid("the client wants authentication"));
    }
    socket.write_all(&[5, 0]).await?;

    let mut request = [0u8; 4];
    socket.read_exact(&mut request).await?;
    if request[1] != 1 {
        socks_reply(socket, SOCKS_BAD_COMMAND).await?;
        return Err(invalid("only CONNECT is supported"));
    }
    let host = match request[3] {
        1 => {
            let mut ip = [0u8; 4];
            socket.read_exact(&mut ip).await?;
            Ipv4Addr::from(ip).to_string()
        }
        3 => {
            let mut name = vec![0u8; usize::from(socket.read_u8().await?)];
            socket.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| invalid("host name is not UTF-8"))?
        }
        4 => {
            let mut ip = [0u8; 16];
            socket.read_exact(&mut ip).await?;
            Ipv6Addr::from(ip).to_string()
        }
        _ => {
            socks_reply(socket, SOCKS_BAD_ADDRESS).await?;
            return Err(invalid("unknown address type"));
        }
    };
    let port = socket.read_u16().await?;
    Ok((host, port))
}

/// A SOCKS5 reply; the bound address is left zero, which clients ignore for CONNECT.
async fn socks_reply(socket: &mut TcpStream, code: u8) -> io::Result<()> {
    socket.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}
