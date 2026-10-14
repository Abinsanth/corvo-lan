//! This machine's identity: an X25519 keypair kept in the config directory.
//! A lock file makes a second copy on the same machine use a throwaway key.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use curve25519_dalek::MontgomeryPoint;

use crate::protocol::PeerId;
use crate::room::{from_hex, to_hex};

const KEY_FILE: &str = "identity.key";
const LOCK_FILE: &str = "identity.lock";
const KNOWN_FILE: &str = "known_peers.json";
pub const NO_DIR: &str = "no config directory; using a temporary identity for this session";
pub const ALREADY_RUNNING: &str = "another corvo-lan is running here; using a temporary identity for this session";

/// An X25519 keypair. `Debug` never shows the secret.
#[derive(Clone)]
pub struct Keys {
    secret: [u8; 32],
    pub public: PeerId,
}

impl Keys {
    pub fn generate() -> Keys {
        Keys::from_secret(rand::random())
    }

    /// Derives the public key the same way snow does (clamped base-point multiply).
    pub fn from_secret(secret: [u8; 32]) -> Keys {
        Keys { secret, public: PeerId(MontgomeryPoint::mul_base_clamped(secret).to_bytes()) }
    }

    pub fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    /// Carries only a public id, for tests that never handshake.
    #[cfg(test)]
    pub fn fake(public: PeerId) -> Keys {
        Keys { secret: [0; 32], public }
    }
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keys").field("public", &self.public).finish_non_exhaustive()
    }
}

pub struct Identity {
    pub keys: Keys,
    /// Where known peers are saved; `None` for a throwaway identity.
    pub known_peers: Option<PathBuf>,
    /// Shown once at startup.
    pub notice: Option<String>,
    /// Held for the life of the process.
    _lock: Option<File>,
}

impl Identity {
    fn throwaway(notice: &str) -> Identity {
        Identity { keys: Keys::generate(), known_peers: None, notice: Some(notice.into()), _lock: None }
    }
}

/// `$XDG_CONFIG_HOME/corvo-lan`, else `$HOME/.config/corvo-lan`. Empty or
/// relative values are ignored, as the XDG spec says.
pub fn config_dir(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let absolute = |v: Option<&str>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = absolute(xdg).or_else(|| absolute(home).map(|h| h.join(".config")))?;
    Some(base.join("corvo-lan"))
}

/// Loads this machine's identity from the default config directory.
pub fn load() -> anyhow::Result<Identity> {
    let xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    load_from(config_dir(xdg.as_deref(), home.as_deref()))
}

/// Errors only for a key file that exists but can't be used: replacing it
/// silently would make everyone we've met see a key-change warning.
pub fn load_from(dir: Option<PathBuf>) -> anyhow::Result<Identity> {
    let Some(dir) = dir else {
        return Ok(Identity::throwaway(NO_DIR));
    };
    if fs::create_dir_all(&dir).and_then(|()| fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))).is_err() {
        return Ok(Identity::throwaway(NO_DIR));
    }
    let Ok(lock) = OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(dir.join(LOCK_FILE))
    else {
        return Ok(Identity::throwaway(NO_DIR));
    };
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(Identity::throwaway(ALREADY_RUNNING)),
        Err(TryLockError::Error(_)) => return Ok(Identity::throwaway(NO_DIR)),
    }
    let keys = read_or_create(&dir.join(KEY_FILE))?;
    Ok(Identity { keys, known_peers: Some(dir.join(KNOWN_FILE)), notice: None, _lock: Some(lock) })
}

fn read_or_create(path: &Path) -> anyhow::Result<Keys> {
    let unusable =
        |reason: &str| anyhow!("cannot read {}: {reason}. Delete it to create a new identity.", path.display());
    match fs::read_to_string(path) {
        Ok(text) => {
            let secret = from_hex(text.trim())
                .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                .ok_or_else(|| unusable("not a valid key"))?;
            Ok(Keys::from_secret(secret))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let keys = Keys::generate();
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .with_context(|| format!("cannot save {}", path.display()))?;
            writeln!(file, "{}", to_hex(keys.secret())).with_context(|| format!("cannot save {}", path.display()))?;
            Ok(keys)
        }
        Err(e) => Err(unusable(&e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corvo-identity-{:016x}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("corvo-lan")
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn public_key_follows_from_the_secret() {
        let keys = Keys::generate();
        assert_eq!(Keys::from_secret(*keys.secret()).public, keys.public);
        assert_ne!(Keys::generate().public, keys.public);
        let shown = format!("{keys:?}");
        assert!(!shown.contains(&to_hex(keys.secret())), "Debug must not leak the secret");
    }

    #[test]
    fn first_run_creates_a_private_key_and_reuses_it() {
        let dir = temp_dir();
        let first = load_from(Some(dir.clone())).unwrap();
        assert_eq!(first.notice, None);
        assert_eq!(first.known_peers, Some(dir.join("known_peers.json")));
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("identity.key")), 0o600);
        let public = first.keys.public;
        drop(first);
        assert_eq!(load_from(Some(dir)).unwrap().keys.public, public);
    }

    #[test]
    fn a_second_copy_gets_a_throwaway_identity() {
        let dir = temp_dir();
        let first = load_from(Some(dir.clone())).unwrap();
        let saved = fs::read(dir.join("identity.key")).unwrap();
        let second = load_from(Some(dir.clone())).unwrap();
        assert_eq!(second.notice.as_deref(), Some(ALREADY_RUNNING));
        assert_eq!(second.known_peers, None);
        assert_ne!(second.keys.public, first.keys.public);
        assert_eq!(fs::read(dir.join("identity.key")).unwrap(), saved, "the first copy's key is untouched");
        drop(first);
        assert_eq!(load_from(Some(dir)).unwrap().notice, None, "the lock goes with the first copy");
    }

    #[test]
    fn a_malformed_key_file_refuses_to_start() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("identity.key"), "nonsense\n").unwrap();
        let err = load_from(Some(dir.clone())).err().unwrap().to_string();
        assert!(err.contains("cannot read") && err.contains("identity.key"), "{err}");
        assert!(err.contains("Delete it to create a new identity"), "{err}");
        assert_eq!(fs::read_to_string(dir.join("identity.key")).unwrap(), "nonsense\n", "never replaced");
    }

    #[test]
    fn whitespace_around_the_key_is_fine() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let keys = Keys::generate();
        fs::write(dir.join("identity.key"), format!("  {}\r\n", to_hex(keys.secret()))).unwrap();
        assert_eq!(load_from(Some(dir)).unwrap().keys.public, keys.public);
    }

    #[test]
    fn no_directory_means_a_throwaway_identity() {
        let id = load_from(None).unwrap();
        assert_eq!(id.notice.as_deref(), Some(NO_DIR));
        assert_eq!(id.known_peers, None);
    }

    #[test]
    fn config_dir_follows_xdg_rules() {
        assert_eq!(config_dir(Some("/x"), Some("/home/a")), Some(PathBuf::from("/x/corvo-lan")));
        assert_eq!(config_dir(Some(""), Some("/home/a")), Some(PathBuf::from("/home/a/.config/corvo-lan")));
        assert_eq!(config_dir(Some("rel"), Some("/home/a")), Some(PathBuf::from("/home/a/.config/corvo-lan")));
        assert_eq!(config_dir(None, Some("/home/a")), Some(PathBuf::from("/home/a/.config/corvo-lan")));
        assert_eq!(config_dir(None, Some("")), None);
        assert_eq!(config_dir(None, None), None);
    }
}
