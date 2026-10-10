//! Shutdown signals become an interrupt that blocking operations subscribe to,
//! so a running Git child is stopped and reaped before ownership is released.

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

type Listener = Arc<dyn Fn() + Send + Sync>;

static TRIGGERED: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static LISTENERS: Mutex<Vec<(u64, Listener)>> = Mutex::new(Vec::new());

fn listeners() -> MutexGuard<'static, Vec<(u64, Listener)>> {
    LISTENERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Routes SIGINT, SIGTERM and SIGHUP to [`trigger`] from a dedicated thread.
pub fn install() -> io::Result<()> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
    thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for _ in signals.forever() {
                trigger();
            }
        })?;
    Ok(())
}

pub fn trigger() {
    TRIGGERED.store(true, Ordering::SeqCst);

    for (_, listener) in listeners().iter() {
        listener();
    }
}

pub fn triggered() -> bool {
    TRIGGERED.load(Ordering::SeqCst)
}

/// Unsubscribes when dropped.
pub struct Subscription(u64);

/// Calls `listener` on interrupt, or immediately if one already happened.
/// Listeners may run more than once and must not subscribe or unsubscribe.
pub fn subscribe(listener: impl Fn() + Send + Sync + 'static) -> Subscription {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let listener: Listener = Arc::new(listener);
    listeners().push((id, Arc::clone(&listener)));

    if triggered() {
        listener();
    }

    Subscription(id)
}

impl Drop for Subscription {
    fn drop(&mut self) {
        listeners().retain(|(id, _)| *id != self.0);
    }
}
