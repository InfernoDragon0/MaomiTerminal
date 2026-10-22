//! End to end against an in-process russh server: host-key prompt, password and key login,
//! shell output into the terminal grid, input, resize, the exit code, a jump host, the
//! three kinds of port forward, ssh-agent login and forwarding, and SFTP.

#[path = "support/memfs.rs"]
mod memfs;

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use maomi_ssh::{AuthMethod, ConnectOptions, Forward, Login, Prompt, Shell, SshError, connect};
use maomi_term::{PtySession, SessionOptions, TermSize};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, Pty};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use zeroize::Zeroizing;

const HOST_KEY: &str = include_str!("fixtures/host_ed25519");
const CLIENT_KEY: &str = include_str!("fixtures/client_ed25519");
const CLIENT_PUB: &str = include_str!("fixtures/client_ed25519.pub");

/// What the server saw, for the assertions.
#[derive(Default)]
struct Seen {
    pty: Option<(String, u32, u32)>,
    resizes: Vec<(u32, u32)>,
    /// What came back through a remote forward.
    remote: Option<String>,
    /// Keys the forwarded agent listed.
    agent_keys: Option<u32>,
}

#[derive(Clone)]
struct TestServer {
    seen: Arc<Mutex<Seen>>,
    client_key: PublicKey,
    /// This connection's shell; data on tunnel channels is not echoed.
    shell: Option<ChannelId>,
    /// Session channels not yet claimed by a subsystem.
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
    /// The SFTP subsystem's files.
    files: memfs::Tree,
}

impl server::Server for TestServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        Self {
            channels: Arc::default(),
            ..self.clone()
        }
    }
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == "kit" && password == "pw" {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(
            if user == "kit" && key.key_data() == self.client_key.key_data() {
                Auth::Accept
            } else {
                Auth::reject()
            },
        )
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().unwrap().insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let claimed = self.channels.lock().unwrap().remove(&channel);
        match claimed {
            Some(stream) if name == "sftp" => {
                session.channel_success(channel)?;
                russh_sftp::server::run(
                    stream.into_stream(),
                    memfs::MemFs::new(self.files.clone()),
                )
                .await;
            }
            _ => session.channel_failure(channel)?,
        }
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        term: &str,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.seen.lock().unwrap().pty = Some((term.to_owned(), cols, rows));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.shell = Some(channel);
        session.channel_success(channel)?;
        session.data(channel, &b"welcome to fennec\r\n$ "[..])?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _: ChannelId,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.seen.lock().unwrap().resizes.push((cols, rows));
        Ok(())
    }

    /// Connects to the requested address and pipes it, for jumps and -L / -D forwards.
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        // A refused connection rejects the channel by dropping the reply.
        if let Ok(mut socket) = TcpStream::connect(format!("{host}:{port}")).await {
            reply.accept().await;
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
            });
        }
        Ok(())
    }

    /// Agrees, then sends "ping" through one forwarded connection and records the answer.
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let (handle, address, port) = (session.handle(), address.to_owned(), *port);
        let seen = self.seen.clone();
        tokio::spawn(async move {
            let channel = handle
                .channel_open_forwarded_tcpip(address, port, "203.0.113.1", 5000)
                .await
                .unwrap();
            let mut stream = channel.into_stream();
            stream.write_all(b"ping").await.unwrap();
            let mut answer = [0u8; 4];
            stream.read_exact(&mut answer).await.unwrap();
            seen.lock().unwrap().remote = Some(String::from_utf8_lossy(&answer).into());
        });
        Ok(true)
    }

    /// Asks the forwarded agent for its keys and records how many it has.
    async fn agent_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let (handle, seen) = (session.handle(), self.seen.clone());
        tokio::spawn(async move {
            let mut stream = handle.channel_open_agent().await.unwrap().into_stream();
            // SSH_AGENTC_REQUEST_IDENTITIES, length-prefixed.
            stream.write_all(&[0, 0, 0, 1, 11]).await.unwrap();
            let mut head = [0u8; 9];
            stream.read_exact(&mut head).await.unwrap();
            assert_eq!(head[4], 12, "not an identities answer");
            let count = u32::from_be_bytes([head[5], head[6], head[7], head[8]]);
            seen.lock().unwrap().agent_keys = Some(count);
        });
        session.channel_success(channel)?;
        Ok(true)
    }

    /// Echoes input; a line "exit" ends the shell with code 3.
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.shell != Some(channel) {
            return Ok(());
        }
        if data.starts_with(b"exit") {
            session.exit_status_request(channel, 3)?;
            session.eof(channel)?;
            session.close(channel)?;
        } else {
            session.data(channel, format!("echo:{}", String::from_utf8_lossy(data)))?;
        }
        Ok(())
    }
}

