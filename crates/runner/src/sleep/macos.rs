//! macOS sleep notifications through IOKit `IORegisterForSystemPower` (tunnel.md §3.7).
//!
//! A thread owned by this module runs a CoreFoundation run loop with the power port's source.
//! The callback maps each message with [`power_action`]. `kIOMessageSystemWillSleep` is queued
//! for the tokio side: it writes the hint, then calls `IOAllowPowerChange`. The OS waits for that
//! acknowledgement (30 s at most), so the callback never waits on the runner.
//! `kIOMessageCanSystemSleep` is allowed on the spot.
//!
//! The FFI is declared by hand (no extra crate). This file was written on a Linux host, so it is
//! not compiled or run here. The macOS CI job builds it once CI-001 is fixed.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{mpsc, oneshot};

use super::{PowerAction, PowerEvent, PowerMessage, SleepWatcher, power_action};

/// `io_connect_t` (a Mach port name), `io_object_t`, `io_service_t`, `IOReturn`.
type IoConnect = u32;
type IoObject = u32;
type IoService = u32;
type IoReturn = i32;
/// `IONotificationPortRef` and `CFRunLoopSourceRef` / `CFRunLoopRef` are opaque pointers.
type IoNotificationPort = *mut c_void;
type CfRunLoopSource = *mut c_void;
type CfRunLoop = *mut c_void;
type CfStringRef = *const c_void;

/// `IOServiceInterestCallback`. For power messages `argument` is the notification id.
type InterestCallback = unsafe extern "C" fn(
    refcon: *mut c_void,
    service: IoService,
    message: u32,
    argument: *mut c_void,
);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut IoNotificationPort,
        callback: InterestCallback,
        notifier: *mut IoObject,
    ) -> IoConnect;
    fn IOAllowPowerChange(kernel_port: IoConnect, notification_id: isize) -> IoReturn;
    fn IONotificationPortGetRunLoopSource(port: IoNotificationPort) -> CfRunLoopSource;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetCurrent() -> CfRunLoop;
    fn CFRunLoopAddSource(run_loop: CfRunLoop, source: CfRunLoopSource, mode: CfStringRef);
    fn CFRunLoopRun();
    static kCFRunLoopDefaultMode: CfStringRef;
}

/// IOKit's `iokit_common_msg(x)`: `sys_iokit | sub_iokit_common | x`, with `sys_iokit` = `err_system(0x38)`.
const fn iokit_common_msg(message: u32) -> u32 {
    0xE000_0000 | message
}

const K_IO_MESSAGE_CAN_SYSTEM_SLEEP: u32 = iokit_common_msg(0x270);
const K_IO_MESSAGE_SYSTEM_WILL_SLEEP: u32 = iokit_common_msg(0x280);

fn classify(message: u32) -> PowerMessage {
    match message {
        K_IO_MESSAGE_CAN_SYSTEM_SLEEP => PowerMessage::CanSystemSleep,
        K_IO_MESSAGE_SYSTEM_WILL_SLEEP => PowerMessage::SystemWillSleep,
        _ => PowerMessage::Other,
    }
}

/// A `WillSleep` for the tokio side, with the notification id that acknowledges it.
#[derive(Debug, Clone, Copy)]
struct Queued {
    event: PowerEvent,
    notification_id: isize,
}

/// Shared by the run loop thread (callback) and the watcher. Leaked: it lives for the process.
struct Context {
    events: mpsc::UnboundedSender<Queued>,
    // The root port from `IORegisterForSystemPower`, stored before the run loop starts.
    root_port: AtomicU32,
}

impl Context {
    fn allow(&self, notification_id: isize) -> IoReturn {
        let root = self.root_port.load(Ordering::Acquire);
        // SAFETY: `root` is the port IORegisterForSystemPower returned, and the id came from the
        // callback that the OS is waiting on.
        unsafe { IOAllowPowerChange(root, notification_id) }
    }
}

/// The IOKit interest callback. Runs on the run loop thread.
unsafe extern "C" fn on_power_message(
    refcon: *mut c_void,
    _service: IoService,
    message: u32,
    argument: *mut c_void,
) {
    // SAFETY: `refcon` is the leaked `Context` passed to IORegisterForSystemPower.
    let context = unsafe { &*(refcon as *const Context) };
    let notification_id = argument as isize;
    match power_action(classify(message)) {
        PowerAction::AllowNow => {
            context.allow(notification_id);
        }
        PowerAction::HintThenAllow => {
            let queued = Queued {
                event: PowerEvent::WillSleep,
                notification_id,
            };
            // With no watcher left, do not hold the sleep.
            if context.events.send(queued).is_err() {
                context.allow(notification_id);
            }
        }
        PowerAction::Ignore => {}
    }
}

/// Registers for power messages and runs the run loop. Returns only if the run loop stops.
fn run_power_loop(context: &'static Context, ready: oneshot::Sender<Result<()>>) {
    let mut port: IoNotificationPort = std::ptr::null_mut();
    let mut notifier: IoObject = 0;
    let refcon = (context as *const Context).cast_mut().cast::<c_void>();
    // SAFETY: the out-pointers are valid for writes; `refcon` outlives the run loop.
    let root =
        unsafe { IORegisterForSystemPower(refcon, &mut port, on_power_message, &mut notifier) };
    if root == 0 {
        let _ = ready.send(Err(anyhow!("IORegisterForSystemPower failed")));
        return;
    }
    context.root_port.store(root, Ordering::Release);
    // SAFETY: `port` was set by IORegisterForSystemPower; the run loop is this thread's.
    unsafe {
        let source = IONotificationPortGetRunLoopSource(port);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
    }
    let _ = ready.send(Ok(()));
    // SAFETY: runs this thread's run loop with the source above attached.
    unsafe { CFRunLoopRun() };
}

/// Waits for `WillSleep` from IOKit and holds the sleep until the hint is written.
pub struct MacSleepWatcher {
    events: mpsc::UnboundedReceiver<Queued>,
    context: &'static Context,
    // The id of the `WillSleep` that `release_delay` acknowledges.
    pending: Option<isize>,
}

impl MacSleepWatcher {
    /// Starts the run loop thread and registers with IOKit. Fails when IOKit refuses.
    pub async fn connect() -> Result<Self> {
        let (events_tx, events) = mpsc::unbounded_channel();
        let context: &'static Context = Box::leak(Box::new(Context {
            events: events_tx,
            root_port: AtomicU32::new(0),
        }));
        let (ready_tx, ready_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("sm-power".to_owned())
            .spawn(move || run_power_loop(context, ready_tx))
            .context("start the power notification thread")?;
        ready_rx
            .await
            .context("power notification thread ended before it registered")?
            .context("register for power notifications")?;
        Ok(Self {
            events,
            context,
            pending: None,
        })
    }
}

impl SleepWatcher for MacSleepWatcher {
    async fn next_event(&mut self) -> Result<PowerEvent> {
        let queued = self
            .events
            .recv()
            .await
            .ok_or_else(|| anyhow!("power notifications ended"))?;
        self.pending = Some(queued.notification_id);
        Ok(queued.event)
    }

    async fn take_delay(&mut self) -> Result<()> {
        // Nothing to hold: IOKit waits for the acknowledgement of a pending WillSleep.
        Ok(())
    }

    fn release_delay(&mut self) -> Result<()> {
        let Some(notification_id) = self.pending.take() else {
            return Ok(());
        };
        let status = self.context.allow(notification_id);
        if status != 0 {
            bail!("IOAllowPowerChange returned {status:#x}");
        }
        Ok(())
    }
}
