//! The Linux logind watcher against a mock `org.freedesktop.login1.Manager` on a private bus.
//!
//! The test starts its own `dbus-daemon --session` (the same bus `dbus-run-session` would make,
//! but owned by the test so the address is known), serves a mock Manager on it with the same
//! zbus crate the watcher uses, and connects `LogindSleepWatcher::connect_to` to it. The mock's
//! `Inhibit` hands out one end of a pipe and keeps the other, so the test can see whether the
//! inhibitor fd is still open on the client side (the read end reaches EOF once every copy of the
//! write end is closed). `PrepareForSleep` is emitted by the test.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use runner::sleep::{Hint, LogindSleepWatcher, PowerEvent, SleepWatcher, forward};
use zbus::{Connection, connection, interface};

const WAIT: Duration = Duration::from_secs(5);
const PATH: &str = "/org/freedesktop/login1";
const IFACE: &str = "org.freedesktop.login1.Manager";

/// A private `dbus-daemon`, killed on drop.
struct Bus {
    child: Child,
    address: String,
}

impl Bus {
    fn start() -> Result<Self> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .context("start dbus-daemon (install the dbus package)")?;
        let stdout = child.stdout.take().context("dbus-daemon stdout")?;
        let mut address = String::new();
        BufReader::new(stdout)
            .read_line(&mut address)
            .context("read the bus address")?;
        let address = address.trim().to_owned();
        if address.is_empty() {
            let _ = child.kill();
            bail!("dbus-daemon printed no address");
        }
        Ok(Self { child, address })
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What the mock saw: the arguments of each `Inhibit`, and the read end of each pipe it handed out.
#[derive(Default)]
struct Seen {
    calls: Vec<[String; 4]>,
    read_ends: Vec<OwnedFd>,
}

struct MockManager {
    seen: Arc<Mutex<Seen>>,
}

#[interface(name = "org.freedesktop.login1.Manager")]
impl MockManager {
    fn inhibit(
        &self,
        what: String,
        who: String,
        why: String,
        mode: String,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(zbus::fdo::Error::Failed("pipe".into()));
        }
        // SAFETY: both descriptors are fresh and owned here.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // SAFETY: `read` is a valid open descriptor.
        unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let mut seen = self.seen.lock().unwrap();
        seen.calls.push([what, who, why, mode]);
        seen.read_ends.push(read);
        Ok(write.into())
    }
}

/// How the read end of an inhibitor pipe looks.
#[derive(Debug, PartialEq, Eq)]
enum FdState {
    /// Some write end is still open (the client holds the inhibitor).
    Held,
    /// Every write end is closed.
    Released,
}

fn fd_state(read: &OwnedFd) -> FdState {
    let mut byte = 0u8;
    // SAFETY: reads at most one byte into a valid buffer from an open descriptor.
    let n = unsafe { libc::read(read.as_raw_fd(), (&raw mut byte).cast(), 1) };
    if n == 0 {
        FdState::Released
    } else {
        // -1 with EAGAIN: open and empty.
        FdState::Held
    }
}

struct Fixture {
    _bus: Bus,
    mock: Connection,
    seen: Arc<Mutex<Seen>>,
    watcher: LogindSleepWatcher,
}

async fn fixture() -> Result<Fixture> {
    let bus = Bus::start()?;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let mock = connection::Builder::address(bus.address.as_str())?
        .name("org.freedesktop.login1")?
        .serve_at(
            PATH,
            MockManager {
                seen: Arc::clone(&seen),
            },
        )?
        .build()
        .await
        .context("serve the mock Manager")?;
    let watcher = LogindSleepWatcher::connect_to(&bus.address).await?;
    Ok(Fixture {
        _bus: bus,
        mock,
        seen,
        watcher,
    })
}

async fn emit(mock: &Connection, start: bool) -> Result<()> {
    mock.emit_signal(None::<&str>, PATH, IFACE, "PrepareForSleep", &(start,))
        .await
        .context("emit PrepareForSleep")
}

async fn event(watcher: &mut LogindSleepWatcher) -> Result<PowerEvent> {
    tokio::time::timeout(WAIT, watcher.next_event())
        .await
        .context("no power event in time")?
}

