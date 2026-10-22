//! SSH client for remote shells. Has no GPUI dependency.
//!
//! russh runs on one tokio thread owned by this crate. [`connect`] returns a [`Shell`] whose
//! [`PtySession`] the UI renders like a local shell. Questions for the user (trust this host
//! key, the password, a key passphrase, keyboard-interactive prompts) arrive as [`Prompt`]s
//! carrying a reply channel; dropping the prompt receiver cancels the connection.
//!
//! A connection may hop through jump hosts (ProxyJump): each hop is a full SSH session
//! tunnelled through a `direct-tcpip` channel of the one before it.

mod agent;
mod bridge;
mod forward;
mod keys;
mod known_hosts;
mod sftp;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use maomi_term::{PtySession, RemoteIo, SessionOptions};
use russh::Channel;
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse, Msg};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use zeroize::Zeroizing;

pub use agent::{AgentKey, agent_keys, set_agent_path};
pub use bridge::pty_size;
pub use forward::Forward;
pub use keys::{KeyInfo, KeyType, generate_key, import_key, key_info};
pub use known_hosts::{KnownHost, KnownHosts, Trust};
pub use sftp::{Progress, RemoteEntry, Sftp, SshHandle, base_name, join as join_remote};

/// Terminal type announced to the server.
const TERM: &str = "xterm-256color";
/// Wrong key passphrases allowed before giving up.
const PASSPHRASE_TRIES: usize = 3;

/// Where to connect and how to log in.
pub struct ConnectOptions {
    /// Jump hosts in order, then the target; each hop is reached through the one before it.
    pub route: Vec<Login>,
    /// Started once the shell is open; they stop when it closes.
    pub forwards: Vec<Forward>,
    /// Lets the target use the local ssh-agent (`ssh -A`).
    pub forward_agent: bool,
    /// Size, scrollback and cursor for the terminal; the shell fields do not apply.
    pub session: SessionOptions,
    /// Shown as the shell name, e.g. "ssh fennec".
    pub label: String,
}

/// One server on the route and how to log in to it.
pub struct Login {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Tried in order until one succeeds.
    pub auth: Vec<AuthMethod>,
}

/// An open remote shell.
#[derive(Debug)]
pub struct Shell {
    pub session: PtySession,
    /// One per [`ConnectOptions::forwards`], in order: the port it listens on, or why it
    /// failed.
    pub forwards: Vec<Result<u16, String>>,
    /// The connection, for SFTP.
    pub handle: SshHandle,
}

pub enum AuthMethod {
    /// None = ask with [`Prompt::Password`].
    Password(Option<Zeroizing<String>>),
    /// An OpenSSH or PEM private key. An encrypted key without `passphrase` asks with
    /// [`Prompt::KeyPassphrase`].
    PrivateKey {
        key: Zeroizing<String>,
        passphrase: Option<Zeroizing<String>>,
    },
    KeyboardInteractive,
    /// Every key in the running ssh-agent; skipped when there is no agent.
    Agent,
}

/// The server's host key, for the trust question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    /// e.g. "ssh-ed25519".
    pub algorithm: String,
    /// "SHA256:…", as OpenSSH prints it.
    pub fingerprint: String,
    /// The public key in OpenSSH format, for a known_hosts entry.
    pub openssh: String,
}

/// Answer channel of a [`Prompt`]; dropping it cancels the connection.
pub type Reply<T> = async_channel::Sender<T>;

/// A question for the user while connecting. `hop` is the index in
/// [`ConnectOptions::route`] of the server asking.
pub enum Prompt {
    /// Reply true to trust the key and go on. The caller checks its known hosts first and
    /// may answer without asking.
    HostKey { key: HostKey, reply: Reply<bool> },
    /// Reply None to cancel.
    Password {
        hop: usize,
        user: String,
        host: String,
        reply: Reply<Option<Zeroizing<String>>>,
    },
    /// The private key is encrypted. `retry` is true after a wrong passphrase.
    KeyPassphrase {
        hop: usize,
        retry: bool,
        reply: Reply<Option<Zeroizing<String>>>,
    },
    /// One answer per prompt, in order; `echo` false means hide the typing.
    KeyboardInteractive {
        hop: usize,
        name: String,
        instructions: String,
        prompts: Vec<KeyboardPrompt>,
        reply: Reply<Option<Vec<Zeroizing<String>>>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardPrompt {
    pub text: String,
    pub echo: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("cannot reach {host}: {source}")]
    Connect {
        host: String,
        source: std::io::Error,
    },
    #[error("cannot reach {host} through {via}: {reason}")]
    Unreachable {
        host: String,
        via: String,
        reason: String,
    },
    #[error("no host to connect to")]
    NoHost,
    #[error("the host key was not trusted")]
    HostKeyRejected,
    #[error("authentication failed")]
    AuthFailed,
    #[error("cancelled")]
    Cancelled,
    #[error("ssh-agent: {0}")]
    Agent(String),
    #[error("{0}")]
    Files(String),
    #[error("private key: {0}")]
    Key(String),
    #[error("the private key is encrypted; enter its passphrase")]
    KeyEncrypted,
    #[error("SSH: {0}")]
    Ssh(#[from] russh::Error),
    #[error("terminal: {0}")]
    Terminal(String),
}

/// Connects along the route, logs in, opens a shell with a PTY and starts the forwards. Works
/// from any async executor: the SSH work runs on this crate's tokio thread.
pub async fn connect(
    options: ConnectOptions,
    prompts: async_channel::Sender<Prompt>,
) -> Result<Shell, SshError> {
    on_runtime(open_shell(options, prompts)).await
}

/// Runs `work` on the SSH thread and waits for it from any executor.
pub(crate) async fn on_runtime<T: Send + 'static>(
    work: impl Future<Output = Result<T, SshError>> + Send + 'static,
) -> Result<T, SshError> {
    let (done, result) = async_channel::bounded(1);
    runtime().spawn(async move {
        _ = done.send(work.await).await;
    });
    result.recv().await.unwrap_or(Err(SshError::Cancelled))
}

/// The crate's tokio runtime: one thread, started on first use.
fn runtime() -> &'static tokio::runtime::Handle {
    static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime for SSH");
        let handle = runtime.handle().clone();
        thread::Builder::new()
            .name("maomi-ssh".into())
            .spawn(move || runtime.block_on(std::future::pending::<()>()))
            .expect("SSH runtime thread");
        handle
    })
}

