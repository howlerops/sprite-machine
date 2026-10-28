//! The trusted directories of this machine: `<state dir>/trust.json`, a JSON object
//! `{"trusted": ["/abs/dir", ...]}` whose entries are canonical (symlinks resolved). The file is
//! written 0600. The home directory and `/` are never stored: they are asked about every time.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const FILE: &str = "trust.json";

/// The trusted directories, read from and written to [`FILE`].
#[derive(Debug, Clone)]
pub struct TrustStore {
    file: PathBuf,
    /// The home directory, canonical when it exists.
    home: Option<PathBuf>,
}

impl TrustStore {
    pub fn new(state_dir: &Path, home: &Path) -> Self {
        Self {
            file: state_dir.join(FILE),
            home: fs::canonicalize(home).ok(),
        }
    }

    /// `dir` with symlinks resolved: the form every lookup and stored entry uses.
    pub fn canonical(dir: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(dir)
    }

    /// Whether the canonical `dir` may be stored as trusted: `/` and the home directory may not.
    pub fn persistable(&self, dir: &Path) -> bool {
        dir.parent().is_some() && self.home.as_deref() != Some(dir)
    }

    /// Whether `dir` is trusted. A stored entry for `/` or the home directory does not count.
    pub fn is_trusted(&self, dir: &Path) -> io::Result<bool> {
        let dir = Self::canonical(dir)?;
        Ok(self.persistable(&dir) && self.entries()?.contains(&dir))
    }

    /// Stores `dir` as trusted and returns its canonical form.
    pub fn trust(&self, dir: &Path) -> io::Result<PathBuf> {
        let dir = Self::canonical(dir)?;
        if !self.persistable(&dir) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "{} is never stored as trusted: it is asked every time",
                    dir.display()
                ),
            ));
        }
        let mut entries = self.entries()?;
        entries.insert(dir.clone());
        self.write(&entries)?;
        Ok(dir)
    }

    /// Forgets `dir`; `false` when it was not trusted. A deleted directory is matched by its
    /// given path.
    pub fn untrust(&self, dir: &Path) -> io::Result<bool> {
        let dir = Self::canonical(dir).unwrap_or_else(|_| dir.to_path_buf());
        let mut entries = self.entries()?;
        let removed = entries.remove(&dir);
        if removed {
            self.write(&entries)?;
        }
        Ok(removed)
    }

    /// The stored directories, in order.
    pub fn list(&self) -> io::Result<Vec<PathBuf>> {
        Ok(self.entries()?.into_iter().collect())
    }

    fn invalid(&self) -> io::Error {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("{} is not a trust list", self.file.display()),
        )
    }

    /// The stored entries. A missing file is an empty list; a file that does not parse is an
    /// error, so nothing is trusted by accident.
    fn entries(&self) -> io::Result<BTreeSet<PathBuf>> {
        let bytes = match fs::read(&self.file) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(error) => return Err(error),
        };
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| self.invalid())?;
        let list = value
            .get("trusted")
            .and_then(Value::as_array)
            .ok_or_else(|| self.invalid())?;
        list.iter()
            .map(|entry| {
                entry
                    .as_str()
                    .map(PathBuf::from)
                    .ok_or_else(|| self.invalid())
            })
            .collect()
    }

    fn write(&self, entries: &BTreeSet<PathBuf>) -> io::Result<()> {
        if let Some(parent) = self.file.parent() {
            fs::create_dir_all(parent)?;
        }
        let names: Vec<String> = entries
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect();
        let body = json!({ "trusted": names }).to_string();
        // Written beside the file and renamed over it, so a crash keeps the old list.
        let tmp = self.file.with_extension("json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut out = create_private(&tmp)?;
        out.write_all(body.as_bytes())?;
        out.sync_all()?;
        drop(out);
        fs::rename(&tmp, &self.file)
    }
}

/// A new file that only its owner can read or write.
fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
