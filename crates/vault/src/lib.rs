//! One age-encrypted file for every secret (SSH keys, host passwords). The OS keystore holds
//! only the key that unlocks it; without a keystore, a passphrase (age scrypt) does. No UI here:
//! the app asks for passphrases and decides when to lock (dropping a [`Vault`] locks it).

mod keystore;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub use age::secrecy::{ExposeSecret, SecretString};
pub use keystore::{Keystore, MemoryKeystore, OsKeystore};

/// Format version inside the encrypted payload.
const PAYLOAD_VERSION: u32 = 1;
/// age's own limit for files it decrypts; stops a crafted file from costing hours of CPU.
const MAX_SCRYPT_WORK_FACTOR: u8 = 22;

#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    #[error("a vault already exists at {0}")]
    Exists(PathBuf),
    #[error("no vault at {0}")]
    Missing(PathBuf),
    #[error("this vault is locked with a passphrase")]
    NeedsPassphrase,
    #[error("the OS keystore has no key for this vault")]
    KeystoreEmpty,
    #[error("wrong passphrase or key")]
    WrongKey,
    #[error("the vault file is damaged: {0}")]
    Damaged(String),
    #[error("OS keystore: {0}")]
    Keystore(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// How the vault file is unlocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// A random age X25519 identity stored in the OS keystore.
    Keystore,
    Passphrase,
}

/// What [`Vault::create`], [`Vault::unlock`] and [`Vault::set_unlock`] use.
pub enum Unlock {
    Keystore,
    Passphrase(SecretString),
}

impl Unlock {
    fn method(&self) -> Method {
        match self {
            Unlock::Keystore => Method::Keystore,
            Unlock::Passphrase(_) => Method::Passphrase,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Password,
    PrivateKey,
    Other,
}

/// One secret. The value is zeroed when the entry is dropped.
#[derive(Debug, Deserialize)]
pub struct Entry {
    pub label: String,
    pub kind: EntryKind,
    pub secret: SecretString,
}

/// Serialising borrows the secrets, so no extra copies sit in memory.
#[derive(Serialize)]
struct PayloadOut<'a> {
    version: u32,
    entries: BTreeMap<&'a str, EntryOut<'a>>,
}

#[derive(Serialize)]
struct EntryOut<'a> {
    label: &'a str,
    kind: EntryKind,
    secret: &'a str,
}

#[derive(Deserialize)]
struct PayloadIn {
    #[allow(dead_code)]
    version: u32,
    entries: BTreeMap<String, Entry>,
}

/// An unlocked vault. Every change is written to disk straight away.
pub struct Vault {
    path: PathBuf,
    keystore: Arc<dyn Keystore>,
    unlock: Unlock,
    entries: BTreeMap<String, Entry>,
    last_used: Instant,
    /// Lower than age's default only in tests, where a real scrypt run takes about a second.
    work_factor: Option<u8>,
}