async fn open_shell(
    options: ConnectOptions,
    prompts: async_channel::Sender<Prompt>,
) -> Result<Shell, SshError> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..client::Config::default()
    });
    let rejected = Arc::new(AtomicBool::new(false));
    let remotes = forward::Remotes::default();
    // Every hop logged in so far; the last one carries the next.
    let mut hops: Vec<Handle<Client>> = Vec::with_capacity(options.route.len());
    for (hop, login) in options.route.iter().enumerate() {
        let handler = Client {
            host: login.host.clone(),
            port: login.port,
            prompts: prompts.clone(),
            rejected: rejected.clone(),
            remotes: remotes.clone(),
            forward_agent: options.forward_agent && hop + 1 == options.route.len(),
        };
        let connected = match hops.last() {
            None => {
                client::connect(config.clone(), (login.host.as_str(), login.port), handler).await
            }
            Some(jump) => {
                let channel = jump
                    .channel_open_direct_tcpip(
                        login.host.as_str(),
                        u32::from(login.port),
                        "127.0.0.1",
                        0,
                    )
                    .await
                    .map_err(|err| SshError::Unreachable {
                        host: login.host.clone(),
                        via: options.route[hop - 1].host.clone(),
                        reason: err.to_string(),
                    })?;
                client::connect_stream(config.clone(), channel.into_stream(), handler).await
            }
        };
        let mut handle = match connected {
            Ok(handle) => handle,
            Err(_) if rejected.load(Ordering::Acquire) => return Err(SshError::HostKeyRejected),
            Err(russh::Error::IO(source)) => {
                return Err(SshError::Connect {
                    host: login.host.clone(),
                    source,
                });
            }
            Err(err) => return Err(err.into()),
        };
        authenticate(&mut handle, hop, login, &prompts).await?;
        hops.push(handle);
    }
    let target = Arc::new(hops.pop().ok_or(SshError::NoHost)?);

    let channel = target.channel_open_session().await?;
    let size = pty_size(options.session.size);
    channel
        .request_pty(true, TERM, size.0, size.1, size.2, size.3, &[])
        .await?;
    if options.forward_agent {
        channel.agent_forward(false).await?;
    }
    channel.request_shell(true).await?;
    let (forwards, listeners) = forward::start(&target, &options.forwards, &remotes).await;

    let handle = SshHandle(target.clone());
    let link = bridge::Link {
        target,
        jumps: hops,
        listeners,
    };
    let parts = bridge::start(link, channel);
    let io = RemoteIo {
        reader: Box::new(parts.reader),
        writer: Box::new(parts.writer),
        control: Box::new(parts.control),
        exit: parts.exit,
        label: options.label,
    };
    let session = PtySession::attach(io, options.session)
        .map_err(|err| SshError::Terminal(err.to_string()))?;
    Ok(Shell {
        session,
        forwards,
        handle,
    })
}

