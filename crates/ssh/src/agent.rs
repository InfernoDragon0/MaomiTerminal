//! The user's running ssh-agent: logging in with its keys (security keys such as
//! sk-ssh-ed25519 included, the agent asks for the touch) and agent forwarding.
//!
//! Windows tries the pipe in `SSH_AUTH_SOCK`, then the OpenSSH agent's pipe, then Pageant.
//! Elsewhere the agent is the socket in `SSH_AUTH_SOCK`. [`set_agent_path`] overrides both.

use std::sync::Mutex;

use russh::Channel;
use russh::client::{Handle, Msg};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{Algorithm, HashAlg};

use crate::{Client, SshError, on_runtime};

type Stream = Box<dyn AgentStream + Send + Unpin>;
type Agent = AgentClient<Stream>;

static AGENT_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Uses the agent at `path` (a pipe name on Windows, a socket elsewhere) instead of
/// `SSH_AUTH_SOCK` and the defaults; None goes back to them.
pub fn set_agent_path(path: Option<String>) {
    *AGENT_PATH.lock().unwrap_or_else(|e| e.into_inner()) = path;
}

fn agent_path() -> Option<String> {
    AGENT_PATH.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// A key the agent holds, for the Settings chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentKey {
    /// e.g. "ssh-ed25519", "sk-ssh-ed25519@openssh.com".
    pub algorithm: String,
    pub comment: String,
}

/// The keys in the running agent. Fails when no agent can be reached.
pub async fn agent_keys() -> Result<Vec<AgentKey>, SshError> {
    on_runtime(async {
        let identities = async { connect().await?.request_identities().await }
            .await
            .map_err(|err| SshError::Agent(err.to_string()))?;
        Ok(identities
            .iter()
            .map(|identity| AgentKey {
                algorithm: identity.public_key().algorithm().as_str().to_owned(),
                comment: identity.comment().to_owned(),
            })
            .collect())
    })
    .await
}

/// Tries every key in the agent. No agent, or none of its keys accepted, gives false.
pub(crate) async fn login(handle: &mut Handle<Client>, user: &str) -> Result<bool, SshError> {
    let mut agent = match connect().await {
        Ok(agent) => agent,
        Err(err) => {
            tracing::debug!(%err, "no ssh-agent");
            return Ok(false);
        }
    };
    let identities = match agent.request_identities().await {
        Ok(identities) => identities,
        Err(err) => {
            tracing::debug!(%err, "ssh-agent did not list its keys");
            return Ok(false);
        }
    };
    for identity in identities {
        let hash = match identity.public_key().algorithm() {
            Algorithm::Rsa { .. } => handle.best_supported_rsa_hash().await?.flatten(),
            _ => None::<HashAlg>,
        };
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                handle
                    .authenticate_publickey_with(user, key, hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                handle
                    .authenticate_certificate_with(user, certificate, hash, &mut agent)
                    .await
            }
        };
        match result {
            Ok(result) if result.success() => return Ok(true),
            Ok(_) => {}
            // A security key that was not touched in time, or a key the agent refuses to use.
            Err(err) => tracing::debug!(%err, "ssh-agent could not sign"),
        }
    }
    Ok(false)
}

/// Connects a forwarded-agent channel from the server to the local agent.
pub(crate) fn serve_forwarded(channel: Channel<Msg>) {
    tokio::spawn(async move {
        match open().await {
            Ok(mut agent) => {
                let mut stream = channel.into_stream();
                _ = tokio::io::copy_bidirectional(&mut stream, &mut agent).await;
            }
            Err(err) => {
                tracing::debug!(%err, "no ssh-agent for a forwarded request");
                _ = channel.close().await;
            }
        }
    });
}

async fn connect() -> Result<Agent, russh::keys::Error> {
    Ok(AgentClient::connect(open().await?))
}

/// OpenSSH for Windows' agent pipe.
#[cfg(windows)]
const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

#[cfg(windows)]
async fn open() -> Result<Stream, russh::keys::Error> {
    let pipe = agent_path()
        .or_else(|| std::env::var("SSH_AUTH_SOCK").ok())
        .filter(|p| p.starts_with(r"\\.\pipe\"))
        .unwrap_or_else(|| OPENSSH_PIPE.to_owned());
    match AgentClient::connect_named_pipe(&pipe).await {
        Ok(agent) => Ok(agent.into_inner()),
        Err(err) => match AgentClient::connect_pageant().await {
            Ok(agent) => Ok(agent.into_inner()),
            // The pipe's error says more than "Pageant is not running".
            Err(_) => Err(err),
        },
    }
}

#[cfg(unix)]
async fn open() -> Result<Stream, russh::keys::Error> {
    let agent = match agent_path() {
        Some(path) => AgentClient::connect_uds(path).await?,
        None => AgentClient::connect_env().await?,
    };
    Ok(agent.into_inner())
}
