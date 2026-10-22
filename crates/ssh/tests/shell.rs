//! End to end against an in-process russh server: host-key prompt, password and key login,
//! shell output into the terminal grid, input, resize and the exit code.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use maomi_ssh::{AuthMethod, ConnectOptions, Prompt, SshError, connect};
use maomi_term::{PtySession, SessionOptions, TermSize};
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{self, Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelId, Pty};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

const HOST_KEY: &str = include_str!("fixtures/host_ed25519");
const CLIENT_KEY: &str = include_str!("fixtures/client_ed25519");
const CLIENT_PUB: &str = include_str!("fixtures/client_ed25519.pub");

/// What the server saw, for the assertions.
#[derive(Default)]
struct Seen {
    pty: Option<(String, u32, u32)>,
    resizes: Vec<(u32, u32)>,
}

#[derive(Clone)]
struct TestServer {
    seen: Arc<Mutex<Seen>>,
    client_key: PublicKey,
}

impl server::Server for TestServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
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
        _channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
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

    /// Echoes input; a line "exit" ends the shell with code 3.
    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
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
    };
    let socket = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = socket.local_addr().unwrap().port();
    tokio::spawn(async move {
        _ = server.run_on_socket(config, &socket).await;
    });
    (port, seen)
}

fn options(port: u16, auth: Vec<AuthMethod>) -> ConnectOptions {
    ConnectOptions {
        host: "127.0.0.1".into(),
        port,
        user: "kit".into(),
        auth,
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
                Prompt::Password { reply, .. } => {
                    seen.lock().unwrap().push("password".into());
                    _ = reply.send(password.map(|p| Zeroizing::new(p.into()))).await;
                }
                Prompt::KeyPassphrase { retry, reply } => {
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
    let session = connect(options(port, vec![AuthMethod::Password(None)]), prompts)
        .await
        .unwrap();

    assert!(session.is_remote());
    assert_eq!(session.shell().display_name(), "ssh fennec");
    wait_for_text(&session, "welcome to fennec").await;
    assert_eq!(
        seen.lock().unwrap().pty,
        Some(("xterm-256color".into(), 100, 30))
    );
    assert_eq!(*log.lock().unwrap(), ["hostkey ssh-ed25519", "password"]);

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
    let session = connect(options(port, auth), prompts).await.unwrap();
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
