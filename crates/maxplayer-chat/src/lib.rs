//! Opt-in local mailbox. No marketplace initialization or mutation.
use maxplayer_core::private_content::transport;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const DEFAULT_RELAY: &str = "wss://relay.maxplayer.ai";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema: String,
    text: String,
}
pub fn validate_text(text: &str) -> Result<()> {
    if text.is_empty() || text.chars().count() > 8000 {
        return Err("message must contain 1–8000 characters".into());
    }
    Ok(())
}
pub fn envelope(text: &str) -> Result<String> {
    validate_text(text)?;
    Ok(serde_json::to_string(&Envelope {
        schema: "maxplayer-chat/1".into(),
        text: text.into(),
    })?)
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Peer {
    pub pubkey: String,
    pub name: String,
    pub added_at: u64,
}
#[derive(Default, Serialize, Deserialize)]
struct Peers {
    #[serde(default)]
    peer: Vec<Peer>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub rumor_id: String,
    pub peer: String,
    pub dir: String,
    pub at: u64,
    pub text: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Sent {
    day: u64,
    per_peer_counts: BTreeMap<String, u32>,
}
pub struct Home {
    pub root: PathBuf,
    pub keys: Keys,
}
fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}
pub struct Lock(File);
impl Lock {
    fn acquire(path: &Path, nonblock: bool) -> Result<Self> {
        let file = private_file(path)?;
        // SAFETY: valid owned file descriptor; flock changes only its advisory lock.
        if unsafe {
            libc::flock(
                file.as_raw_fd(),
                libc::LOCK_EX | if nonblock { libc::LOCK_NB } else { 0 },
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl Home {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        private_dir(&root)?;
        private_dir(&root.join("chat"))?;
        // Serialize first-use key creation without a marketplace lock or partially-written key reads.
        let guard = Lock::acquire(&root.join("chat/state.lock"), false)?;
        let key_path = root.join("key");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_path)
        {
            Ok(mut file) => {
                file.write_all(Keys::generate().secret_key().to_secret_hex().as_bytes())?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let mut secret = String::new();
        let mut key_file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&key_path)?;
        if !key_file.metadata()?.is_file() {
            return Err("home key must be a regular file".into());
        }
        key_file.read_to_string(&mut secret)?;
        let secret = secret.trim();
        // Same validation as core, deliberately local: no core change at all.
        if secret.len() != 64
            || !secret.bytes().all(|b| b.is_ascii_hexdigit())
            || secret.bytes().all(|b| b == b'0')
        {
            return Err("home key must be a nonzero 64-hex secret".into());
        }
        let keys = Keys::parse(secret).map_err(|_| "invalid home key")?;
        drop(guard);
        Ok(Self { root, keys })
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join("chat").join(name)
    }
    fn lock(&self) -> Result<Lock> {
        Lock::acquire(&self.path("state.lock"), false)
    }
    pub fn watch_lock(&self) -> Result<Lock> {
        Lock::acquire(&self.path("watch.lock"), true)
            .map_err(|_| "watch is already running for this home".into())
    }
    pub fn watch_running(&self) -> bool {
        self.watch_lock().is_err()
    }
    fn read(&self, name: &str) -> Result<String> {
        match fs::read_to_string(self.path(name)) {
            Ok(s) => Ok(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e.into()),
        }
    }
    fn write(&self, name: &str, contents: &str) -> Result<()> {
        let tmp = self.path(&format!("{name}.tmp"));
        let mut f = private_file(&tmp)?;
        f.set_len(0)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        fs::rename(tmp, self.path(name))?;
        File::open(self.path(""))?.sync_all()?;
        Ok(())
    }
    pub fn peers(&self) -> Result<Vec<Peer>> {
        let s = self.read("peers.toml")?;
        if s.is_empty() {
            Ok(vec![])
        } else {
            Ok(toml::from_str::<Peers>(&s)?.peer)
        }
    }
    pub fn peer(&self, id: &str) -> Result<Peer> {
        let hex = PublicKey::parse(id).ok().map(|p| p.to_hex());
        self.peers()?
            .into_iter()
            .find(|p| match &hex {
                Some(hex) => &p.pubkey == hex,
                None => p.name == id,
            })
            .ok_or_else(|| "peer is not approved".into())
    }
    pub fn add_peer(&self, id: &str, name: &str) -> Result<()> {
        if name.is_empty() || name.chars().any(char::is_control) || name.contains(['[', ']']) {
            return Err("invalid peer label".into());
        }
        let pubkey = PublicKey::parse(id)
            .map_err(|_| "invalid peer public key")?
            .to_hex();
        let _lock = self.lock()?;
        let mut peers = self.peers()?;
        if peers.iter().any(|p| p.name == name && p.pubkey != pubkey) {
            return Err("peer label already used".into());
        }
        peers.retain(|p| p.pubkey != pubkey);
        peers.push(Peer {
            pubkey,
            name: name.into(),
            added_at: Timestamp::now().as_secs(),
        });
        self.write("peers.toml", &toml::to_string(&Peers { peer: peers })?)
    }
    pub fn remove_peer(&self, id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let peer = self.peer(id)?;
        let mut peers = self.peers()?;
        peers.retain(|p| p.pubkey != peer.pubkey);
        self.write("peers.toml", &toml::to_string(&Peers { peer: peers })?)
    }
    pub fn entries(&self) -> Result<Vec<Entry>> {
        self.read("log.jsonl")?
            .lines()
            .map(|s| Ok(serde_json::from_str(s)?))
            .collect()
    }
    fn append(&self, entry: &Entry) -> Result<()> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path("log.jsonl"))?;
        writeln!(file, "{}", serde_json::to_string(entry)?)?;
        file.sync_all()?;
        Ok(())
    }
    pub fn cursor(&self) -> Result<u64> {
        Ok(self.read("cursor")?.trim().parse().unwrap_or(0))
    }
    pub async fn receive(&self, event: &Event) -> Result<Option<String>> {
        let Ok((sender, body)) = transport::unwrap_message(&self.keys, event).await else {
            return Ok(None);
        };
        let Ok(env) = serde_json::from_str::<Envelope>(&body) else {
            return Ok(None);
        };
        if env.schema != "maxplayer-chat/1" || validate_text(&env.text).is_err() {
            return Ok(None);
        }
        // Core authenticated all layers. SDK exposes the verified rumor's logical ID, not wrap ID.
        let gift = UnwrappedGift::from_gift_wrap(&self.keys, event)
            .await
            .map_err(|_| "invalid rumor")?;
        let rumor_id = gift.rumor.id.ok_or("missing rumor id")?.to_hex();
        let _lock = self.lock()?;
        let Ok(peer) = self.peer(&sender.to_hex()) else {
            return Ok(None);
        };
        if self.entries()?.iter().any(|e| e.rumor_id == rumor_id) {
            return Ok(None);
        }
        self.append(&Entry {
            rumor_id,
            peer: peer.pubkey,
            dir: "in".into(),
            at: gift.rumor.created_at.as_secs(),
            text: env.text,
        })?;
        // Outer time, never a peer-controlled future rumor timestamp. Only advance after durable append.
        let cursor = self
            .cursor()?
            .max(event.created_at.as_secs().min(Timestamp::now().as_secs()));
        self.write("cursor", &cursor.to_string())?;
        Ok(Some(peer.name))
    }
    pub fn print_inbox(&self, json: bool, mut out: impl Write) -> Result<()> {
        let _lock = self.lock()?;
        let read = self.read("read")?;
        let entries = self.entries()?;
        let start = entries
            .iter()
            .position(|e| e.rumor_id == read)
            .map_or(0, |i| i + 1);
        let mut last = None;
        for e in &entries[start..] {
            if e.dir != "in" {
                continue;
            }
            let Ok(peer) = self.peer(&e.peer) else {
                continue;
            };
            let npub = PublicKey::parse(&e.peer)?.to_bech32()?;
            if json {
                writeln!(
                    out,
                    "{}",
                    serde_json::json!({"from":npub,"name":peer.name,"at":e.at,"untrusted":true,"text":e.text})
                )?;
            } else {
                writeln!(out, "[untrusted message from {}] {}", peer.name, e.text)?;
            }
            last = Some(&e.rumor_id);
        }
        out.flush()?;
        if let Some(last) = last {
            self.write("read", last)?;
        }
        Ok(())
    }
    pub fn print_log(&self, id: &str, json: bool, mut out: impl Write) -> Result<()> {
        let _lock = self.lock()?;
        let peer = self.peer(id)?;
        for e in self.entries()?.iter().filter(|e| e.peer == peer.pubkey) {
            if json {
                writeln!(
                    out,
                    "{}",
                    serde_json::json!({"rumor_id":e.rumor_id,"peer":e.peer,"dir":e.dir,"at":e.at,"untrusted":e.dir=="in","text":e.text})
                )?;
            } else if e.dir == "in" {
                writeln!(out, "in [untrusted message from {}] {}", peer.name, e.text)?;
            } else {
                writeln!(out, "out {}", e.text)?;
            }
        }
        Ok(())
    }
    // Reserve before publishing: an ambiguous failed ACK must not allow unlimited sends.
    pub fn reserve_send(&self, peer: &str, now: u64) -> Result<()> {
        let _lock = self.lock()?;
        self.peer(peer)?;
        let s = self.read("sent-today")?;
        let mut sent: Sent = if s.is_empty() {
            Sent::default()
        } else {
            serde_json::from_str(&s)?
        };
        let day = now / 86400;
        if sent.day != day {
            sent = Sent {
                day,
                ..Default::default()
            };
        }
        let count = sent.per_peer_counts.entry(peer.into()).or_default();
        if *count >= 50 {
            return Err("daily send cap reached (50 per peer per UTC day)".into());
        }
        *count += 1;
        self.write("sent-today", &serde_json::to_string(&sent)?)
    }
    pub async fn send(&self, relay_url: &str, id: &str, text: &str) -> Result<String> {
        let body = envelope(text)?;
        let peer = self.peer(id)?;
        let (event, rumor_id) =
            wrap_for_send(&self.keys, PublicKey::parse(&peer.pubkey)?, body).await?;
        let (client, relay, _notifications) = connect(&self.keys, relay_url).await?;
        if let Err(error) = self.reserve_send(&peer.pubkey, Timestamp::now().as_secs()) {
            client.shutdown().await;
            return Err(error);
        }
        let result = relay.send_event(&event).await;
        client.shutdown().await;
        let id = result
            .map_err(|error| format!("relay did not accept message: {error} (not retried)"))?;
        let _lock = self.lock()?;
        self.append(&Entry {
            rumor_id,
            peer: peer.pubkey,
            dir: "out".into(),
            at: Timestamp::now().as_secs(),
            text: text.into(),
        })?;
        Ok(format!("relay accepted {}", id.to_hex()))
    }
}

/// Same transport as core::wrap, retaining the rumor ID for the outbound transcript.
/// SDK's gift_wrap randomizes by two days; the relay requires core's 180-second window.
pub async fn wrap_for_send(
    keys: &Keys,
    recipient: PublicKey,
    body: String,
) -> Result<(Event, String)> {
    let mut rumor = EventBuilder::private_msg_rumor(recipient, body)
        .allow_self_tagging()
        .build(keys.public_key());
    rumor.ensure_id();
    let id = rumor.id.ok_or("missing rumor id")?.to_hex();
    let seal = EventBuilder::seal(keys, &recipient, rumor)
        .await?
        .sign(keys)
        .await?;
    let ephemeral = Keys::generate();
    let ciphertext = nostr_sdk::nostr::nips::nip44::encrypt(
        ephemeral.secret_key(),
        &recipient,
        seal.as_json(),
        nostr_sdk::nostr::nips::nip44::Version::default(),
    )?;
    if ciphertext.len() > transport::MAX_WRAPPER_CONTENT {
        return Err("encrypted wrapper too large".into());
    }
    let event = EventBuilder::new(Kind::GiftWrap, ciphertext)
        .tags([Tag::public_key(recipient)])
        .custom_created_at(transport::fresh_created_at())
        .sign_with_keys(&ephemeral)?;
    Ok((event, id))
}

async fn connect(
    keys: &Keys,
    url: &str,
) -> Result<(
    Client,
    Relay,
    tokio::sync::broadcast::Receiver<nostr_sdk::pool::RelayNotification>,
)> {
    let client = Client::builder()
        .signer(keys.clone())
        .opts(ClientOptions::new().automatic_authentication(true))
        .build();
    client
        .pool()
        .add_relay(url, RelayOptions::default().reconnect(false))
        .await?;
    let relay = client.relay(url).await?;
    let mut notifications = relay.notifications();
    relay.connect();
    match maxplayer_core::relay_auth::wait_for_nip42_auth(
        &mut notifications,
        Duration::from_secs(10),
    )
    .await
    {
        Ok(maxplayer_core::relay_auth::AuthWait::Authenticated) => {
            Ok((client, relay, notifications))
        }
        _ => {
            client.shutdown().await;
            Err("relay did not authenticate; no chat subscription sent".into())
        }
    }
}

/// One direct child at a time; a burst during a run becomes one pending wake-up.
#[derive(Default)]
pub struct Notifier {
    command: Vec<String>,
    child: Option<std::process::Child>,
    pending: Option<String>,
}
impl Notifier {
    pub fn new(command: Vec<String>) -> Self {
        Self {
            command,
            child: None,
            pending: None,
        }
    }
    pub fn message(&mut self, peer: String) {
        if !self.command.is_empty() {
            self.pending = Some(peer);
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                return Ok(());
            }
            self.child = None;
        }
        if let Some(peer) = self.pending.take() {
            self.child = Some(
                std::process::Command::new(&self.command[0])
                    .args(&self.command[1..])
                    .env_clear()
                    .env("MAXPLAYER_CHAT_PEER", peer)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|_| "could not run notify command")?,
            );
        }
        Ok(())
    }
}
impl Drop for Notifier {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
pub async fn watch(home: &Home, url: &str, command: Vec<String>) -> Result<()> {
    use nostr_sdk::pool::RelayNotification;
    let _lock = home.watch_lock()?;
    let mut notifier = Notifier::new(command);
    loop {
        let connected = connect(&home.keys, url).await;
        if let Ok((client, relay, mut notifications)) = connected {
            let filter = Filter::new()
                .kind(Kind::GiftWrap)
                .pubkey(home.keys.public_key())
                .since(Timestamp::from(transport::receive_since(home.cursor()?)));
            if relay
                .subscribe(filter, SubscribeOptions::default())
                .await
                .is_ok()
            {
                loop {
                    tokio::select! {
                        notification=notifications.recv()=>match notification {
                            Ok(RelayNotification::Event{event,..})=>{if let Some(peer)=home.receive(&event).await?{notifier.message(peer);}},
                            Ok(RelayNotification::RelayStatus{status}) if status!=RelayStatus::Connected=>break,
                            Ok(RelayNotification::AuthenticationFailed | RelayNotification::Shutdown)=>break,
                            Ok(RelayNotification::Message{message:RelayMessage::Closed{..}})=>break,
                            Err(_)=>break,
                            _=>{},
                        },
                        _=tokio::time::sleep(Duration::from_millis(50))=>{},
                    }
                    if notifier.tick().is_err() {
                        eprintln!("notify failed; messages remain in inbox");
                    }
                }
            }
            // Discard the entire client's subscriptions before a fresh authenticated connection.
            client.shutdown().await;
        }
        eprintln!("relay disconnected; retrying");
        for _ in 0..20 {
            if notifier.tick().is_err() {
                eprintln!("notify failed; messages remain in inbox");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