impl Vault {
    /// How the vault at `path` is unlocked; None when there is no vault yet.
    pub fn method_of(path: &Path) -> Result<Option<Method>, VaultError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let decryptor = age::Decryptor::new_buffered(&bytes[..])
            .map_err(|err| VaultError::Damaged(err.to_string()))?;
        Ok(Some(if decryptor.is_scrypt() {
            Method::Passphrase
        } else {
            Method::Keystore
        }))
    }

    /// Creates an empty vault at `path`. With [`Unlock::Keystore`] a fresh identity replaces
    /// any key the keystore held.
    pub fn create(
        path: &Path,
        unlock: Unlock,
        keystore: Arc<dyn Keystore>,
    ) -> Result<Self, VaultError> {
        Self::create_with(path, unlock, keystore, None)
    }

    fn create_with(
        path: &Path,
        unlock: Unlock,
        keystore: Arc<dyn Keystore>,
        work_factor: Option<u8>,
    ) -> Result<Self, VaultError> {
        if path.exists() {
            return Err(VaultError::Exists(path.to_owned()));
        }
        if matches!(unlock, Unlock::Keystore) {
            let identity = age::x25519::Identity::generate();
            keystore.set(&identity.to_string())?;
        }
        let vault = Self {
            path: path.to_owned(),
            keystore,
            unlock,
            entries: BTreeMap::new(),
            last_used: Instant::now(),
            work_factor,
        };
        vault.save()?;
        Ok(vault)
    }

    /// Opens the vault at `path`. `unlock` must match the file's [`Method`].
    pub fn unlock(
        path: &Path,
        unlock: Unlock,
        keystore: Arc<dyn Keystore>,
    ) -> Result<Self, VaultError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(VaultError::Missing(path.to_owned()));
            }
            Err(err) => return Err(err.into()),
        };
        let plain = match &unlock {
            Unlock::Keystore => {
                if Self::method_of(path)? == Some(Method::Passphrase) {
                    return Err(VaultError::NeedsPassphrase);
                }
                let key = keystore.get()?.ok_or(VaultError::KeystoreEmpty)?;
                let identity: age::x25519::Identity = key
                    .expose_secret()
                    .parse()
                    .map_err(|err: &str| VaultError::Keystore(err.into()))?;
                decrypt(&identity, &bytes)?
            }
            Unlock::Passphrase(passphrase) => {
                let mut identity = age::scrypt::Identity::new(passphrase.clone());
                identity.set_max_work_factor(MAX_SCRYPT_WORK_FACTOR);
                decrypt(&identity, &bytes)?
            }
        };
        let payload: PayloadIn =
            serde_json::from_slice(&plain).map_err(|err| VaultError::Damaged(err.to_string()))?;
        Ok(Self {
            path: path.to_owned(),
            keystore,
            unlock,
            entries: payload.entries,
            last_used: Instant::now(),
            work_factor: None,
        })
    }

    pub fn method(&self) -> Method {
        self.unlock.method()
    }

    /// Entries by id, sorted.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.entries.iter().map(|(id, entry)| (id.as_str(), entry))
    }

    pub fn get(&mut self, id: &str) -> Option<&Entry> {
        self.last_used = Instant::now();
        self.entries.get(id)
    }

    /// Adds or replaces the entry `id` and saves.
    pub fn put(&mut self, id: &str, entry: Entry) -> Result<(), VaultError> {
        self.last_used = Instant::now();
        let old = self.entries.insert(id.to_owned(), entry);
        self.save().inspect_err(|_| {
            // Keep memory and disk in step when the write fails.
            match old {
                Some(old) => self.entries.insert(id.to_owned(), old),
                None => self.entries.remove(id),
            };
        })
    }

    /// Removes the entry `id` and saves; false when there was none.
    pub fn remove(&mut self, id: &str) -> Result<bool, VaultError> {
        self.last_used = Instant::now();
        let Some(old) = self.entries.remove(id) else {
            return Ok(false);
        };
        if let Err(err) = self.save() {
            self.entries.insert(id.to_owned(), old);
            return Err(err);
        }
        Ok(true)
    }

    /// Re-encrypts the file for a new unlock method or passphrase. Moving off the keystore
    /// deletes its key; moving onto it stores a fresh one.
    pub fn set_unlock(&mut self, unlock: Unlock) -> Result<(), VaultError> {
        self.last_used = Instant::now();
        let was_keystore = self.method() == Method::Keystore;
        if matches!(unlock, Unlock::Keystore) {
            let identity = age::x25519::Identity::generate();
            self.keystore.set(&identity.to_string())?;
        }
        self.unlock = unlock;
        self.save()?;
        if was_keystore && self.method() == Method::Passphrase {
            self.keystore.delete()?;
        }
        Ok(())
    }

    /// Time since an entry was last read or changed; the app locks after its idle limit.
    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    fn save(&self) -> Result<(), VaultError> {
        let payload = PayloadOut {
            version: PAYLOAD_VERSION,
            entries: self
                .entries
                .iter()
                .map(|(id, e)| {
                    let out = EntryOut {
                        label: &e.label,
                        kind: e.kind,
                        secret: e.secret.expose_secret(),
                    };
                    (id.as_str(), out)
                })
                .collect(),
        };
        let plain = Zeroizing::new(
            serde_json::to_vec(&payload).map_err(|err| VaultError::Damaged(err.to_string()))?,
        );
        let cipher = match &self.unlock {
            Unlock::Keystore => {
                let key = self.keystore.get()?.ok_or(VaultError::KeystoreEmpty)?;
                let identity: age::x25519::Identity = key
                    .expose_secret()
                    .parse()
                    .map_err(|err: &str| VaultError::Keystore(err.into()))?;
                encrypt(&identity.to_public(), &plain)?
            }
            Unlock::Passphrase(passphrase) => {
                let mut recipient = age::scrypt::Recipient::new(passphrase.clone());
                if let Some(log_n) = self.work_factor {
                    recipient.set_work_factor(log_n);
                }
                encrypt(&recipient, &plain)?
            }
        };
        write_private(&self.path, &cipher)
    }
}

fn encrypt(recipient: &impl age::Recipient, plain: &[u8]) -> Result<Vec<u8>, VaultError> {
    age::encrypt(recipient, plain).map_err(|err| VaultError::Damaged(err.to_string()))
}

fn decrypt(identity: &impl age::Identity, cipher: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
    match age::decrypt(identity, cipher) {
        Ok(plain) => Ok(Zeroizing::new(plain)),
        Err(age::DecryptError::NoMatchingKeys | age::DecryptError::DecryptionFailed) => {
            Err(VaultError::WrongKey)
        }
        Err(err) => Err(VaultError::Damaged(err.to_string())),
    }
}