async fn authenticate(
    handle: &mut Handle<Client>,
    hop: usize,
    login: &Login,
    prompts: &async_channel::Sender<Prompt>,
) -> Result<(), SshError> {
    for method in &login.auth {
        let ok = match method {
            AuthMethod::Password(password) => {
                let password = match password {
                    Some(password) => password.clone(),
                    None => ask(prompts, |reply| Prompt::Password {
                        hop,
                        user: login.user.clone(),
                        host: login.host.clone(),
                        reply,
                    })
                    .await?
                    .ok_or(SshError::Cancelled)?,
                };
                handle
                    .authenticate_password(login.user.as_str(), password.as_str())
                    .await?
                    .success()
            }
            AuthMethod::PrivateKey { key, passphrase } => {
                let key = load_key(hop, key, passphrase.as_ref(), prompts).await?;
                let hash = handle.best_supported_rsa_hash().await?.flatten();
                handle
                    .authenticate_publickey(
                        login.user.as_str(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await?
                    .success()
            }
            AuthMethod::KeyboardInteractive => {
                keyboard_interactive(handle, hop, &login.user, prompts).await?
            }
            AuthMethod::Agent => agent::login(handle, &login.user).await?,
        };
        if ok {
            return Ok(());
        }
    }
    Err(SshError::AuthFailed)
}

/// Decodes a private key, asking for its passphrase when it is encrypted.
async fn load_key(
    hop: usize,
    key: &str,
    passphrase: Option<&Zeroizing<String>>,
    prompts: &async_channel::Sender<Prompt>,
) -> Result<PrivateKey, SshError> {
    let mut passphrase = passphrase.cloned();
    for attempt in 0..=PASSPHRASE_TRIES {
        match russh::keys::decode_secret_key(key, passphrase.as_ref().map(|p| p.as_str())) {
            Ok(key) => return Ok(key),
            Err(err) if attempt < PASSPHRASE_TRIES && needs_passphrase(&err, &passphrase) => {
                let retry = passphrase.is_some();
                passphrase = Some(
                    ask(prompts, |reply| Prompt::KeyPassphrase { hop, retry, reply })
                        .await?
                        .ok_or(SshError::Cancelled)?,
                );
            }
            Err(err) => return Err(SshError::Key(err.to_string())),
        }
    }
    Err(SshError::Key("wrong passphrase".into()))
}

/// Whether decoding failed for want of the right passphrase. A wrong one shows up as a
/// decryption (OpenSSH format) or corrupt-key (PEM) error.
fn needs_passphrase(err: &russh::keys::Error, passphrase: &Option<Zeroizing<String>>) -> bool {
    use russh::keys::{Error, ssh_key};
    match err {
        Error::KeyIsEncrypted => true,
        Error::KeyIsCorrupt | Error::SshKey(ssh_key::Error::Crypto) => passphrase.is_some(),
        _ => false,
    }
}

async fn keyboard_interactive(
    handle: &mut Handle<Client>,
    hop: usize,
    user: &str,
    prompts: &async_channel::Sender<Prompt>,
) -> Result<bool, SshError> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await?;
    loop {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest {
                name,
                instructions,
                prompts: questions,
            } => {
                // Servers send empty rounds; answer them without bothering the user.
                let answers: Vec<String> = if questions.is_empty() {
                    Vec::new()
                } else {
                    let questions = questions
                        .into_iter()
                        .map(|p| KeyboardPrompt {
                            text: p.prompt,
                            echo: p.echo,
                        })
                        .collect();
                    ask(prompts, |reply| Prompt::KeyboardInteractive {
                        hop,
                        name,
                        instructions,
                        prompts: questions,
                        reply,
                    })
                    .await?
                    .ok_or(SshError::Cancelled)?
                    .iter()
                    .map(|a| a.to_string())
                    .collect()
                };
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await?;
            }
        }
    }
}

/// Sends a prompt and waits for its reply.
async fn ask<T>(
    prompts: &async_channel::Sender<Prompt>,
    make: impl FnOnce(Reply<T>) -> Prompt,
) -> Result<T, SshError> {
    let (reply, answer) = async_channel::bounded(1);
    prompts
        .send(make(reply))
        .await
        .map_err(|_| SshError::Cancelled)?;
    answer.recv().await.map_err(|_| SshError::Cancelled)
}

/// russh's per-connection callbacks.
struct Client {
    host: String,
    port: u16,
    prompts: async_channel::Sender<Prompt>,
    /// Set when the user refused the host key, so the error says so.
    rejected: Arc<AtomicBool>,
    /// Where the server's remote-forward connections go.
    remotes: forward::Remotes,
    /// Accept the server's requests to reach the local ssh-agent.
    forward_agent: bool,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        let key = HostKey {
            host: self.host.clone(),
            port: self.port,
            algorithm: key.algorithm().as_str().to_owned(),
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
            openssh: key.to_openssh().unwrap_or_default(),
        };
        let trusted = ask(&self.prompts, |reply| Prompt::HostKey { key, reply })
            .await
            .unwrap_or(false);
        if !trusted {
            self.rejected.store(true, Ordering::Release);
        }
        Ok(trusted)
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // A port we never asked for is rejected by dropping the reply.
        if let Some((host, port)) = forward::remote_target(&self.remotes, connected_port) {
            reply.accept().await;
            forward::serve_remote(channel, host, port);
        }
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: Channel<Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Unasked-for agent requests are rejected by dropping the reply.
        if self.forward_agent {
            reply.accept().await;
            agent::serve_forwarded(channel);
        }
        Ok(())
    }
}
