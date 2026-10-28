//! The spacebar of `sm rc` (M2B-009): the terminal's keys, read one at a time, and the raw mode
//! they need. The guard restores the terminal when it drops, on an error too.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, PoisonError};

use super::QrToggle;

/// The URL of the latest session started on this machine. The server's session sink sets it and
/// the key reader shows its QR.
#[derive(Clone, Default)]
pub struct Latest(Arc<Mutex<Option<String>>>);

impl Latest {
    pub fn set(&self, url: &str) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(url.to_owned());
    }

    pub fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Reads keys from `input` until it ends. Each space shows or hides the QR of the latest session
/// URL, written to `out`; other keys change nothing.
pub fn pump(mut input: impl Read, latest: &Latest, out: &mut impl Write) -> io::Result<()> {
    let mut toggle = QrToggle::default();
    let mut buf = [0u8; 64];
    loop {
        let read = input.read(&mut buf)?;
        if read == 0 {
            return Ok(());
        }
        for &key in &buf[..read] {
            if toggle.on_key(char::from(key))
                && let Some(url) = latest.get()
            {
                out.write_all(toggle.view(&url).as_bytes())?;
                out.flush()?;
            }
        }
    }
}

/// Raw mode on stdin: keys are read as they are typed and not echoed. Ctrl-C still sends SIGINT,
/// so the server's Ctrl-C stop is unchanged, and output keeps its newlines.
#[cfg(unix)]
pub struct RawMode {
    saved: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    /// Turns raw mode on when `terminal` (stdin is a TTY). Without one nothing is touched and
    /// `None` comes back.
    pub fn enable(terminal: bool) -> io::Result<Option<Self>> {
        if !terminal {
            return Ok(None);
        }
        let fd = libc::STDIN_FILENO;
        let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `tcgetattr` writes the whole struct when it returns 0, which is checked.
        if unsafe { libc::tcgetattr(fd, term.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: initialised by the successful `tcgetattr` above.
        let saved = unsafe { term.assume_init() };
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a complete termios read from this descriptor, changed in place.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(Self { saved }))
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: puts back the settings `enable` read from the same descriptor.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}

/// Without unix there is no raw mode: the keys are not read.
#[cfg(not(unix))]
pub struct RawMode;

#[cfg(not(unix))]
impl RawMode {
    pub fn enable(_terminal: bool) -> io::Result<Option<Self>> {
        Ok(None)
    }
}
