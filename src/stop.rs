//! A request to stop, as something a tick can watch without owning signals.
//!
//! Only one thing in a tick is slow enough to be worth interrupting: the
//! gzip of a large generation, which runs on a blocking thread. Everything
//! else is a stat, a rename or a socket request, and a tick that is
//! interrupted between a rename and the reopen that follows it leaves shep
//! writing into a file with the wrong name, which is worse than the wait.
//! So the tick asks this type two things, and only from its tidy loop: is a
//! stop requested before this base, and did one arrive while this base's
//! gzip was running.
//!
//! The poll loop owns the signals. It turns ctrl-c and, on unix, the
//! shepherd's `SIGTERM` into a request here, and watches the same request
//! while it sleeps between ticks. `SIGTERM` has to be among them: it is how
//! every supervised stop begins, and left at its default it would end the
//! process between a rename and its reopen, the one window the tick never
//! lets a stop cut.

use tokio::sync::watch;

/// Where a stop request is read.
#[derive(Debug)]
pub struct Stop(watch::Receiver<bool>);

/// Where a stop request is made. Dropping it without requesting means no
/// request ever comes, which is what a test that never stops wants.
#[derive(Debug)]
pub struct Request(watch::Sender<bool>);

impl Stop {
    /// A stop and the handle that requests it.
    pub fn new() -> (Self, Request) {
        let (sender, receiver) = watch::channel(false);
        (Self(receiver), Request(sender))
    }

    /// A stop nothing will ever request.
    #[cfg(test)]
    pub fn never() -> Self {
        Self::new().0
    }

    /// A stop that ctrl-c requests, and on unix `SIGTERM` as well.
    ///
    /// `SIGTERM` is the signal the shepherd's kill ladder sends first, to
    /// every stop of a live dog, `shep dogs disable` included. With no
    /// handler it takes its default disposition and the process is gone
    /// wherever the tick happens to be, which can be between a rename and
    /// the reopen that follows it. Turning it into the same request ctrl-c
    /// makes lets the tick finish its renames and reopens, and the poll loop
    /// then exits. The shepherd's `SIGKILL` at `kill_timeout` still
    /// guarantees it gets its way, so nothing here argues with the
    /// supervisor about who decides when this process ends.
    ///
    /// The listeners are registered before this returns, so a signal that
    /// arrives after the call is never lost to a task that has not run yet.
    /// The wait for one is a spawned task, so this needs a runtime. Each
    /// listener fails on its own: if one cannot be installed the other still
    /// works, and if neither can the dog runs until the shepherd stops it,
    /// which is how it ran before signals were watched at all. A signal then
    /// ends it the way the OS does with no handler in place: at once,
    /// without the tidy loop's chance to skip or abandon a gzip. Requesting
    /// a stop on that failure instead would exit a dog nobody asked to stop,
    /// and the shepherd would restart it into the same failure.
    #[cfg(unix)]
    pub fn on_stop_signals() -> Self {
        use tokio::signal::unix::{SignalKind, signal};

        let (stop, request) = Self::new();
        let mut interrupt = signal(SignalKind::interrupt()).ok();
        let mut terminate = signal(SignalKind::terminate()).ok();
        tokio::spawn(async move {
            tokio::select! {
                true = arrives(&mut interrupt) => request.request(),
                true = arrives(&mut terminate) => request.request(),
                else => {}
            }
        });
        stop
    }

    /// A stop that ctrl-c requests. Windows has no `SIGTERM` to listen for,
    /// and shep cannot deliver a signal there either, so this is all there
    /// is to watch. See the unix version for the failure behaviour.
    #[cfg(not(unix))]
    pub fn on_stop_signals() -> Self {
        let (stop, request) = Self::new();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                request.request();
            }
        });
        stop
    }

    /// Whether a stop has been requested.
    pub fn requested(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolve once a stop is requested, immediately if it already was, and
    /// never if the [`Request`] was dropped without one.
    pub async fn wait(&mut self) {
        // `wait_for` checks the value before it checks for a dropped
        // sender, so a request made before the wait is still seen. It
        // errors only when the sender is gone and the value is still
        // false, which is the "nobody will ever ask" case.
        if self.0.wait_for(|&requested| requested).await.is_err() {
            core::future::pending::<()>().await;
        }
    }
}

/// Whether `listener` heard its signal. A listener that never installed hears
/// nothing, and one whose driver shut down is not a request either.
#[cfg(unix)]
async fn arrives(listener: &mut Option<tokio::signal::unix::Signal>) -> bool {
    match listener {
        Some(listener) => listener.recv().await.is_some(),
        None => core::future::pending().await,
    }
}

impl Request {
    /// Ask every [`Stop`] made with this to stop.
    pub fn request(self) {
        // Nothing to do if every Stop is already gone.
        let _ = self.0.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn a_request_wakes_a_waiter() {
        let (mut stop, request) = Stop::new();
        assert!(!stop.requested());
        let waiter = tokio::spawn(async move {
            stop.wait().await;
            stop.requested()
        });
        request.request();
        assert!(
            timeout(Duration::from_secs(5), waiter)
                .await
                .expect("woke")
                .expect("joined")
        );
    }

    #[tokio::test]
    async fn a_request_made_before_the_wait_still_wakes_it() {
        let (mut stop, request) = Stop::new();
        request.request();
        assert!(stop.requested());
        timeout(Duration::from_secs(5), stop.wait())
            .await
            .expect("a request already made resolves the wait at once");
    }

    #[tokio::test]
    async fn no_request_never_wakes() {
        // Including once the Request is gone: a dropped handle is "nobody
        // will ever ask", not "asked".
        let mut stop = Stop::never();
        assert!(!stop.requested());
        assert!(
            timeout(Duration::from_millis(50), stop.wait())
                .await
                .is_err(),
            "nothing requested a stop, so the wait must not resolve"
        );
        assert!(!stop.requested());
    }
}