/// Writes a sibling file readable only by the owner (Unix 0600; on Windows the per-user
/// profile folder's ACL already does this), then renames it over `path`.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), VaultError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("age.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fast scrypt for tests; real vaults use age's calibrated default.
    const TEST_WORK_FACTOR: u8 = 4;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("maomi-vault-{name}-{}", std::process::id()));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("vault.age")
    }

    fn pass(s: &str) -> Unlock {
        Unlock::Passphrase(SecretString::from(s.to_owned()))
    }

    fn entry(secret: &str) -> Entry {
        Entry {
            label: "test".into(),
            kind: EntryKind::Password,
            secret: SecretString::from(secret.to_owned()),
        }
    }

    #[test]
    fn keystore_round_trip() {
        let path = temp_path("keystore");
        let keystore = Arc::new(MemoryKeystore::default());
        let mut vault = Vault::create(&path, Unlock::Keystore, keystore.clone()).unwrap();
        vault.put("host-a", entry("hunter2")).unwrap();
        drop(vault);

        assert_eq!(Vault::method_of(&path).unwrap(), Some(Method::Keystore));
        let mut vault = Vault::unlock(&path, Unlock::Keystore, keystore).unwrap();
        assert_eq!(
            vault.get("host-a").unwrap().secret.expose_secret(),
            "hunter2"
        );
        // The secret never appears in the file.
        assert!(!String::from_utf8_lossy(&fs::read(&path).unwrap()).contains("hunter2"));
    }

    #[test]
    fn passphrase_round_trip_and_wrong_passphrase() {
        let path = temp_path("passphrase");
        let keystore = Arc::new(MemoryKeystore::default());
        let mut vault = Vault::create_with(
            &path,
            pass("correct horse"),
            keystore.clone(),
            Some(TEST_WORK_FACTOR),
        )
        .unwrap();
        vault.put("key", entry("-----BEGIN KEY-----")).unwrap();
        drop(vault);

        assert_eq!(Vault::method_of(&path).unwrap(), Some(Method::Passphrase));
        assert!(matches!(
            Vault::unlock(&path, pass("wrong"), keystore.clone()),
            Err(VaultError::WrongKey)
        ));
        assert!(matches!(
            Vault::unlock(&path, Unlock::Keystore, keystore.clone()),
            Err(VaultError::NeedsPassphrase)
        ));
        let mut vault = Vault::unlock(&path, pass("correct horse"), keystore).unwrap();
        assert_eq!(
            vault.get("key").unwrap().secret.expose_secret(),
            "-----BEGIN KEY-----"
        );
    }

    #[test]
    fn missing_keystore_key_and_missing_file() {
        let path = temp_path("empty-keystore");
        let keystore = Arc::new(MemoryKeystore::default());
        Vault::create(&path, Unlock::Keystore, keystore.clone()).unwrap();
        keystore.delete().unwrap();
        assert!(matches!(
            Vault::unlock(&path, Unlock::Keystore, keystore.clone()),
            Err(VaultError::KeystoreEmpty)
        ));
        let none = path.with_file_name("none.age");
        assert_eq!(Vault::method_of(&none).unwrap(), None);
        assert!(matches!(
            Vault::unlock(&none, Unlock::Keystore, keystore),
            Err(VaultError::Missing(_))
        ));
    }

    #[test]
    fn create_refuses_an_existing_vault() {
        let path = temp_path("exists");
        let keystore = Arc::new(MemoryKeystore::default());
        Vault::create(&path, Unlock::Keystore, keystore.clone()).unwrap();
        assert!(matches!(
            Vault::create(&path, Unlock::Keystore, keystore),
            Err(VaultError::Exists(_))
        ));
    }

    #[test]
    fn switching_unlock_methods_keeps_entries() {
        let path = temp_path("switch");
        let keystore = Arc::new(MemoryKeystore::default());
        let mut vault = Vault::create(&path, Unlock::Keystore, keystore.clone()).unwrap();
        vault.work_factor = Some(TEST_WORK_FACTOR);
        vault.put("a", entry("one")).unwrap();

        vault.set_unlock(pass("pw")).unwrap();
        assert!(keystore.get().unwrap().is_none(), "keystore key is deleted");
        drop(vault);
        let mut vault = Vault::unlock(&path, pass("pw"), keystore.clone()).unwrap();
        assert_eq!(vault.get("a").unwrap().secret.expose_secret(), "one");

        vault.set_unlock(Unlock::Keystore).unwrap();
        drop(vault);
        let mut vault = Vault::unlock(&path, Unlock::Keystore, keystore).unwrap();
        assert_eq!(vault.get("a").unwrap().secret.expose_secret(), "one");
    }

    #[test]
    fn remove_and_list() {
        let path = temp_path("remove");
        let keystore = Arc::new(MemoryKeystore::default());
        let mut vault = Vault::create(&path, Unlock::Keystore, keystore).unwrap();
        vault.put("b", entry("2")).unwrap();
        vault.put("a", entry("1")).unwrap();
        let ids: Vec<&str> = vault.entries().map(|(id, _)| id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(vault.remove("a").unwrap());
        assert!(!vault.remove("a").unwrap());
        assert!(vault.idle_for() < Duration::from_secs(5));
    }
}
