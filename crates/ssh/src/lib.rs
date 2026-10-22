//! SSH client for remote shells. Has no GPUI dependency.
//!
//! russh runs on one tokio thread owned by this crate. [`connect`] returns a [`PtySession`]
//! that the UI renders like a local shell. Questions for the user (trust this host key, the
//! password, a key passphrase, keyboard-interactive prompts) arrive as [`Prompt`]s carrying
//! a reply channel; dropping the prompt receiver cancels the connection.

mod bridge;
mod keys;
mod known_hosts;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use maomi_term::{PtySession, RemoteIo, SessionOptions};
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use zeroize::Zeroizing;

pub use bridge::pty_size;
pub use keys::{KeyInfo, KeyType, generate_key, import_key, key_info};
pub use known_hosts::{KnownHost, KnownHosts, Trust};

/// Terminal type announced to the server.
const TERM: &str = "xterm-256color";
/// Wrong key passphrases allowed before giving up.
const PASSPHRASE_TRIES: usize = 3;

/// Where to connect and how to log in.
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Tried in order until one succeeds.
    pub auth: Vec<AuthMethod>,
    /// Size, scrollback and cursor for the terminal; the shell fields do not apply.
    pub session: SessionOptions,
    /// Shown as the shell name, e.g. "ssh fennec".
    pub label: String,
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

/// A question for the user while connecting.
pub enum Prompt {
    /// Reply true to trust the key and go on. The caller checks its known hosts first and
    /// may answer without asking.
    HostKey { key: HostKey, reply: Reply<bool> },
    /// Reply None to cancel.
    Password {
        user: String,
        host: String,
        reply: Reply<Option<Zeroizing<String>>>,
    },
    /// The private key is encrypted. `retry` is true after a wrong passphrase.
    KeyPassphrase {
        retry: bool,
        reply: Reply<Option<Zeroizing<String>>>,
    },
    /// One answer per prompt, in order; `echo` false means hide the typing.
    KeyboardInteractive {
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
    #[error("the host key was not trusted")]
    HostKeyRejected,
    #[error("authentication failed")]
    AuthFailed,
    #[error("cancelled")]
    Cancelled,
    #[error("private key: {0}")]
    Key(String),
    #[error("the private key is encrypted; enter its passphrase")]
    KeyEncrypted,
    #[error("SSH: {0}")]
    Ssh(#[from] russh::Error),
    #[error("terminal: {0}")]
    Terminal(String),
}

/// Connects, logs in and opens a shell with a PTY. Works from any async executor: the SSH work
/// runs on this crate's tokio thread.
pub async fn connect(
    options: ConnectOptions,
    prompts: async_channel::Sender<Prompt>,
) -> Result<PtySession, SshError> {
    let (done, result) = async_channel::bounded(1);
    runtime().spawn(async move {
        _ = done.send(open_shell(options, prompts).await).await;
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
) -> Result<PtySession, SshError> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..client::Config::default()
    });
    let rejected = Arc::new(AtomicBool::new(false));
    let handler = Client {
        host: options.host.clone(),
        port: options.port,
        prompts: prompts.clone(),
        rejected: rejected.clone(),
    };
    let mut handle =
        match client::connect(config, (options.host.as_str(), options.port), handler).await {
            Ok(handle) => handle,
            Err(_) if rejected.load(Ordering::Acquire) => return Err(SshError::HostKeyRejected),
            Err(russh::Error::IO(source)) => {
                return Err(SshError::Connect {
                    host: options.host,
                    source,
                });
            }
            Err(err) => return Err(err.into()),
        };
    authenticate(&mut handle, &options, &prompts).await?;

    let channel = handle.channel_open_session().await?;
    let size = pty_size(options.session.size);
    channel
        .request_pty(true, TERM, size.0, size.1, size.2, size.3, &[])
        .await?;
    channel.request_shell(true).await?;

    let parts = bridge::start(handle, channel);
    let io = RemoteIo {
        reader: Box::new(parts.reader),
        writer: Box::new(parts.writer),
        control: Box::new(parts.control),
        exit: parts.exit,
        label: options.label,
    };
    PtySession::attach(io, options.session).map_err(|err| SshError::Terminal(err.to_string()))
}

async fn authenticate(
    handle: &mut Handle<Client>,
    options: &ConnectOptions,
    prompts: &async_channel::Sender<Prompt>,
) -> Result<(), SshError> {
    for method in &options.auth {
        let ok = match method {
            AuthMethod::Password(password) => {
                let password = match password {
                    Some(password) => password.clone(),
                    None => ask(prompts, |reply| Prompt::Password {
                        user: options.user.clone(),
                        host: options.host.clone(),
                        reply,
                    })
                    .await?
                    .ok_or(SshError::Cancelled)?,
                };
                handle
                    .authenticate_password(options.user.as_str(), password.as_str())
                    .await?
                    .success()
            }
            AuthMethod::PrivateKey { key, passphrase } => {
                let key = load_key(key, passphrase.as_ref(), prompts).await?;
                let hash = handle.best_supported_rsa_hash().await?.flatten();
                handle
                    .authenticate_publickey(
                        options.user.as_str(),
                        PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                    )
                    .await?
                    .success()
            }
            AuthMethod::KeyboardInteractive => {
                keyboard_interactive(handle, &options.user, prompts).await?
            }
        };
        if ok {
            return Ok(());
        }
    }
    Err(SshError::AuthFailed)
}

/// Decodes a private key, asking for its passphrase when it is encrypted.
async fn load_key(
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
                    ask(prompts, |reply| Prompt::KeyPassphrase { retry, reply })
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
}
