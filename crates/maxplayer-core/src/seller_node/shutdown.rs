//! Graceful-exit signalling for the seller run loop (#747).
//!
//! The seat's kind-30340 announcement is addressable, so whatever it published last is its
//! PERMANENT public answer. A seat that leaves the selling role therefore has to say so on the wire
//! before it goes — see [`crate::heartbeat::retraction_for_state`]. This module is the seam that
//! gives it the chance: a request to leave arrives here, the run loop's `select!` sees it, breaks,
//! and publishes the terminal beat on its way out.
//!
//! ⛔ **This can only ever cover a GRACEFUL exit.** SIGKILL, a panic that skips unwinding, an OOM
//! kill and a power cut do not run code, so nothing here fires and the seat's last `accepting=y`
//! stays standing exactly as before. Consumer-side recency filtering remains the only cover for
//! those, and remains required. Belt AND braces — this is never a replacement for it.
//!
//! WHY A CHANNEL RATHER THAN A SIGNAL HANDLER IN THE LOOP: "leaving the selling role" is not only
//! an OS signal. A supervisor, an embedder, or a future role switch asks for the same thing, and a
//! `select!` arm wired directly to `SIGTERM` could serve none of them — nor could a test drive it
//! without signalling the whole test binary. [`spawn_os_signal_listener`] translates the OS half
//! into a request on this channel; everything downstream sees one shape.

use tokio::sync::mpsc;

/// Requests in flight at once. One is enough: the loop leaves on the FIRST request, so a second is
/// redundant by definition (a repeated `Ctrl-C` must not queue a second departure).
const REQUEST_CAPACITY: usize = 1;

/// Asks a running seller node to leave the selling role and exit.
///
/// Cloneable and cheap. Holding one does NOT keep the node alive, and dropping every clone does not
/// stop it — see [`ShutdownChannel`] on why the node keeps a sender of its own.
#[derive(Clone, Debug)]
pub struct ShutdownHandle {
    tx: mpsc::Sender<String>,
}

impl ShutdownHandle {
    /// Ask the node to stop. `reason` is what the operator sees in the log line and is recorded as
    /// the cause of the terminal beat.
    ///
    /// Never blocks and never fails loudly: a full channel means a departure is ALREADY under way
    /// (capacity 1), and a closed channel means the loop has already finished. Both are the
    /// requested end state, so both return `false` — "nothing more for me to do" — rather than an
    /// error a caller on the exit path would have to invent a policy for.
    pub fn request(&self, reason: impl Into<String>) -> bool {
        self.tx.try_send(reason.into()).is_ok()
    }
}

/// The node's end of the departure channel: a sender it keeps for life, plus the receiver the run
/// loop takes once.
///
/// The node holding its OWN sender is load-bearing. Without it, dropping the last external
/// [`ShutdownHandle`] would close the channel, `recv` would resolve `None`, and a loop reading that
/// as a departure would take a perfectly healthy seat off the market because nobody happened to be
/// holding a handle. [`next_request`] parks forever instead, and this sender means that case cannot
/// arise at all.
#[derive(Debug)]
pub struct ShutdownChannel {
    tx: mpsc::Sender<String>,
    rx: std::sync::Mutex<Option<mpsc::Receiver<String>>>,
}

impl ShutdownChannel {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(REQUEST_CAPACITY);
        Self {
            tx,
            rx: std::sync::Mutex::new(Some(rx)),
        }
    }

    /// A handle callers can use to ask this node to leave.
    pub fn handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            tx: self.tx.clone(),
        }
    }

    /// Take the receiver for the run loop. Returns `None` on any call after the first (one loop per
    /// node), which reads as "nothing will ever arrive here" — [`next_request`] then parks forever
    /// rather than reporting a departure nobody asked for.
    pub fn take_receiver(&self) -> Option<mpsc::Receiver<String>> {
        self.rx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }
}

impl Default for ShutdownChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// The run loop's `select!` arm: resolves with the reason when a departure is requested, and parks
/// forever when one can never arrive.
///
/// Cancel-safe (`mpsc::Receiver::recv` is), which it must be to sit in a `select!` that other arms
/// win constantly — a request dropped on the floor because a heartbeat tick fired first would be a
/// seat that ignored its own shutdown.
pub async fn next_request(rx: &mut Option<mpsc::Receiver<String>>) -> String {
    match rx.as_mut() {
        Some(rx) => match rx.recv().await {
            Some(reason) => reason,
            // Unreachable while the node holds its own sender; parking is the safe reading of it.
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Translate the OS's "please stop" into a departure request, for as long as the returned task
/// lives.
///
/// This is what makes the terminal beat reach the wire in the field: `Ctrl-C`, `systemctl stop`,
/// `docker stop` and a Kubernetes pod delete all arrive as SIGINT or SIGTERM, and with no listener
/// the default disposition kills the process outright — no exit path runs, so no retraction is ever
/// published.
///
/// ⛔ It does NOT — and cannot — cover SIGKILL, a panic that skips unwinding, an OOM kill or a power
/// cut. SIGKILL is not deliverable to a handler at all; the rest never reach the runtime. Those
/// exits still leave the seat's last `accepting=y` standing, and consumer-side recency filtering is
/// still the only thing that covers them.
///
/// Registration failure is logged and otherwise ignored: a seat that cannot install a signal
/// listener must still sell. It then behaves exactly as it did before #747 — killed outright,
/// leaving stale residue — which is a degrade, never a refusal to work.
pub fn spawn_os_signal_listener(handle: ShutdownHandle) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let reason = wait_for_os_signal().await;
        crate::opline!("seller node: {reason} received; leaving the selling role");
        handle.request(reason);
    })
}

