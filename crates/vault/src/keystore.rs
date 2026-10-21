//! Where the vault's unlock identity lives: the OS keystore (Windows Credential Manager,
//! macOS Keychain, Secret Service on Linux), or memory in tests.

use std::sync::Mutex;

use age::secrecy::{ExposeSecret as _, SecretString};

use crate::VaultError;

/// Holds one secret: the vault's age identity string (about 74 bytes, well inside Windows
/// Credential Manager's 2560-byte limit).
pub trait Keystore: Send + Sync {
    /// None when nothing is stored.
    fn get(&self) -> Result<Option<SecretString>, VaultError>;
    fn set(&self, identity: &SecretString) -> Result<(), VaultError>;
    /// Succeeds when nothing was stored.
    fn delete(&self) -> Result<(), VaultError>;
}

/// The platform keystore, under service `MaomiTerminal`, account `vault-identity`.
pub struct OsKeystore {
    service: String,
    account: String,
}

impl Default for OsKeystore {
    fn default() -> Self {
        Self {
            service: "MaomiTerminal".into(),
            account: "vault-identity".into(),
        }
    }
}

impl OsKeystore {
    fn entry(&self) -> Result<keyring::Entry, VaultError> {
        keyring::Entry::new(&self.service, &self.account).map_err(keystore_error)
    }
}

impl Keystore for OsKeystore {
    fn get(&self) -> Result<Option<SecretString>, VaultError> {
        match self.entry()?.get_password() {
            Ok(identity) => Ok(Some(SecretString::from(identity))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(keystore_error(err)),
        }
    }

    fn set(&self, identity: &SecretString) -> Result<(), VaultError> {
        self.entry()?
            .set_password(identity.expose_secret())
            .map_err(keystore_error)
    }

    fn delete(&self) -> Result<(), VaultError> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(keystore_error(err)),
        }
    }
}

fn keystore_error(err: keyring::Error) -> VaultError {
    VaultError::Keystore(err.to_string())
}

/// Keeps the identity in memory: tests, and machines without a keystore.
#[derive(Default)]
pub struct MemoryKeystore(Mutex<Option<SecretString>>);

impl Keystore for MemoryKeystore {
    fn get(&self) -> Result<Option<SecretString>, VaultError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn set(&self, identity: &SecretString) -> Result<(), VaultError> {
        *self.0.lock().unwrap() = Some(identity.clone());
        Ok(())
    }

    fn delete(&self) -> Result<(), VaultError> {
        *self.0.lock().unwrap() = None;
        Ok(())
    }
}