#[tokio::test]
async fn prepare_for_sleep_true_and_false_map_to_will_sleep_and_resumed() -> Result<()> {
    let mut f = fixture().await?;
    emit(&f.mock, true).await?;
    assert_eq!(event(&mut f.watcher).await?, PowerEvent::WillSleep);
    emit(&f.mock, false).await?;
    assert_eq!(event(&mut f.watcher).await?, PowerEvent::Resumed);
    Ok(())
}

#[tokio::test]
async fn take_delay_asks_for_a_sleep_delay_inhibitor_and_holds_its_fd() -> Result<()> {
    let mut f = fixture().await?;
    f.watcher.take_delay().await?;
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.calls.len(), 1);
    let [what, who, _why, mode] = &seen.calls[0];
    assert_eq!(
        (what.as_str(), who.as_str(), mode.as_str()),
        ("sleep", "sm", "delay")
    );
    assert_eq!(fd_state(&seen.read_ends[0]), FdState::Held);
    Ok(())
}

#[tokio::test]
async fn release_delay_closes_the_fd_and_is_idempotent() -> Result<()> {
    let mut f = fixture().await?;
    f.watcher.take_delay().await?;
    f.watcher.release_delay()?;
    assert_eq!(
        fd_state(&f.seen.lock().unwrap().read_ends[0]),
        FdState::Released
    );
    // A second release with nothing held is not an error.
    f.watcher.release_delay()?;
    Ok(())
}

#[tokio::test]
async fn taking_the_delay_again_closes_the_previous_fd() -> Result<()> {
    let mut f = fixture().await?;
    f.watcher.take_delay().await?;
    f.watcher.take_delay().await?;
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.read_ends.len(), 2);
    assert_eq!(fd_state(&seen.read_ends[0]), FdState::Released);
    assert_eq!(fd_state(&seen.read_ends[1]), FdState::Held);
    Ok(())
}

#[tokio::test]
async fn dropping_the_watcher_releases_the_inhibitor() -> Result<()> {
    let f = fixture().await?;
    let Fixture {
        mut watcher, seen, ..
    } = f;
    watcher.take_delay().await?;
    drop(watcher);
    assert_eq!(
        fd_state(&seen.lock().unwrap().read_ends[0]),
        FdState::Released
    );
    Ok(())
}

/// Records, when the hint is written, whether the inhibitor fd was still held.
struct CheckingHint {
    seen: Arc<Mutex<Seen>>,
    held_at_hint: Arc<Mutex<Vec<bool>>>,
}

impl Hint for CheckingHint {
    async fn send_asleep(&self) -> bool {
        let seen = self.seen.lock().unwrap();
        let held = seen
            .read_ends
            .last()
            .is_some_and(|read| fd_state(read) == FdState::Held);
        self.held_at_hint.lock().unwrap().push(held);
        true
    }
}

#[tokio::test]
async fn forward_writes_the_hint_while_the_fd_is_held_then_releases_and_retakes_on_resume()
-> Result<()> {
    let f = fixture().await?;
    let held_at_hint = Arc::new(Mutex::new(Vec::new()));
    let hint = CheckingHint {
        seen: Arc::clone(&f.seen),
        held_at_hint: Arc::clone(&held_at_hint),
    };
    let task = tokio::spawn(forward(f.watcher, hint));

    // The delay is taken at startup.
    wait_for(|| f.seen.lock().unwrap().read_ends.len() == 1).await?;

    emit(&f.mock, true).await?;
    wait_for(|| held_at_hint.lock().unwrap().len() == 1).await?;
    assert_eq!(*held_at_hint.lock().unwrap(), vec![true]);
    wait_for(|| fd_state(&f.seen.lock().unwrap().read_ends[0]) == FdState::Released).await?;

    // Resume takes a fresh inhibitor.
    emit(&f.mock, false).await?;
    wait_for(|| f.seen.lock().unwrap().read_ends.len() == 2).await?;
    assert_eq!(
        fd_state(&f.seen.lock().unwrap().read_ends[1]),
        FdState::Held
    );

    task.abort();
    Ok(())
}

async fn wait_for(mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !condition() {
        if tokio::time::Instant::now() > deadline {
            bail!("condition not met in time");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}
