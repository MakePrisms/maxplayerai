//! Authenticated transport, durable health and bounded same-event publication.
use super::*;
use nostr_sdk::pool::RelayNotification;
use std::time::Duration;
use tokio::{sync::broadcast::Receiver, time::Instant};
type Notifications = Receiver<RelayNotification>;
const PAGE: usize = 100;
const BOUNDARY_PAGE: usize = 1000;

#[derive(Clone, Debug)]
pub struct WatchOptions {
    pub max_failures: u32,
    pub cycle_timeout: Duration,
    pub liveness_interval: Duration,
    pub liveness_timeout: Duration,
    pub refresh_interval: Duration,
    pub backoff_unit: Duration,
}
impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            max_failures: 10,
            cycle_timeout: Duration::from_secs(60),
            liveness_interval: Duration::from_secs(300),
            liveness_timeout: Duration::from_secs(15),
            refresh_interval: Duration::from_secs(30),
            backoff_unit: Duration::from_secs(2),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchStatus {
    pub state: String,
    pub pid: u32,
    pub relay: String,
    pub last_healthy_at: Option<u64>,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub updated_at: u64,
}
/// Offline, read-only health inspection: never initialize a home or read its key.
pub fn read_status(root: &Path) -> Result<WatchStatus> {
    reject_symlink(root)?;
    reject_symlink(&root.join("chat"))?;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("chat/status.json"))?;
    if !file.metadata()?.is_file() {
        return Err("status must be a regular file".into());
    }
    let mut json = String::new();
    file.read_to_string(&mut json)?;
    let mut status: WatchStatus = serde_json::from_str(&json)?;
    let lock = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("chat/watch.lock"));
    let running = match lock {
        Ok(file) => {
            // SAFETY: valid descriptor. The guard releases any acquired test lock.
            let guard = Lock(file);
            if unsafe { libc::flock(guard.0.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                false
            } else {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                    return Err(error.into());
                }
                true
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    // A new watcher holds the lock briefly before replacing the previous process's file;
    // a status written by a dead pid must never read as this watcher's health.
    // SAFETY: signal 0 only checks that the pid exists.
    let writer_alive = status.pid != 0 && unsafe { libc::kill(status.pid as libc::pid_t, 0) } == 0;
    if !running {
        status.state = "unhealthy".into();
        status.last_error = Some("watch not running (lock not held)".into());
    } else if !writer_alive {
        status.state = "starting".into();
        status.last_error = Some("status written by a previous watch process".into());
    } else if Timestamp::now().as_secs().saturating_sub(status.updated_at) > 90 {
        status.state = "unhealthy".into();
        status.last_error = Some("stale status file (>90s)".into());
    }
    Ok(status)
}
impl Home {
    pub fn status(&self) -> Result<WatchStatus> {
        read_status(&self.root)
    }
    fn save_status(&self, status: &mut WatchStatus) -> Result<()> {
        status.updated_at = Timestamp::now().as_secs();
        self.write("status.json", &serde_json::to_string(status)?)
    }
    fn history_since(&self) -> Result<u64> {
        // v1 advanced per-event, including quota drops and incomplete relay pages.
        // Re-scan retained history once rather than trusting a possibly skipped range.
        if self.read("checkpoint-version")?.trim() != "2" {
            return Ok(0);
        }
        Ok(transport::receive_since(self.cursor()?))
    }
    pub(crate) fn checkpoint(&self, at: u64) -> Result<()> {
        let _lock = self.lock()?;
        self.write("cursor", &at.to_string())?;
        self.write("checkpoint-version", "2")?;
        // Wraps older than the next history window are never requested again.
        self.prune_dropped(transport::receive_since(at))
    }
}
fn log(url: &str, attempt: u64, message: &str, delay: Duration) {
    eprintln!(
        "{} relay={} attempt={} {} next_delay_ms={}",
        Timestamp::now().to_human_datetime(),
        display_text(url),
        attempt,
        display_text(message),
        delay.as_millis()
    );
}
fn retry_delay(failures: u32, unit: Duration) -> Duration {
    let base = unit.mul_f64(2f64.powi(failures.saturating_sub(1).min(5) as i32));
    // Public ephemeral randomness only; +/-10%, with a hard 60s ceiling.
    let random = Keys::generate().public_key().to_bytes()[0] as f64 / 255.0;
    base.mul_f64(0.9 + random * 0.2)
        .min(Duration::from_secs(60))
}
fn client(keys: &Keys) -> Client {
    Client::builder()
        .signer(keys.clone())
        .opts(ClientOptions::new().automatic_authentication(true))
        .build()
}
async fn shutdown(client: Client) {
    let _ = tokio::time::timeout(Duration::from_secs(2), client.shutdown()).await;
}
async fn connect(client: &Client, url: &str) -> Result<(Relay, Notifications)> {
    client
        .pool()
        .add_relay(url, RelayOptions::default().reconnect(false))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let relay = client
        .relay(url)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let mut notifications = relay.notifications();
    relay.connect();
    let mut opened = false;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match notifications.recv().await {
                Ok(RelayNotification::Authenticated) => {
                    return Ok::<(), Box<dyn std::error::Error + Send + Sync>>(());
                }
                Ok(RelayNotification::AuthenticationFailed) => {
                    return Err("auth: NIP-42 rejected".into());
                }
                Ok(RelayNotification::RelayStatus {
                    status: RelayStatus::Connected,
                }) => opened = true,
                Ok(RelayNotification::RelayStatus {
                    status: RelayStatus::Disconnected | RelayStatus::Terminated,
                }) => return Err("connect: disconnected before authentication".into()),
                Ok(RelayNotification::Shutdown) => return Err("connect: shutdown".into()),
                Err(e) => return Err(format!("auth: receive error: {e}").into()),
                _ => {}
            }
        }
    })
    .await;
    match result {
        Ok(result) => result?,
        Err(_) => {
            return Err(if opened {
                "auth: timeout waiting for NIP-42"
            } else {
                "connect: timeout opening socket"
            }
            .into());
        }
    }
    Ok((relay, notifications))
}
fn failure(notification: &RelayNotification, ids: &[&SubscriptionId]) -> Option<String> {
    match notification {
        RelayNotification::RelayStatus { status } if *status != RelayStatus::Connected => {
            Some(format!("disconnected: {status:?}"))
        }
        RelayNotification::AuthenticationFailed => Some("auth: NIP-42 failed".into()),
        RelayNotification::Shutdown => Some("disconnected: shutdown".into()),
        RelayNotification::Message {
            message:
                RelayMessage::Closed {
                    subscription_id,
                    message,
                },
        } if ids.iter().any(|id| *id == subscription_id.as_ref()) => {
            Some(format!("CLOSED: {message}"))
        }
        _ => None,
    }
}
fn filter(home: &Home, since: u64) -> Filter {
    Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(home.keys.public_key())
        .since(Timestamp::from(since))
}
async fn receive(home: &Home, event: &Event, notifier: &mut Notifier) -> Result<usize> {
    match home
        .receive(event)
        .await
        .map_err(|_| "receive error: local message storage failed")?
    {
        Some(peer) => {
            notifier.message(peer);
            Ok(1)
        }
        None => Ok(0),
    }
}
fn tick(notifier: &mut Notifier) {
    if notifier.tick().is_err() {
        eprintln!("notify failed; messages remain in inbox");
    }
}
// Use Message::Event, not the SDK's deduplicated Event notification: pagination must
// count repeated boundary events, including wrappers already seen by this connection.
async fn catch_up(
    home: &Home,
    relay: &Relay,
    notifications: &mut Notifications,
    live: &SubscriptionId,
    notifier: &mut Notifier,
    since: u64,
    mut until: u64,
) -> Result<usize> {
    let mut count = 0;
    let mut limit = PAGE;
    loop {
        let id = relay
            .subscribe(
                filter(home, since)
                    .until(Timestamp::from(until))
                    .limit(limit),
                SubscribeOptions::default(),
            )
            .await
            .map_err(|e| format!("subscribe: {e}"))?;
        let mut rows = 0;
        let mut oldest = until;
        loop {
            let notification = notifications
                .recv()
                .await
                .map_err(|e| format!("receive error: {e}"))?;
            if let Some(error) = failure(&notification, &[live, &id]) {
                return Err(error.into());
            }
            match notification {
                RelayNotification::Message {
                    message:
                        RelayMessage::Event {
                            subscription_id,
                            event,
                        },
                } if subscription_id.as_ref() == &id || subscription_id.as_ref() == live => {
                    if subscription_id.as_ref() == &id {
                        rows += 1;
                        oldest = oldest.min(event.created_at.as_secs());
                    }
                    count += receive(home, &event, notifier).await?;
                }
                RelayNotification::Message {
                    message: RelayMessage::EndOfStoredEvents(eose),
                } if eose.as_ref() == &id => break,
                _ => {}
            }
            tick(notifier);
        }
        relay
            .unsubscribe(&id)
            .await
            .map_err(|e| format!("subscribe: close page: {e}"))?;
        if rows < limit {
            break;
        }
        // Inclusive until preserves all events sharing the boundary second. If that
        // second fills even the larger page, fail closed: NIP-01 has no ID tie cursor.
        if oldest == until {
            if limit == BOUNDARY_PAGE {
                return Err(
                    "subscribe: backlog second saturated; cannot prove complete history".into(),
                );
            }
            limit = BOUNDARY_PAGE;
        } else {
            until = oldest;
            limit = PAGE;
        }
    }
    Ok(count)
}
async fn matching_eose(
    home: &Home,
    notifications: &mut Notifications,
    id: &SubscriptionId,
    live: &SubscriptionId,
    notifier: &mut Notifier,
) -> Result<()> {
    loop {
        let notification = notifications
            .recv()
            .await
            .map_err(|e| format!("receive error: {e}"))?;
        if let Some(error) = failure(&notification, &[live, id]) {
            return Err(error.into());
        }
        match notification {
            RelayNotification::Message {
                message: RelayMessage::EndOfStoredEvents(eose),
            } if eose.as_ref() == id => return Ok(()),
            RelayNotification::Message {
                message:
                    RelayMessage::Event {
                        subscription_id,
                        event,
                    },
            } if subscription_id.as_ref() == live => {
                receive(home, &event, notifier).await?;
            }
            _ => {}
        }
        tick(notifier);
    }
}
pub async fn watch(home: &Home, url: &str, command: Vec<String>) -> Result<()> {
    watch_with_options(home, url, command, WatchOptions::default()).await
}
pub async fn watch_with_options(
    home: &Home,
    url: &str,
    command: Vec<String>,
    options: WatchOptions,
) -> Result<()> {
    let _lock = home.watch_lock()?;
    let mut notifier = Notifier::new(command);
    let mut status = WatchStatus {
        state: "starting".into(),
        pid: std::process::id(),
        relay: url.into(),
        last_healthy_at: None,
        consecutive_failures: 0,
        last_error: None,
        updated_at: 0,
    };
    home.save_status(&mut status)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let client = client(&home.keys);
        let mut phase = "connect/auth";
        let setup = tokio::time::timeout(options.cycle_timeout, async {
            let (relay, mut notifications) = connect(&client, url).await?;
            phase = "subscribe/matching EOSE";
            let since = home.history_since()?;
            let checkpoint = Timestamp::now().as_secs();
            let live = relay
                .subscribe(filter(home, since).limit(0), SubscribeOptions::default())
                .await
                .map_err(|e| format!("subscribe: {e}"))?;
            matching_eose(home, &mut notifications, &live, &live, &mut notifier).await?;
            let count = catch_up(
                home,
                &relay,
                &mut notifications,
                &live,
                &mut notifier,
                since,
                checkpoint,
            )
            .await?;
            home.checkpoint(checkpoint)?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((
                relay,
                notifications,
                live,
                since,
                count,
            ))
        })
        .await;
        let error = match setup {
            Ok(Ok((relay, mut notifications, live, since, count))) => {
                // Only a fully authenticated, EOSE-verified history resets the backoff.
                status.consecutive_failures = 0;
                status.state = "healthy".into();
                status.last_error = None;
                status.last_healthy_at = Some(Timestamp::now().as_secs());
                home.save_status(&mut status)?;
                log(url, attempt, "authenticated", Duration::ZERO);
                log(
                    url,
                    attempt,
                    &format!(
                        "subscribed, caught up {count} messages since {}",
                        Timestamp::from(since).to_human_datetime()
                    ),
                    Duration::ZERO,
                );
                let mut refresh = tokio::time::interval_at(
                    Instant::now() + options.refresh_interval,
                    options.refresh_interval,
                );
                let mut probe = tokio::time::interval_at(
                    Instant::now() + options.liveness_interval,
                    options.liveness_interval,
                );
                loop {
                    let result: Result<()> = tokio::select! {
                        notification = notifications.recv() => match notification {
                            Ok(notification) => {
                                if let Some(error) = failure(&notification, &[&live]) { Err(error.into()) }
                                else if let RelayNotification::Message { message: RelayMessage::Event { subscription_id, event } } = notification {
                                    if subscription_id.as_ref() == &live { receive(home, &event, &mut notifier).await.map(|_| ()) } else { Ok(()) }
                                } else { Ok(()) }
                            }
                            Err(e) => Err(format!("receive error: {e}").into()),
                        },
                        _ = refresh.tick() => home.save_status(&mut status),
                        _ = probe.tick() => {
                            let at = Timestamp::now().as_secs();
                            let check = async {
                                let id = relay.subscribe(filter(home, at).limit(0), SubscribeOptions::default()).await?;
                                matching_eose(home, &mut notifications, &id, &live, &mut notifier).await?;
                                relay.unsubscribe(&id).await?;
                                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                            };
                            let alive = match tokio::time::timeout(options.liveness_timeout, check).await {
                                Ok(result) => result,
                                Err(_) => Err("subscribe: liveness EOSE timeout".into()),
                            };
                            match alive {
                                // The liveness EOSE proves the relay answers, not that the live
                                // subscription delivered everything. Only a paged, EOSE-verified
                                // re-read of history since the last checkpoint may advance it.
                                Ok(()) => {
                                    let verified = async {
                                        let since = home.history_since()?;
                                        catch_up(home, &relay, &mut notifications, &live, &mut notifier, since, at).await?;
                                        home.checkpoint(at)?;
                                        status.last_healthy_at = Some(at);
                                        home.save_status(&mut status)
                                    };
                                    match tokio::time::timeout(options.cycle_timeout, verified).await {
                                        Ok(result) => result,
                                        Err(_) => Err("subscribe: history check deadline exceeded".into()),
                                    }
                                }
                                Err(error) => Err(error),
                            }
                        },
                        _ = tokio::time::sleep(Duration::from_millis(50)) => Ok(()),
                    };
                    tick(&mut notifier);
                    if let Err(error) = result {
                        break error.to_string();
                    }
                }
            }
            Ok(Err(error)) => error.to_string(),
            Err(_) => format!("{phase}: whole-cycle deadline exceeded"),
        };
        status.consecutive_failures += 1;
        status.state = if status.consecutive_failures >= 3 {
            "unhealthy"
        } else {
            "reconnecting"
        }
        .into();
        status.last_error = Some(display_text(&error));
        home.save_status(&mut status)?;
        shutdown(client).await;
        let exiting =
            options.max_failures > 0 && status.consecutive_failures >= options.max_failures;
        let delay = if exiting {
            Duration::ZERO
        } else {
            retry_delay(status.consecutive_failures, options.backoff_unit)
        };
        log(url, attempt, &error, delay);
        if exiting {
            return Err(format!(
                "watch stopped after {} consecutive failures",
                status.consecutive_failures
            )
            .into());
        }
        let until = Instant::now() + delay;
        while Instant::now() < until {
            tick(&mut notifier);
            tokio::time::sleep_until(until.min(Instant::now() + Duration::from_millis(100))).await;
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SendOutcome {
    pub status: String,
    pub attempts: u32,
    pub relay: String,
    pub event_id: String,
    pub rumor_id: String,
    pub error: Option<String>,
}
impl SendOutcome {
    pub fn exit_code(&self) -> i32 {
        match self.status.as_str() {
            "accepted" => 0,
            "unconfirmed" => 2,
            _ => 1,
        }
    }
    pub fn message(&self) -> String {
        if self.status == "accepted" {
            format!(
                "relay accepted {} (attempt {}/3)",
                self.event_id, self.attempts
            )
        } else if self.status == "unconfirmed" {
            format!(
                "unconfirmed {}: may or may not be delivered; do not resend blindly: {}",
                self.event_id,
                display_text(self.error.as_deref().unwrap_or("no OK"))
            )
        } else {
            format!(
                "failed: nothing delivered: {}",
                display_text(self.error.as_deref().unwrap_or("refused"))
            )
        }
    }
}
impl Home {
    pub async fn send_outcome(&self, url: &str, id: &str, text: &str) -> Result<SendOutcome> {
        let body = envelope(text)?;
        let peer = self.peer(id)?;
        let (event, rumor_id) =
            wrap_for_send(&self.keys, PublicKey::parse(&peer.pubkey)?, body).await?;
        let reserved_at = Timestamp::now().as_secs();
        self.reserve_send(&peer.pubkey, reserved_at)?;
        let mut outcome = SendOutcome {
            status: "failed".into(),
            attempts: 0,
            relay: url.into(),
            event_id: event.id.to_hex(),
            rumor_id,
            error: None,
        };
        let mut ambiguous = false;
        for attempt in 1..=3 {
            outcome.attempts = attempt;
            let client = client(&self.keys);
            let mut retry = true;
            match tokio::time::timeout(Duration::from_secs(12), connect(&client, url)).await {
                Ok(Ok((relay, mut notifications))) => {
                    if Timestamp::now()
                        .as_secs()
                        .saturating_sub(event.created_at.as_secs())
                        >= 165
                    {
                        outcome.error =
                            Some("event admission window exhausted before publication".into());
                        shutdown(client).await;
                        break;
                    }
                    client.automatic_authentication(false);
                    // One signed EVENT across all connections; no hidden SDK retry.
                    match relay.send_msg(ClientMessage::event(event.clone())) {
                        Ok(()) => {
                            let ack = tokio::time::timeout(Duration::from_secs(15), async {
                                loop {
                                    match notifications.recv().await {
                                        Ok(RelayNotification::Message {
                                            message:
                                                RelayMessage::Ok {
                                                    event_id,
                                                    status,
                                                    message,
                                                },
                                        }) if event_id == event.id => {
                                            return Some((status, message.to_string()));
                                        }
                                        Ok(RelayNotification::Shutdown) | Err(_) => return None,
                                        Ok(RelayNotification::RelayStatus { status })
                                            if status != RelayStatus::Connected =>
                                        {
                                            return None;
                                        }
                                        _ => {}
                                    }
                                }
                            })
                            .await;
                            match ack {
                                Ok(Some((true, _))) => {
                                    outcome.status = "accepted".into();
                                    outcome.error = None;
                                }
                                Ok(Some((false, reason))) => {
                                    retry = reason.starts_with("auth-required:")
                                        || reason.starts_with("rate-limited:");
                                    outcome.error = Some(reason);
                                }
                                _ => {
                                    ambiguous = true;
                                    outcome.error =
                                        Some("timeout/connection lost waiting for OK".into());
                                }
                            }
                        }
                        Err(error) => {
                            outcome.error = Some(format!("connect: {error}"));
                        }
                    }
                }
                Ok(Err(error)) => outcome.error = Some(error.to_string()),
                Err(_) => outcome.error = Some("connect/auth: deadline exceeded".into()),
            }
            shutdown(client).await;
            if outcome.status == "accepted" || !retry {
                break;
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_millis(250 * u64::from(attempt))).await;
            }
        }
        // From here on the relay outcome is final: local bookkeeping failures only warn,
        // so a delivered message never reads as "failed" and invites a duplicate resend.
        if outcome.status == "accepted" {
            let recorded = self.lock().and_then(|_lock| {
                self.append(&Entry {
                    rumor_id: outcome.rumor_id.clone(),
                    peer: peer.pubkey.clone(),
                    dir: "out".into(),
                    received_at: None,
                    at: Timestamp::now().as_secs(),
                    text: text.into(),
                })
            });
            if let Err(error) = recorded {
                eprintln!(
                    "maxplayer-chat: warning: delivered, but not recorded in the local transcript: {}",
                    display_text(&error.to_string())
                );
            }
        } else if ambiguous {
            // A later refusal cannot disprove delivery on an earlier ambiguous attempt.
            outcome.status = "unconfirmed".into();
        } else if let Err(error) = self.refund_send(&peer.pubkey, reserved_at) {
            eprintln!(
                "maxplayer-chat: warning: the daily send slot was not refunded: {}",
                display_text(&error.to_string())
            );
        }
        Ok(outcome)
    }
}
