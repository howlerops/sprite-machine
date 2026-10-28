//! Where the machine credential lives: the OS keychain, else a `0600` file.
//!
//! The keychain holds one secret, the JSON of a [`Login`]. When it cannot be used (no keychain
//! on this host, a headless container, a locked store) the same JSON goes to a file created with
//! mode `0600` inside a `0700` directory. `SM_CREDENTIAL_FILE` forces the file and skips the
//! keychain altogether (CI, containers).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

/// The env var that forces the file store at the given path.
pub const CREDENTIAL_FILE_ENV: &str = "SM_CREDENTIAL_FILE";

const KEYCHAIN_SERVICE: &str = "sm";
const KEYCHAIN_USER: &str = "machine-credential";

/// What `sm login` learned: which hub, which machine, and the credential. `Debug` hides the
/// credential.
#[derive(Clone, PartialEq, Eq)]
pub struct Login {
    pub hub: String,
    pub machine_id: String,
    pub credential: String,
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login")
            .field("hub", &self.hub)
            .field("machine_id", &self.machine_id)
            .field("credential", &"..")
            .finish()
    }
}

impl Login {
    fn to_json(&self) -> String {
        json!({
            "hub": self.hub,
            "machine_id": self.machine_id,
            "credential": self.credential,
        })
        .to_string()
    }

    fn from_json(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text).context("stored login is not JSON")?;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("stored login has no {name}"))
        };
        Ok(Self {
            hub: field("hub")?,
            machine_id: field("machine_id")?,
            credential: field("credential")?,
        })
    }
}

/// An OS secret store holding one secret. [`OsKeychain`] is the real one; tests plug in fakes.
pub trait Keychain: Send + Sync {
    fn get(&self) -> Result<Option<String>>;
    fn set(&self, secret: &str) -> Result<()>;
    /// Deleting what is not there is not an error.
    fn delete(&self) -> Result<()>;
}

/// The platform keychain through the `keyring` crate: keyutils on Linux, the Keychain on macOS.
/// Other platforms have none (`CredentialStore::from_env` then uses the file).
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub struct OsKeychain {
    entry: keyring::Entry,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl OsKeychain {
    pub fn open() -> Result<Self> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_USER)
            .map_err(|error| anyhow!("keychain unavailable: {error}"))?;
        Ok(Self { entry })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Keychain for OsKeychain {
    fn get(&self) -> Result<Option<String>> {
        match self.entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(anyhow!("keychain read failed: {error}")),
        }
    }

    fn set(&self, secret: &str) -> Result<()> {
        self.entry
            .set_password(secret)
            .map_err(|error| anyhow!("keychain write failed: {error}"))
    }

    fn delete(&self) -> Result<()> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(anyhow!("keychain delete failed: {error}")),
        }
    }
}

/// Which store a save ended up in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Keychain,
    File(PathBuf),
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keychain => f.write_str("the OS keychain"),
            Self::File(path) => write!(f, "{}", path.display()),
        }
    }
}

/// The credential store: an optional keychain in front of a file.
pub struct CredentialStore {
    keychain: Option<Box<dyn Keychain>>,
    file: PathBuf,
}

impl CredentialStore {
    /// The default store: `SM_CREDENTIAL_FILE` forces the file; otherwise the OS keychain with
    /// the file (`$XDG_CONFIG_HOME/sm/credential.json`, else `~/.config/sm/credential.json`) as
    /// the fallback.
    pub fn from_env() -> Result<Self> {
        if let Some(path) = std::env::var_os(CREDENTIAL_FILE_ENV).filter(|p| !p.is_empty()) {
            return Ok(Self::file_only(PathBuf::from(path)));
        }
        let file = default_file()?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let keychain = OsKeychain::open()
            .ok()
            .map(|keychain| Box::new(keychain) as Box<dyn Keychain>);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let keychain = None;
        Ok(Self { keychain, file })
    }

    /// Only the file, at `path`.
    pub fn file_only(path: PathBuf) -> Self {
        Self {
            keychain: None,
            file: path,
        }
    }

    /// A keychain in front of the file at `path` (tests inject fakes here).
    pub fn with_keychain(keychain: Box<dyn Keychain>, path: PathBuf) -> Self {
        Self {
            keychain: Some(keychain),
            file: path,
        }
    }

    /// The fallback file's path.
    pub fn file_path(&self) -> &Path {
        &self.file
    }

    /// Saves the login: in the keychain when it works, else in the `0600` file. Never both, so
    /// a stale copy cannot outlive a wipe.
    pub fn save(&self, login: &Login) -> Result<Backend> {
        let text = login.to_json();
        if let Some(keychain) = &self.keychain {
            match keychain.set(&text) {
                Ok(()) => {
                    // A file left by an earlier fallback would shadow nothing but still hold a
                    // credential: remove it.
                    let _ = std::fs::remove_file(&self.file);
                    return Ok(Backend::Keychain);
                }
                Err(error) => tracing::warn!("{error}; using {}", self.file.display()),
            }
        }
        write_private_file(&self.file, &text)?;
        Ok(Backend::File(self.file.clone()))
    }

    /// The stored login, if any: the keychain first, then the file.
    pub fn load(&self) -> Result<Option<Login>> {
        if let Some(keychain) = &self.keychain {
            match keychain.get() {
                Ok(Some(text)) => return Login::from_json(&text).map(Some),
                Ok(None) => {}
                Err(error) => tracing::warn!("{error}; trying {}", self.file.display()),
            }
        }
        match std::fs::read_to_string(&self.file) {
            Ok(text) => Login::from_json(&text).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", self.file.display())),
        }
    }

    /// Forgets the credential everywhere (the hub revoked it, or `sm logout`).
    pub fn wipe(&self) -> Result<()> {
        let keychain = match &self.keychain {
            Some(keychain) => keychain.delete(),
            None => Ok(()),
        };
        let file = match std::fs::remove_file(&self.file) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(anyhow!("removing {}: {error}", self.file.display())),
        };
        keychain.and(file)
    }
}

fn default_file() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|p| !p.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or_else(|| anyhow!("no home directory: set {CREDENTIAL_FILE_ENV}"))?;
    Ok(base.join("sm").join("credential.json"))
}

/// Writes `text` to `path` with mode `0600` (directory `0700`): a temp file created with that
/// mode, then renamed over the target, so the secret is never readable by others, not even for a
/// moment.
fn write_private_file(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;

    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    create_private_dir(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("{} is not a file path", path.display()))?;
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}