/// Resolve with the name of the first termination signal this process receives.
#[cfg(unix)]
async fn wait_for_os_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    // SIGTERM is what a supervisor sends (systemd, docker, kubelet); SIGINT is an operator's
    // Ctrl-C. Both mean "leave the role", and a seat that honoured only one of them would publish
    // its retraction on only half of its real shutdowns.
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => Some(stream),
        Err(error) => {
            crate::opline!(
                "seller node WARN: SIGTERM listener unavailable ({error}); a supervised stop \
                 will kill this seat outright and leave its announcement standing"
            );
            None
        }
    };
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(stream) => Some(stream),
        Err(error) => {
            crate::opline!(
                "seller node WARN: SIGINT listener unavailable ({error}); Ctrl-C will kill this \
                 seat outright and leave its announcement standing"
            );
            None
        }
    };
    match (terminate.as_mut(), interrupt.as_mut()) {
        (Some(terminate), Some(interrupt)) => tokio::select! {
            _ = terminate.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        },
        (Some(terminate), None) => {
            terminate.recv().await;
            "SIGTERM"
        }
        (None, Some(interrupt)) => {
            interrupt.recv().await;
            "SIGINT"
        }
        // Neither installed: park rather than report a stop nobody asked for.
        (None, None) => std::future::pending().await,
    }
}

/// Non-unix fallback: `Ctrl-C` only, which is all the platform offers as a deliverable stop.
#[cfg(not(unix))]
async fn wait_for_os_signal() -> &'static str {
    if let Err(error) = tokio::signal::ctrl_c().await {
        crate::opline!(
            "seller node WARN: Ctrl-C listener unavailable ({error}); this seat will not retract \
             on stop"
        );
        return std::future::pending().await;
    }
    "Ctrl-C"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam the run loop is wired to: a request arrives, with the reason intact.
    #[tokio::test]
    async fn a_request_reaches_the_loop_with_its_reason() {
        let channel = ShutdownChannel::new();
        let mut rx = channel.take_receiver();
        assert!(channel.handle().request("SIGTERM"));
        assert_eq!(next_request(&mut rx).await, "SIGTERM");
    }

    /// Dropping every external handle must NOT read as a shutdown. Pre-`Option` shapes of this
    /// channel would resolve `None` here, and a loop that broke on `None` would retract and exit a
    /// healthy seat the moment its caller stopped holding a handle.
    #[tokio::test]
    async fn dropping_every_handle_is_not_a_shutdown() {
        let channel = ShutdownChannel::new();
        let mut rx = channel.take_receiver();
        drop(channel.handle());
        drop(channel.handle());

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), next_request(&mut rx))
                .await
                .is_err(),
            "a dropped handle must never be read as a request to leave the market"
        );
        // …and a real request still lands afterwards.
        assert!(channel.handle().request("SIGINT"));
        assert_eq!(next_request(&mut rx).await, "SIGINT");
    }

    /// A second request while one is already in flight is redundant, not an error: the loop leaves
    /// on the first. A repeated Ctrl-C must not be reported as a failure by a caller on the exit
    /// path, and must not queue a second departure.
    #[tokio::test]
    async fn a_repeated_request_is_redundant_not_an_error() {
        let channel = ShutdownChannel::new();
        let mut rx = channel.take_receiver();
        assert!(channel.handle().request("SIGINT"));
        assert!(!channel.handle().request("SIGINT again"));
        assert_eq!(next_request(&mut rx).await, "SIGINT");
    }

    /// Only one loop per node takes the receiver; a second take parks forever rather than
    /// inventing a departure.
    #[tokio::test]
    async fn the_receiver_is_taken_once() {
        let channel = ShutdownChannel::new();
        assert!(channel.take_receiver().is_some());
        let mut second = channel.take_receiver();
        assert!(second.is_none());
        channel.handle().request("SIGTERM");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), next_request(&mut second))
                .await
                .is_err(),
            "a loop with no receiver must park, never report a shutdown"
        );
    }
}