/// Starts the server on a free port; returns the port and what it saw.
async fn start_server() -> (u16, Arc<Mutex<Seen>>) {
    let (port, seen, _) = start_server_with_files().await;
    (port, seen)
}

/// [`start_server`] and the SFTP files.
async fn start_server_with_files() -> (u16, Arc<Mutex<Seen>>, memfs::Tree) {
    let config = Arc::new(server::Config {
        keys: vec![PrivateKey::from_openssh(HOST_KEY).unwrap()],
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mut server = TestServer {
        seen: seen.clone(),
        client_key: PublicKey::from_openssh(CLIENT_PUB).unwrap(),
        shell: None,
        channels: Arc::default(),
        files: memfs::tree(),
    };
    let files = server.files.clone();
    let socket = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = socket.local_addr().unwrap().port();
    tokio::spawn(async move {
        _ = server.run_on_socket(config, &socket).await;
    });
    (port, seen, files)
}

fn login(port: u16, auth: Vec<AuthMethod>) -> Login {
    Login {
        host: "127.0.0.1".into(),
        port,
        user: "kit".into(),
        auth,
    }
}

fn options(port: u16, auth: Vec<AuthMethod>) -> ConnectOptions {
    ConnectOptions {
        route: vec![login(port, auth)],
        forwards: Vec::new(),
        forward_agent: false,
        session: SessionOptions {
            size: TermSize::new(100, 30),
            ..SessionOptions::default()
        },
        label: "ssh fennec".into(),
    }
}

/// Answers prompts: trusts the host key (recording its fingerprint) and replies to password /
/// passphrase prompts with the given answers.
fn answer_prompts(
    password: Option<&'static str>,
    passphrases: Vec<&'static str>,
) -> (async_channel::Sender<Prompt>, Arc<Mutex<Vec<String>>>) {
    let (tx, rx) = async_channel::unbounded::<Prompt>();
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        let mut passphrases = passphrases.into_iter();
        while let Ok(prompt) = rx.recv().await {
            match prompt {
                Prompt::HostKey { key, reply } => {
                    seen.lock()
                        .unwrap()
                        .push(format!("hostkey {}", key.algorithm));
                    assert!(key.fingerprint.starts_with("SHA256:"));
                    _ = reply.send(true).await;
                }
                Prompt::Password { hop, reply, .. } => {
                    seen.lock().unwrap().push(format!("password hop={hop}"));
                    _ = reply.send(password.map(|p| Zeroizing::new(p.into()))).await;
                }
                Prompt::KeyPassphrase { retry, reply, .. } => {
                    seen.lock()
                        .unwrap()
                        .push(format!("passphrase retry={retry}"));
                    let answer = passphrases.next().map(|p| Zeroizing::new(p.into()));
                    _ = reply.send(answer).await;
                }
                Prompt::KeyboardInteractive { reply, .. } => _ = reply.send(None).await,
            }
        }
    });
    (tx, log)
}

