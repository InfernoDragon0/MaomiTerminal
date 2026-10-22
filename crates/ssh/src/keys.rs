//! Private keys for the key manager: generate, import (decrypting with a passphrase) and
//! describe. Keys are kept as unencrypted OpenSSH text; the vault encrypts them.

use russh::keys::ssh_key::private::{Ed25519Keypair, RsaKeypair};
use russh::keys::ssh_key::{LineEnding, PrivateKey};
use russh::keys::{Algorithm, HashAlg, decode_secret_key};
use zeroize::Zeroizing;

use crate::SshError;

const RSA_BITS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Ed25519,
    /// 4096-bit RSA, for servers without ed25519.
    Rsa,
}

/// What the key list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// e.g. "ed25519", "RSA-4096", "sk-ed25519".
    pub kind: String,
    /// "SHA256:…".
    pub fingerprint: String,
    /// One line for `authorized_keys`.
    pub public_openssh: String,
}

/// A new key as OpenSSH text. RSA takes a few seconds: call it off the UI thread.
pub fn generate_key(kind: KeyType, comment: &str) -> Result<Zeroizing<String>, SshError> {
    let mut rng = rand::rng();
    let mut key = match kind {
        KeyType::Ed25519 => PrivateKey::from(Ed25519Keypair::random(&mut rng)),
        KeyType::Rsa => PrivateKey::from(
            RsaKeypair::random(&mut rng, RSA_BITS).map_err(|e| SshError::Key(e.to_string()))?,
        ),
    };
    key.set_comment(comment);
    encode(&key)
}

/// Reads a pasted or loaded OpenSSH / PEM key and returns it unencrypted as OpenSSH text.
/// An encrypted key needs `passphrase`; [`SshError::KeyEncrypted`] says one is missing.
pub fn import_key(text: &str, passphrase: Option<&str>) -> Result<Zeroizing<String>, SshError> {
    let passphrase = passphrase.filter(|p| !p.is_empty());
    let key = decode_secret_key(text.trim(), passphrase).map_err(|err| match err {
        russh::keys::Error::KeyIsEncrypted => SshError::KeyEncrypted,
        russh::keys::Error::KeyIsCorrupt | russh::keys::Error::SshKey(_)
            if passphrase.is_some() =>
        {
            SshError::Key("wrong passphrase, or the key is damaged".into())
        }
        err => SshError::Key(err.to_string()),
    })?;
    if matches!(
        key.algorithm(),
        Algorithm::SkEd25519 | Algorithm::SkEcdsaSha2NistP256
    ) {
        // Signing needs the hardware key; ssh-agent talks to it, this app does not.
        return Err(SshError::Key(
            "security keys (sk-*) sign through ssh-agent: add it with ssh-add, then pick Agent for the host".into(),
        ));
    }
    encode(&key)
}

/// Type, fingerprint and public line of a stored key.
pub fn key_info(openssh: &str) -> Result<KeyInfo, SshError> {
    let key = decode_secret_key(openssh, None).map_err(|e| SshError::Key(e.to_string()))?;
    let public = key.public_key();
    let kind = match public.algorithm().as_str() {
        "ssh-ed25519" => "ed25519".to_owned(),
        "ssh-rsa" => match public.key_data().rsa() {
            Some(rsa) => format!("RSA-{}", rsa.key_size()),
            None => "RSA".to_owned(),
        },
        "sk-ssh-ed25519@openssh.com" => "sk-ed25519".to_owned(),
        "sk-ecdsa-sha2-nistp256@openssh.com" => "sk-ecdsa".to_owned(),
        other => other.trim_start_matches("ssh-").to_owned(),
    };
    Ok(KeyInfo {
        kind,
        fingerprint: public.fingerprint(HashAlg::Sha256).to_string(),
        public_openssh: public
            .to_openssh()
            .map_err(|e| SshError::Key(e.to_string()))?,
    })
}

fn encode(key: &PrivateKey) -> Result<Zeroizing<String>, SshError> {
    key.to_openssh(LineEnding::LF)
        .map(|text| Zeroizing::new(text.to_string()))
        .map_err(|e| SshError::Key(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCRYPTED: &str = include_str!("../tests/fixtures/client_ed25519");

    #[test]
    fn generated_ed25519_round_trips() {
        let key = generate_key(KeyType::Ed25519, "kit@maomi").unwrap();
        let info = key_info(&key).unwrap();
        assert_eq!(info.kind, "ed25519");
        assert!(info.fingerprint.starts_with("SHA256:"));
        assert!(info.public_openssh.starts_with("ssh-ed25519 "));
        assert!(info.public_openssh.ends_with("kit@maomi"));
    }

    #[test]
    fn import_needs_the_passphrase() {
        assert!(matches!(
            import_key(ENCRYPTED, None),
            Err(SshError::KeyEncrypted)
        ));
        assert!(matches!(
            import_key(ENCRYPTED, Some("wrong")),
            Err(SshError::Key(_))
        ));
        let key = import_key(ENCRYPTED, Some("pp")).unwrap();
        // Stored without the passphrase: the vault encrypts it.
        assert!(key_info(&key).is_ok());
        assert!(import_key("not a key", None).is_err());
    }
}
