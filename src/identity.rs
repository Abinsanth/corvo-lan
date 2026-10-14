//! This machine's identity: an X25519 keypair kept in the config directory.
//! A lock file makes a second copy on the same machine use a throwaway key.

use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{ErrorKind, Write};
#[cfg(unix)]
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
pub const NO_LOCK: &str = "couldn't lock the identity; using a temporary identity for this session";

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

/// On Unix: `$XDG_CONFIG_HOME/corvo-lan`, else `$HOME/.config/corvo-lan`. Empty or
/// relative values are ignored, as the XDG spec says.
pub fn config_dir(xdg: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let absolute = |v: Option<&str>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = absolute(xdg).or_else(|| absolute(home).map(|h| h.join(".config")))?;
    Some(base.join("corvo-lan"))
}

/// `%APPDATA%\corvo-lan`. Empty or relative values are ignored.
pub fn windows_config_dir(appdata: Option<&str>) -> Option<PathBuf> {
    appdata.map(PathBuf::from).filter(|p| p.is_absolute()).map(|p| p.join("corvo-lan"))
}

/// Loads this machine's identity from the default config directory.
pub fn load() -> anyhow::Result<Identity> {
    let var = |name: &str| std::env::var(name).ok();
    let dir = if cfg!(windows) {
        windows_config_dir(var("APPDATA").as_deref())
    } else {
        config_dir(var("XDG_CONFIG_HOME").as_deref(), var("HOME").as_deref())
    };
    load_from(dir)
}

/// Errors only for a key file that exists but can't be used: replacing it
/// silently would make everyone we've met see a key-change warning.
pub fn load_from(dir: Option<PathBuf>) -> anyhow::Result<Identity> {
    let Some(dir) = dir else {
        return Ok(Identity::throwaway(NO_DIR));
    };
    if private_dir(&dir).is_err() {
        return Ok(Identity::throwaway(NO_DIR));
    }
    let Ok(lock) = private_file().create(true).truncate(false).open(dir.join(LOCK_FILE)) else {
        return Ok(Identity::throwaway(NO_DIR));
    };
    if let Err(e) = lock.try_lock() {
        return Ok(Identity::throwaway(lock_notice(&e)));
    }
    let keys = read_or_create(&dir.join(KEY_FILE))?;
    Ok(Identity { keys, known_peers: Some(dir.join(KNOWN_FILE)), notice: None, _lock: Some(lock) })
}

fn lock_notice(e: &TryLockError) -> &'static str {
    match e {
        TryLockError::WouldBlock => ALREADY_RUNNING,
        TryLockError::Error(_) => NO_LOCK,
    }
}

/// Write access to a file only we can read: mode 0600 on Unix. On Windows the
/// config folder sits in the user's profile, which is already private.
pub(crate) fn private_file() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
}

/// Creates `dir` readable only by us (0700 on Unix; see `private_file`).
fn private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Writes beside `path` and renames, so a crash never leaves a half-written key.
fn write_key(path: &Path, keys: &Keys) -> std::io::Result<()> {
    let tmp = path.with_extension("key.tmp");
    // A leftover temp file would keep its old mode (mode only applies on creation), so always start fresh.
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = private_file().create_new(true).open(&tmp)?;
    writeln!(file, "{}", to_hex(keys.secret()))?;
    file.sync_all()?;
    fs::rename(&tmp, path)
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
            write_key(path, &keys).with_context(|| format!("cannot save {}", path.display()))?;
            Ok(keys)
        }
        Err(e) => Err(unusable(&e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use crate::testutil::Scratch;

    /// Where the config folder goes. It isn't created, so tests see a first run.
    fn config(scratch: &Scratch) -> PathBuf {
        scratch.path().join("corvo-lan")
    }

    #[cfg(unix)]
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
        let scratch = Scratch::new("corvo-identity");
        let dir = config(&scratch);
        let first = load_from(Some(dir.clone())).unwrap();
        assert_eq!(first.notice, None);
        assert_eq!(first.known_peers, Some(dir.join("known_peers.json")));
        #[cfg(unix)]
        {
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("identity.key")), 0o600);
        }
        let public = first.keys.public;
        drop(first);
        assert_eq!(load_from(Some(dir)).unwrap().keys.public, public);
    }

    #[test]
    fn a_second_copy_gets_a_throwaway_identity() {
        let scratch = Scratch::new("corvo-identity");
        let dir = config(&scratch);
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
        let scratch = Scratch::new("corvo-identity");
        let dir = config(&scratch);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("identity.key"), "nonsense\n").unwrap();
        let err = load_from(Some(dir.clone())).err().unwrap().to_string();
        assert!(err.contains("cannot read") && err.contains("identity.key"), "{err}");
        assert!(err.contains("Delete it to create a new identity"), "{err}");
        assert_eq!(fs::read_to_string(dir.join("identity.key")).unwrap(), "nonsense\n", "never replaced");
    }

    #[test]
    fn whitespace_around_the_key_is_fine() {
        let scratch = Scratch::new("corvo-identity");
        let dir = config(&scratch);
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
    fn a_leftover_temp_key_is_replaced_and_cleaned_up() {
        let scratch = Scratch::new("corvo-identity");
        let dir = config(&scratch);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("identity.key.tmp"), "").unwrap(); // a crash mid-write left this
        let id = load_from(Some(dir.clone())).unwrap();
        let saved = fs::read_to_string(dir.join("identity.key")).unwrap();
        assert_eq!(saved.trim(), to_hex(id.keys.secret()));
        assert!(!dir.join("identity.key.tmp").exists());
        #[cfg(unix)]
        assert_eq!(mode(&dir.join("identity.key")), 0o600);
    }

    #[test]
    fn lock_errors_say_what_happened() {
        assert_eq!(lock_notice(&TryLockError::WouldBlock), ALREADY_RUNNING);
        assert_eq!(lock_notice(&TryLockError::Error(std::io::Error::other("nope"))), NO_LOCK);
    }

    // Unix paths: "/x" isn't absolute on Windows, which uses windows_config_dir instead.
    #[cfg(unix)]
    #[test]
    fn config_dir_follows_xdg_rules() {
        assert_eq!(config_dir(Some("/x"), Some("/home/a")), Some(PathBuf::from("/x/corvo-lan")));
        assert_eq!(config_dir(Some(""), Some("/home/a")), Some(PathBuf::from("/home/a/.config/corvo-lan")));
        assert_eq!(config_dir(Some("rel"), Some("/home/a")), Some(PathBuf::from("/home/a/.config/corvo-lan")));
        assert_eq!(config_dir(None, Some("/home/a")), Some(PathBuf::from("/home/a/.config/corvo-lan")));
        assert_eq!(config_dir(None, Some("")), None);
        assert_eq!(config_dir(None, None), None);
    }

    #[test]
    fn windows_config_dir_needs_an_absolute_appdata() {
        assert_eq!(windows_config_dir(None), None);
        assert_eq!(windows_config_dir(Some("")), None);
        assert_eq!(windows_config_dir(Some("relative\\dir")), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_config_dir_is_under_appdata() {
        assert_eq!(
            windows_config_dir(Some(r"C:\Users\a\AppData\Roaming")),
            Some(PathBuf::from(r"C:\Users\a\AppData\Roaming\corvo-lan"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn windows_config_dir_appends_corvo_lan() {
        assert_eq!(windows_config_dir(Some("/appdata")), Some(PathBuf::from("/appdata/corvo-lan")));
    }
}