/// Waits until the grid shows `text`.
async fn wait_for_text(session: &PtySession, text: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if session.snapshot().text().contains(text) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "never saw {text:?}; grid:\n{}",
            session.snapshot().text()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn password_login_shell_resize_and_exit() {
    let (port, seen) = start_server().await;
    let (prompts, log) = answer_prompts(Some("pw"), Vec::new());
    let Shell { session, .. } = connect(options(port, vec![AuthMethod::Password(None)]), prompts)
        .await
        .unwrap();

    assert!(session.is_remote());
    assert_eq!(session.shell().display_name(), "ssh fennec");
    wait_for_text(&session, "welcome to fennec").await;
    assert_eq!(
        seen.lock().unwrap().pty,
        Some(("xterm-256color".into(), 100, 30))
    );
    assert_eq!(
        *log.lock().unwrap(),
        ["hostkey ssh-ed25519", "password hop=0"]
    );

    session.write("hello");
    wait_for_text(&session, "echo:hello").await;

    session.resize(TermSize::new(120, 40));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.lock().unwrap().resizes.contains(&(120, 40)) {
        assert!(Instant::now() < deadline, "resize never reached the server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let events = session.events();
    session.write("exit\r");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let event = tokio::time::timeout_at(deadline.into(), events.recv())
            .await
            .expect("no Exited event")
            .unwrap();
        if let maomi_term::TermEvent::Exited(code) = event {
            assert_eq!(code, Some(3));
            break;
        }
    }
    assert_eq!(session.exit_code(), Some(Some(3)));
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_key_asks_for_passphrase_again_after_a_wrong_one() {
    let (port, _) = start_server().await;
    let (prompts, log) = answer_prompts(None, vec!["wrong", "pp"]);
    let auth = vec![AuthMethod::PrivateKey {
        key: Zeroizing::new(CLIENT_KEY.into()),
        passphrase: None,
    }];
    let session = connect(options(port, auth), prompts).await.unwrap().session;
    wait_for_text(&session, "welcome to fennec").await;
    assert_eq!(
        *log.lock().unwrap(),
        [
            "hostkey ssh-ed25519",
            "passphrase retry=false",
            "passphrase retry=true"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_fails_and_refused_host_key_is_reported() {
    let (port, _) = start_server().await;
    let (prompts, _) = answer_prompts(None, Vec::new());
    let auth = vec![AuthMethod::Password(Some(Zeroizing::new("nope".into())))];
    let err = connect(options(port, auth), prompts).await.unwrap_err();
    assert!(matches!(err, SshError::AuthFailed), "{err}");

    let (tx, rx) = async_channel::unbounded::<Prompt>();
    tokio::spawn(async move {
        while let Ok(prompt) = rx.recv().await {
            if let Prompt::HostKey { reply, .. } = prompt {
                _ = reply.send(false).await;
            }
        }
    });
    let err = connect(options(port, vec![AuthMethod::Password(None)]), tx)
        .await
        .unwrap_err();
    assert!(matches!(err, SshError::HostKeyRejected), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn jump_host_carries_the_target_session() {
    let (port, _) = start_server().await;
    let (prompts, log) = answer_prompts(Some("pw"), Vec::new());
    // The server is its own jump host: the second hop goes through a direct-tcpip channel.
    let options = ConnectOptions {
        route: vec![
            login(port, vec![AuthMethod::Password(None)]),
            login(port, vec![AuthMethod::Password(None)]),
        ],
        ..options(port, Vec::new())
    };
    let session = connect(options, prompts).await.unwrap().session;
    wait_for_text(&session, "welcome to fennec").await;
    assert_eq!(
        *log.lock().unwrap(),
        [
            "hostkey ssh-ed25519",
            "password hop=0",
            "hostkey ssh-ed25519",
            "password hop=1"
        ]
    );
    session.write("through");
    wait_for_text(&session, "echo:through").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn jump_to_a_closed_port_names_both_hosts() {
    let (port, _) = start_server().await;
    let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let (prompts, _) = answer_prompts(Some("pw"), Vec::new());
    let options = ConnectOptions {
        route: vec![
            login(port, vec![AuthMethod::Password(None)]),
            login(closed_port, vec![AuthMethod::Password(None)]),
        ],
        ..options(port, Vec::new())
    };
    let err = connect(options, prompts).await.unwrap_err();
    assert!(matches!(err, SshError::Unreachable { .. }), "{err}");
}

/// Echoes every connection back to itself; returns its port.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = socket.split();
                _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    port
}

async fn round_trip(socket: &mut TcpStream, text: &[u8]) {
    socket.write_all(text).await.unwrap();
    let mut back = vec![0u8; text.len()];
    tokio::time::timeout(Duration::from_secs(5), socket.read_exact(&mut back))
        .await
        .expect("no echo")
        .unwrap();
    assert_eq!(back, text);
}

#[tokio::test(flavor = "multi_thread")]
async fn local_dynamic_and_remote_forwards() {
    let (port, seen) = start_server().await;
    let echo = echo_server().await;
    let (prompts, _) = answer_prompts(Some("pw"), Vec::new());
    let local = |port| Forward::Local {
        bind: String::new(),
        port,
        host: "127.0.0.1".into(),
        host_port: echo,
    };
    let options = ConnectOptions {
        forwards: vec![
            local(0),
            Forward::Dynamic {
                bind: String::new(),
                port: 0,
            },
            Forward::Remote {
                bind: String::new(),
                port: 9000,
                host: "127.0.0.1".into(),
                host_port: echo,
            },
            // The echo server holds this port already.
            local(echo),
        ],
        ..options(port, vec![AuthMethod::Password(None)])
    };
    let shell = connect(options, prompts).await.unwrap();
    let [Ok(local_port), Ok(socks_port), Ok(9000), Err(in_use)] = &shell.forwards[..] else {
        panic!("forwards: {:?}", shell.forwards);
    };
    assert_eq!(in_use, "port in use");

    let mut socket = TcpStream::connect(("127.0.0.1", *local_port))
        .await
        .unwrap();
    round_trip(&mut socket, b"over -L").await;

    let mut socks = TcpStream::connect(("127.0.0.1", *socks_port))
        .await
        .unwrap();
    socks.write_all(&[5, 1, 0]).await.unwrap();
    let mut method = [0u8; 2];
    socks.read_exact(&mut method).await.unwrap();
    assert_eq!(method, [5, 0]);
    let mut request = vec![5, 1, 0, 3, 9];
    request.extend_from_slice(b"127.0.0.1");
    request.extend_from_slice(&echo.to_be_bytes());
    socks.write_all(&request).await.unwrap();
    let mut reply = [0u8; 10];
    socks.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[1], 0, "SOCKS reply {reply:?}");
    round_trip(&mut socks, b"over -D").await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().remote.is_none() {
        assert!(Instant::now() < deadline, "nothing came back through -R");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(seen.lock().unwrap().remote.as_deref(), Some("ping"));

    // Closing the shell stops the local listeners.
    shell.session.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(("127.0.0.1", *local_port)).await.is_ok() {
        assert!(Instant::now() < deadline, "-L listener outlived the shell");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[derive(Clone)]
struct TestAgent;

impl russh::keys::agent::server::Agent for TestAgent {}

/// Starts an ssh-agent holding the client key and points maomi-ssh at it.
async fn start_agent() {
    use russh::keys::agent::client::AgentClient;

    #[cfg(windows)]
    let path = {
        use tokio::net::windows::named_pipe::ServerOptions;
        let name = format!(r"\\.\pipe\maomi-test-agent-{}", std::process::id());
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)
            .unwrap();
        let pipe = name.clone();
        let connections = futures::stream::unfold(first, move |server| {
            let pipe = pipe.clone();
            async move {
                let connected = server.connect().await.map(|()| server);
                Some((connected, ServerOptions::new().create(&pipe).unwrap()))
            }
        });
        tokio::spawn(russh::keys::agent::server::serve(
            Box::pin(connections),
            TestAgent,
        ));
        name
    };
    #[cfg(unix)]
    let path = {
        let path = std::env::temp_dir().join(format!("maomi-test-agent-{}", std::process::id()));
        _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let connections = futures::stream::unfold(listener, |listener| async move {
            let accepted = listener.accept().await.map(|(socket, _)| socket);
            Some((accepted, listener))
        });
        tokio::spawn(russh::keys::agent::server::serve(
            Box::pin(connections),
            TestAgent,
        ));
        path.display().to_string()
    };

    maomi_ssh::set_agent_path(Some(path.clone()));
    let key = russh::keys::decode_secret_key(CLIENT_KEY, Some("pp")).unwrap();
    #[cfg(windows)]
    let mut client = AgentClient::connect_named_pipe(&path).await.unwrap();
    #[cfg(unix)]
    let mut client = AgentClient::connect_uds(&path).await.unwrap();
    client.add_identity(&key, &[]).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_login_and_forwarding() {
    start_agent().await;
    let keys = maomi_ssh::agent_keys().await.unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].algorithm, "ssh-ed25519");

    let (port, seen) = start_server().await;
    let (prompts, log) = answer_prompts(None, Vec::new());
    let options = ConnectOptions {
        forward_agent: true,
        ..options(port, vec![AuthMethod::Agent])
    };
    let session = connect(options, prompts).await.unwrap().session;
    wait_for_text(&session, "welcome to fennec").await;
    // The agent signed: no password or passphrase was asked.
    assert_eq!(*log.lock().unwrap(), ["hostkey ssh-ed25519"]);

    let deadline = Instant::now() + Duration::from_secs(5);
    while seen.lock().unwrap().agent_keys.is_none() {
        assert!(
            Instant::now() < deadline,
            "the forwarded agent never answered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(seen.lock().unwrap().agent_keys, Some(1));
}

/// A scratch folder under the build temp folder.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("maomi-sftp-{name}-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn sftp_lists_uploads_downloads_renames_and_deletes() {
    use maomi_ssh::Progress;
    use memfs::{HOME, Node};

    let (port, _, files) = start_server_with_files().await;
    let (prompts, _) = answer_prompts(Some("pw"), Vec::new());
    let shell = connect(options(port, vec![AuthMethod::Password(None)]), prompts)
        .await
        .unwrap();
    let sftp = shell.handle.sftp().await.unwrap();
    assert_eq!(sftp.home().await.unwrap(), HOME);

    // A folder with a file bigger than one chunk, and a nested folder.
    let local = scratch("up").join("project");
    std::fs::create_dir_all(local.join("src")).unwrap();
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(local.join("big.bin"), &big).unwrap();
    std::fs::write(local.join("src").join("main.rs"), b"fn main() {}").unwrap();

    let progress = Arc::new(Progress::default());
    sftp.upload(local.clone(), HOME, progress.clone())
        .await
        .unwrap();
    let total = progress.total.load(Ordering::Relaxed);
    assert_eq!(total, 200_000 + 12);
    assert_eq!(progress.done.load(Ordering::Relaxed), total);
    assert_eq!(
        files.lock().unwrap().get("/home/kit/project/big.bin"),
        Some(&Node::File {
            data: big.clone(),
            mode: 0o644
        })
    );

    let mut names: Vec<_> = sftp
        .list(&format!("{HOME}/project"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.is_dir, e.size))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            ("big.bin".to_owned(), false, 200_000),
            ("src".to_owned(), true, 0)
        ]
    );

    sftp.set_mode("/home/kit/project/big.bin", 0o600)
        .await
        .unwrap();
    let listed = sftp.list("/home/kit/project").await.unwrap();
    let big_entry = listed.iter().find(|e| e.name == "big.bin").unwrap();
    assert_eq!(big_entry.mode, Some(0o600));

    sftp.rename("/home/kit/project", "/home/kit/renamed")
        .await
        .unwrap();
    sftp.create_dir("/home/kit/renamed/empty").await.unwrap();

    let down = scratch("down");
    sftp.download(
        "/home/kit/renamed",
        down.clone(),
        Arc::new(Progress::default()),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(down.join("renamed").join("big.bin")).unwrap(),
        big
    );
    assert_eq!(
        std::fs::read(down.join("renamed").join("src").join("main.rs")).unwrap(),
        b"fn main() {}"
    );
    assert!(down.join("renamed").join("empty").is_dir());

    sftp.remove("/home/kit/renamed").await.unwrap();
    assert!(
        files
            .lock()
            .unwrap()
            .keys()
            .all(|p| !p.starts_with("/home/kit/renamed")),
        "folder left behind"
    );
    let err = sftp.list("/home/kit/renamed").await.unwrap_err();
    assert!(matches!(err, SshError::Files(_)), "{err}");

    // A cancelled download leaves no partial file.
    let cancelled = Arc::new(Progress::default());
    cancelled.cancel.store(true, Ordering::Relaxed);
    let local_file = scratch("one").join("big.bin");
    sftp.upload(local.join("big.bin"), HOME, Arc::new(Progress::default()))
        .await
        .unwrap();
    let err = sftp
        .download(
            "/home/kit/big.bin",
            local_file.parent().unwrap().to_path_buf(),
            cancelled,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SshError::Cancelled), "{err}");
    assert!(!local_file.exists());
}
