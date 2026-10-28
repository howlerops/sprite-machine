//! The spacebar of `sm rc` (M2B-009): the terminal's keys, read one at a time, and the raw mode
//! they need. The guard restores the terminal when it drops, on an error too.

use std::io::{self, BufRead, Read, Write};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

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

/// The trust question, routed through the key reader. The key reader is the only reader of stdin:
/// while a question is pending it takes the typed bytes as the answer line (echoed, with
/// backspace) instead of as keys for the QR toggle.
#[derive(Clone, Default)]
pub struct Prompt(Arc<Mutex<Option<mpsc::Sender<String>>>>);

impl Prompt {
    /// Starts waiting for one answer line. The returned reader yields it (or end of input when
    /// the key reader has stopped).
    pub fn listen(&self) -> Answer {
        let (tx, rx) = mpsc::channel();
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        Answer {
            rx,
            line: Vec::new(),
            pos: 0,
        }
    }

    fn take(&self) -> Option<mpsc::Sender<String>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// Whether a question is waiting for its answer.
    pub fn pending(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

/// The answer line of a pending question, as a reader for [`crate::trust::ask_on`].
pub struct Answer {
    rx: mpsc::Receiver<String>,
    line: Vec<u8>,
    pos: usize,
}

impl Read for Answer {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Answer {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.pos >= self.line.len() {
            self.pos = 0;
            self.line = match self.rx.recv() {
                Ok(line) => format!("{line}\n").into_bytes(),
                // The key reader stopped: no answer, which is a no.
                Err(_) => Vec::new(),
            };
        }
        Ok(&self.line[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.line.len());
    }
}

/// Reads keys from `input` until it ends. Each space shows or hides the QR of the latest session
/// URL, written to `out`; other keys change nothing. While a question is pending in `prompt`,
/// every byte goes to its answer line (echoed to `out`) and none reaches the QR toggle.
pub fn pump(
    mut input: impl Read,
    latest: &Latest,
    prompt: &Prompt,
    out: &mut impl Write,
) -> io::Result<()> {
    let mut toggle = QrToggle::default();
    let mut answer = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let read = input.read(&mut buf)?;
        if read == 0 {
            // Dropping the pending sender ends the question with no answer.
            prompt.take();
            return Ok(());
        }
        for &key in &buf[..read] {
            if prompt.pending() {
                match key {
                    b'\n' | b'\r' => {
                        out.write_all(b"\r\n")?;
                        out.flush()?;
                        let line =
                            String::from_utf8_lossy(&std::mem::take(&mut answer)).into_owned();
                        if let Some(tx) = prompt.take() {
                            let _ = tx.send(line);
                        }
                    }
                    0x7f | 0x08 => {
                        if answer.pop().is_some() {
                            out.write_all(b"\x08 \x08")?;
                            out.flush()?;
                        }
                    }
                    _ => {
                        answer.push(key);
                        out.write_all(&[key])?;
                        out.flush()?;
                    }
                }
            } else if toggle.on_key(char::from(key))
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
