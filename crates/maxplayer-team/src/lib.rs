//! maxplayer-team: a mailbox plus a shared private git repo for a group of agents
//! (docs/specs/maxplayer-team.md). It makes no model call and starts no harness.
//!
//! The relay holds each team's member list (`/api/teams`); this crate only caches it.
//! Every message is NIP-59 wrapped once per member plus once for the Maxplayer service.
use base64::Engine as _;
use maxplayer_chat::{display_text, validate_text};
use maxplayer_core::private_content::transport;
use nostr_sdk::nostr::hashes::{sha256, Hash as _};
use nostr_sdk::nostr::nips::nip98::{HttpData, HttpMethod};
use nostr_sdk::pool::RelayNotification;
use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub const SCHEMA: &str = "maxplayer-team/1";
pub const DEFAULT_RELAY: &str = "wss://relay.maxplayer.ai";
/// Spec §7: what stops two agents replying to each other forever.
pub const SEND_DAILY_CAP: u32 = 50;
pub const INBOUND_DAILY_CAP: usize = 100;
pub const MAX_PENDING_INVITES: usize = 20;

fn is_hex(s: &str, bytes: usize) -> bool {
    s.len() == bytes * 2 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn random_id() -> String {
    Keys::generate().public_key().to_hex()
}
fn now() -> u64 {
    Timestamp::now().as_secs()
}

/// Rumor content of every team message (spec §5), decoded `deny_unknown_fields`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub schema: String,
    pub team: String,
    pub owner: String,
    pub msg: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub version: i64,
    pub recipients: Vec<String>,
    pub text: String,
}

/// The relay's view of a team (`GET /api/teams/{owner}/{team_id}`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TeamInfo {
    pub owner: String,
    pub team_id: String,
    pub members: Vec<String>,
    pub service: String,
    pub version: i64,
    pub repo: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Joined {
    pub name: String,
    pub info: TeamInfo,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Invite {
    pub name: String,
    pub owner: String,
    pub team_id: String,
    pub at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub team: String,
    pub msg: String,
    pub from: String,
    pub dir: String,
    pub kind: String,
    pub at: u64,
    pub text: String,
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub teams: BTreeMap<String, Joined>,
    #[serde(default)]
    pub invites: Vec<Invite>,
    #[serde(default)]
    pub cursor: u64,
    #[serde(default)]
    pub inbox_seen: usize,
    #[serde(default)]
    pub sent_day: u64,
    #[serde(default)]
    pub sent: BTreeMap<String, u32>,
}

impl State {
    /// Exact id, local name, or a unique id prefix of at least 8 characters.
    pub fn resolve(&self, query: &str) -> Result<&Joined> {
        if let Some(t) = self.teams.get(query) {
            return Ok(t);
        }
        let hits: Vec<_> = self
            .teams
            .values()
            .filter(|t| t.name == query || (query.len() >= 8 && t.info.team_id.starts_with(query)))
            .collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("no joined team named {}", display_text(query)).into()),
            _ => Err("several teams match; use the team id".into()),
        }
    }
}

struct Lock(#[allow(dead_code)] File);
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

pub struct Home {
    pub root: PathBuf,
    pub keys: Keys,
    pub relay: String,
}

/// Spawns `--notify <cmd>` with `MAXPLAYER_TEAM=<team_id>`; one child at a time, a burst
/// during a run becomes one pending wake-up.
pub struct Notifier {
    cmd: Vec<String>,
    child: Option<std::process::Child>,
    pending: Option<String>,
}
impl Notifier {
    pub fn new(cmd: Vec<String>) -> Self {
        Self { cmd, child: None, pending: None }
    }
    fn note(&mut self, team: String) {
        if !self.cmd.is_empty() {
            self.pending = Some(team);
        }
    }
    fn tick(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return;
            }
            self.child = None;
        }
        if let Some(team) = self.pending.take() {
            match std::process::Command::new(&self.cmd[0])
                .args(&self.cmd[1..])
                .env("MAXPLAYER_TEAM", team)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(child) => self.child = Some(child),
                Err(_) => eprintln!("notify failed; messages remain in inbox"),
            }
        }
    }
}

pub fn http_base(relay: &str) -> Result<String> {
    let relay = relay.trim().trim_end_matches('/');
    if let Some(rest) = relay.strip_prefix("wss://") {
        Ok(format!("https://{rest}"))
    } else if let Some(rest) = relay.strip_prefix("ws://") {
        Ok(format!("http://{rest}"))
    } else {
        Err("relay URL must start with wss:// or ws://".into())
    }
}

/// NIP-98 header. A random nonce tag keeps two requests in one second distinct (the
/// relay refuses a replayed event id).
pub fn nip98(keys: &Keys, url: &str, method: HttpMethod, body: Option<&[u8]>) -> Result<String> {
    let mut data = HttpData::new(Url::parse(url)?, method);
    if let Some(body) = body {
        data = data.payload(sha256::Hash::hash(body));
    }
    let event = EventBuilder::http_auth(data)
        .tag(Tag::parse(["nonce", &random_id()])?)
        .sign_with_keys(keys)?;
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(event.as_json())
    ))
}

impl Home {
    /// Uses chat's home-key loader unchanged, so both binaries share `<home>/key`.
    pub fn open(root: impl AsRef<Path>, relay: &str) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let keys = maxplayer_chat::Home::open(&root)?.keys;
        let dir = root.join("team");
        if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("home directories must not be symlinks".into());
        }
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        http_base(relay)?;
        Ok(Self { root, keys, relay: relay.to_owned() })
    }
    pub fn me(&self) -> String {
        self.keys.public_key().to_hex()
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join("team").join(name)
    }
    fn lock(&self) -> Result<Lock> {
        let file = private_file(&self.path("state.lock"))?;
        // SAFETY: valid owned descriptor; flock changes only its advisory lock.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Lock(file))
    }
    pub fn load(&self) -> Result<State> {
        match fs::read_to_string(self.path("state.json")) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e.into()),
        }
    }
    fn save(&self, state: &State) -> Result<()> {
        let tmp = self.path("state.json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(state)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(tmp, self.path("state.json"))?;
        Ok(())
    }
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let file = match File::open(self.path("messages.jsonl")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = vec![];
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.trim().is_empty() {
                out.push(serde_json::from_str(&line)?);
            }
        }
        Ok(out)
    }
    fn append(&self, entry: &Entry) -> Result<()> {
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path("messages.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(entry)?)?;
        f.sync_all()?;
        Ok(())
    }

    // ---- relay HTTP API --------------------------------------------------------------

    async fn api(&self, put: Option<Vec<u8>>, path: &str) -> Result<TeamInfo> {
        let url = format!("{}{path}", http_base(&self.relay)?);
        let method = if put.is_some() { HttpMethod::PUT } else { HttpMethod::GET };
        let auth = nip98(&self.keys, &url, method, put.as_deref())?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        let request = match put {
            Some(body) => client
                .put(&url)
                .header("content-type", "application/json")
                .body(body),
            None => client.get(&url),
        };
        let response = request.header("authorization", auth).send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(format!("relay refused ({status}): {}", display_text(text.trim())).into());
        }
        let info: TeamInfo = serde_json::from_str(&text)?;
        if !is_hex(&info.owner, 32) || !is_hex(&info.team_id, 32) || !is_hex(&info.service, 32) {
            return Err("relay returned an invalid team".into());
        }
        Ok(info)
    }
    pub async fn fetch_team(&self, owner: &str, team_id: &str) -> Result<TeamInfo> {
        self.api(None, &format!("/api/teams/{owner}/{team_id}")).await
    }
    async fn put_team(&self, team_id: &str, members: &[String], expected: i64) -> Result<TeamInfo> {
        let body = serde_json::to_vec(&serde_json::json!({"members": members, "expected_version": expected}))?;
        self.api(Some(body), &format!("/api/teams/{team_id}")).await
    }

    // ---- relay connection ------------------------------------------------------------

    async fn connect(&self) -> Result<Client> {
        let client = Client::builder()
            .signer(self.keys.clone())
            .opts(ClientOptions::new().automatic_authentication(true))
            .build();
        client.add_relay(&self.relay).await?;
        let relay = client.relay(&self.relay).await?;
        let mut notes = relay.notifications();
        client.connect().await;
        let signed_in = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match notes.recv().await {
                    Ok(RelayNotification::Authenticated) => return Ok(()),
                    Ok(RelayNotification::AuthenticationFailed) => {
                        return Err("relay refused sign-in (NIP-42)".to_string())
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("relay connection: {e}")),
                }
            }
        })
        .await;
        match signed_in {
            Ok(Err(e)) => {
                client.shutdown().await;
                Err(e.into())
            }
            // A relay that issues no challenge is not a failure; reads may still be refused.
            _ => Ok(client),
        }
    }

    // ---- team management -------------------------------------------------------------

    pub async fn create(&self, name: &str) -> Result<Joined> {
        validate_name(name)?;
        if self.load()?.teams.values().any(|t| t.name == name) {
            return Err("you already have a team with that name".into());
        }
        let team_id = random_id();
        let info = self.put_team(&team_id, &[], 0).await?;
        let joined = Joined { name: name.into(), info };
        let _lock = self.lock()?;
        let mut state = self.load()?;
        state.teams.insert(team_id, joined.clone());
        self.save(&state)?;
        Ok(joined)
    }

    async fn change_members(&self, team: &str, who: &str, add: bool) -> Result<Joined> {
        let joined = self.load()?.resolve(team)?.clone();
        if joined.info.owner != self.me() {
            return Err("only the team's creator can change its members".into());
        }
        let who = PublicKey::parse(who).map_err(|_| "not an npub or hex public key")?.to_hex();
        if who == self.me() {
            return Err("the creator is always a member".into());
        }
        let fresh = self.fetch_team(&joined.info.owner, &joined.info.team_id).await?;
        let mut members: BTreeSet<String> = fresh.members.iter().cloned().collect();
        if add {
            members.insert(who);
        } else if !members.remove(&who) {
            return Err("not a member".into());
        }
        let members: Vec<String> = members.into_iter().collect();
        let info = self.put_team(&fresh.team_id, &members, fresh.version).await?;
        let joined = Joined { name: joined.name, info };
        let _lock = self.lock()?;
        let mut state = self.load()?;
        state.teams.insert(joined.info.team_id.clone(), joined.clone());
        self.save(&state)?;
        Ok(joined)
    }
    /// Adds the key to the relay's member list (repo access is immediate), then sends the
    /// team an `invite` message. The invitee's client only stores the team's messages
    /// after `join`.
    pub async fn invite(&self, team: &str, who: &str) -> Result<SendReport> {
        let joined = self.change_members(team, who, true).await?;
        self.send_kind(&joined.info.team_id, &joined.name, "invite").await
    }
    pub async fn remove(&self, team: &str, who: &str) -> Result<Joined> {
        self.change_members(team, who, false).await
    }
    pub async fn join(&self, query: &str) -> Result<Joined> {
        let state = self.load()?;
        let hits: Vec<_> = state
            .invites
            .iter()
            .filter(|i| i.team_id == query || i.name == query || (query.len() >= 8 && i.team_id.starts_with(query)))
            .cloned()
            .collect();
        let invite = match hits.as_slice() {
            [one] => one.clone(),
            [] => return Err("no pending invite matches; run `invites`".into()),
            _ => return Err("several invites match; use the team id".into()),
        };
        // Proves the relay lists us, and gives the current member list.
        let info = self.fetch_team(&invite.owner, &invite.team_id).await?;
        let mut name = invite.name.clone();
        let _lock = self.lock()?;
        let mut state = self.load()?;
        if state.teams.values().any(|t| t.name == name) {
            name = format!("{name}-{}", &info.team_id[..8]);
        }
        let joined = Joined { name, info };
        state.invites.retain(|i| i.team_id != invite.team_id);
        state.teams.insert(joined.info.team_id.clone(), joined.clone());
        self.save(&state)?;
        Ok(joined)
    }
    pub fn leave(&self, team: &str) -> Result<String> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        let id = state.resolve(team)?.info.team_id.clone();
        state.teams.remove(&id);
        self.save(&state)?;
        Ok(id)
    }

    // ---- messages --------------------------------------------------------------------

    pub async fn send(&self, team: &str, text: &str) -> Result<SendReport> {
        let id = self.load()?.resolve(team)?.info.team_id.clone();
        self.send_kind(&id, text, "text").await
    }

    async fn send_kind(&self, team_id: &str, text: &str, kind: &str) -> Result<SendReport> {
        validate_text(text)?;
        let joined = self.load()?.resolve(team_id)?.clone();
        // The relay's list, not our cache: a removed member must stop receiving at once.
        let info = self.fetch_team(&joined.info.owner, &joined.info.team_id).await?;
        let me = self.me();
        if !info.members.contains(&me) {
            return Err("you are no longer a member of this team".into());
        }
        {
            let _lock = self.lock()?;
            let mut state = self.load()?;
            let day = now() / 86400;
            if state.sent_day != day {
                state.sent_day = day;
                state.sent.clear();
            }
            let count = state.sent.entry(info.team_id.clone()).or_default();
            if *count >= SEND_DAILY_CAP {
                return Err(format!("daily send cap reached ({SEND_DAILY_CAP} per team per UTC day)").into());
            }
            *count += 1;
            if let Some(t) = state.teams.get_mut(&info.team_id) {
                t.info = info.clone();
            }
            self.save(&state)?;
        }
        let mut recipients: BTreeSet<String> = info.members.iter().cloned().collect();
        recipients.insert(info.service.clone());
        let envelope = Envelope {
            schema: SCHEMA.into(),
            team: info.team_id.clone(),
            owner: info.owner.clone(),
            msg: random_id(),
            kind: kind.into(),
            version: info.version,
            recipients: recipients.iter().cloned().collect(),
            text: text.into(),
        };
        let body = serde_json::to_string(&envelope)?;
        let mut wraps = vec![];
        for r in recipients.iter().filter(|r| **r != me) {
            let pk = PublicKey::from_hex(r)?;
            wraps.push((r.clone(), transport::wrap(&self.keys, pk, body.clone()).await?));
        }
        let client = self.connect().await?;
        let mut report = SendReport { msg: envelope.msg.clone(), delivered: vec![], failed: vec![] };
        for (who, event) in &wraps {
            match client.send_event(event).await {
                Ok(out) if !out.success.is_empty() => report.delivered.push(who.clone()),
                Ok(out) => report.failed.push((
                    who.clone(),
                    out.failed.values().next().cloned().unwrap_or_else(|| "refused".into()),
                )),
                Err(e) => report.failed.push((who.clone(), e.to_string())),
            }
        }
        client.shutdown().await;
        if !report.delivered.is_empty() || wraps.is_empty() {
            let _lock = self.lock()?;
            self.append(&Entry {
                team: info.team_id,
                msg: envelope.msg,
                from: me,
                dir: "out".into(),
                kind: kind.into(),
                at: now(),
                text: text.into(),
            })?;
        }
        Ok(report)
    }

    /// Authenticate, filter and store wrapped events. Returns the team ids that got a new
    /// message or invite. Plaintext of a refused message is never logged.
    pub async fn ingest(&self, events: Vec<Event>) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        let entries = self.entries()?;
        let mut seen: BTreeSet<String> =
            entries.iter().map(|e| format!("{}:{}", e.team, e.msg)).collect();
        let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
        let today = now() / 86400;
        for e in entries.iter().filter(|e| e.dir == "in" && e.at / 86400 == today) {
            *counts.entry((e.team.clone(), e.from.clone())).or_default() += 1;
        }
        let me = self.me();
        let mut stored = vec![];
        for event in events {
            state.cursor = state.cursor.max(event.created_at.as_secs().min(now()));
            let Ok((sender, body)) = transport::unwrap_message(&self.keys, &event).await else {
                continue;
            };
            let Ok(env) = serde_json::from_str::<Envelope>(&body) else {
                continue;
            };
            let sender = sender.to_hex();
            if env.schema != SCHEMA
                || !is_hex(&env.team, 32)
                || !is_hex(&env.owner, 32)
                || !is_hex(&env.msg, 32)
                || validate_text(&env.text).is_err()
                || !env.recipients.contains(&me)
                || !matches!(env.kind.as_str(), "text" | "invite")
                || seen.contains(&format!("{}:{}", env.team, env.msg))
            {
                continue;
            }
            if !state.teams.contains_key(&env.team) {
                // Unjoined team: only the owner's invite is kept, nothing else.
                if env.kind == "invite"
                    && sender == env.owner
                    && validate_name(&env.text).is_ok()
                    && !state.invites.iter().any(|i| i.team_id == env.team)
                {
                    state.invites.push(Invite {
                        name: env.text.clone(),
                        owner: env.owner.clone(),
                        team_id: env.team.clone(),
                        at: now(),
                    });
                    if state.invites.len() > MAX_PENDING_INVITES {
                        state.invites.remove(0);
                    }
                    stored.push(env.team.clone());
                }
                continue;
            }
            let joined = state.teams.get_mut(&env.team).expect("checked");
            if joined.info.owner != env.owner {
                continue;
            }
            if !joined.info.members.contains(&sender) || env.version > joined.info.version {
                match self.fetch_team(&joined.info.owner, &joined.info.team_id).await {
                    Ok(info) => joined.info = info,
                    Err(_) => continue,
                }
                if !joined.info.members.contains(&sender) {
                    continue;
                }
            }
            if !env.recipients.contains(&joined.info.service) {
                continue;
            }
            let count = counts.entry((env.team.clone(), sender.clone())).or_default();
            if *count >= INBOUND_DAILY_CAP {
                continue;
            }
            *count += 1;
            seen.insert(format!("{}:{}", env.team, env.msg));
            self.append(&Entry {
                team: env.team.clone(),
                msg: env.msg,
                from: sender,
                dir: "in".into(),
                kind: env.kind,
                at: now(),
                text: env.text,
            })?;
            stored.push(env.team);
        }
        self.save(&state)?;
        Ok(stored)
    }

    fn filter(&self, since: u64) -> Filter {
        Filter::new()
            .kind(Kind::GiftWrap)
            .pubkey(self.keys.public_key())
            .since(Timestamp::from(since))
    }

    /// One-shot catch-up from the stored cursor.
    pub async fn sync(&self) -> Result<usize> {
        let since = transport::receive_since(self.load()?.cursor);
        let client = self.connect().await?;
        let events = client.fetch_events(self.filter(since), Duration::from_secs(10)).await;
        client.shutdown().await;
        Ok(self.ingest(events?.into_iter().collect()).await?.len())
    }

    /// Stay connected; reconnect fresh every five minutes so a silent drop can't hide
    /// messages for long. Never returns except on a fatal local error.
    pub async fn watch(&self, notify: Vec<String>) -> Result<()> {
        let mut notifier = Notifier::new(notify);
        let mut failures = 0u32;
        loop {
            match self.watch_once(&mut notifier).await {
                Ok(()) => failures = 0,
                Err(e) => {
                    failures += 1;
                    eprintln!("maxplayer-team watch: {}", display_text(&e.to_string()));
                    let delay = Duration::from_secs((2u64 << failures.min(5)).min(60));
                    let until = tokio::time::Instant::now() + delay;
                    while tokio::time::Instant::now() < until {
                        notifier.tick();
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
    async fn watch_once(&self, notifier: &mut Notifier) -> Result<()> {
        let since = transport::receive_since(self.load()?.cursor);
        let client = self.connect().await?;
        let mut notes = client.notifications();
        client.subscribe(self.filter(since), None).await?;
        let refresh = tokio::time::Instant::now() + Duration::from_secs(300);
        let result = loop {
            notifier.tick();
            if tokio::time::Instant::now() > refresh {
                break Ok(());
            }
            match tokio::time::timeout(Duration::from_millis(500), notes.recv()).await {
                Err(_) => {}
                Ok(Ok(RelayPoolNotification::Event { event, .. })) => {
                    match self.ingest(vec![*event]).await {
                        Ok(teams) => teams.into_iter().for_each(|t| notifier.note(t)),
                        Err(e) => break Err(e),
                    }
                }
                Ok(Ok(RelayPoolNotification::Shutdown)) => break Err("relay pool shut down".into()),
                Ok(Ok(_)) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => break Ok(()),
                Ok(Err(e)) => break Err(e.into()),
            }
        };
        client.shutdown().await;
        result
    }

    /// Unread incoming messages and invites, oldest first; marks them read.
    pub fn take_inbox(&self) -> Result<Vec<Entry>> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        let entries = self.entries()?;
        let new: Vec<Entry> = entries
            .iter()
            .skip(state.inbox_seen.min(entries.len()))
            .filter(|e| e.dir == "in")
            .cloned()
            .collect();
        state.inbox_seen = entries.len();
        self.save(&state)?;
        Ok(new)
    }

    // ---- git -------------------------------------------------------------------------

    /// Run git with a fresh NIP-98 header scoped (by git's URL matching) to the team repo.
    /// The token goes through `GIT_CONFIG_*`, so it is never in argv or `.git/config`.
    pub fn git(&self, team: &str, args: &[String]) -> Result<i32> {
        let repo = self.load()?.resolve(team)?.info.repo.clone();
        let header = nip98(&self.keys, &repo, HttpMethod::POST, None)?;
        let status = std::process::Command::new("git")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", format!("http.{repo}.extraHeader"))
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: {header}"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(args)
            .status()?;
        Ok(status.code().unwrap_or(1))
    }
    pub fn clone_repo(&self, team: &str, dir: Option<&str>) -> Result<i32> {
        let joined = self.load()?.resolve(team)?.clone();
        let dir = dir.map(str::to_owned).unwrap_or_else(|| joined.name.clone());
        self.git(team, &["clone".into(), joined.info.repo.clone(), dir])
    }
}

#[derive(Debug, Serialize)]
pub struct SendReport {
    pub msg: String,
    pub delivered: Vec<String>,
    pub failed: Vec<(String, String)>,
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.chars().count() > 64
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || name.starts_with('.')
    {
        return Err("team names are 1–64 characters of letters, digits, '-', '_' or '.'".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(dir: &Path) -> Home {
        // Unreachable relay: any membership refetch fails closed (message dropped).
        Home::open(dir, "ws://127.0.0.1:9").unwrap()
    }
    fn team(owner: &Keys, members: &[&Keys], service: &Keys) -> TeamInfo {
        TeamInfo {
            owner: owner.public_key().to_hex(),
            team_id: "ab".repeat(32),
            members: members.iter().map(|k| k.public_key().to_hex()).collect(),
            service: service.public_key().to_hex(),
            version: 1,
            repo: "http://127.0.0.1:9/git/x/y".into(),
        }
    }
    async fn wrapped(from: &Keys, to: &Keys, env: &Envelope) -> Event {
        transport::wrap(from, to.public_key(), serde_json::to_string(env).unwrap())
            .await
            .unwrap()
    }
    fn envelope(info: &TeamInfo, kind: &str, text: &str, recipients: Vec<String>) -> Envelope {
        Envelope {
            schema: SCHEMA.into(),
            team: info.team_id.clone(),
            owner: info.owner.clone(),
            msg: random_id(),
            kind: kind.into(),
            version: info.version,
            recipients,
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn stores_member_messages_once_and_drops_outsiders_unjoined_and_serviceless() {
        let dir = tempfile::tempdir().unwrap();
        let h = home(dir.path());
        let (owner, service, outsider) = (Keys::generate(), Keys::generate(), Keys::generate());
        let info = team(&owner, &[&owner, &h.keys], &service);
        let all = vec![info.owner.clone(), h.me(), info.service.clone()];

        // Before joining: a text is dropped, the owner's invite is kept, a non-owner's isn't.
        let text = wrapped(&owner, &h.keys, &envelope(&info, "text", "hi", all.clone())).await;
        let invite = wrapped(&owner, &h.keys, &envelope(&info, "invite", "crew", all.clone())).await;
        let fake = wrapped(&outsider, &h.keys, &envelope(&info, "invite", "evil", all.clone())).await;
        assert_eq!(h.ingest(vec![text, fake]).await.unwrap().len(), 0);
        assert_eq!(h.ingest(vec![invite]).await.unwrap().len(), 1);
        let state = h.load().unwrap();
        assert_eq!(state.invites.len(), 1);
        assert_eq!(state.invites[0].name, "crew");

        // Joined (cache only; join itself needs the relay).
        {
            let _l = h.lock().unwrap();
            let mut s = h.load().unwrap();
            s.invites.clear();
            s.teams.insert(info.team_id.clone(), Joined { name: "crew".into(), info: info.clone() });
            h.save(&s).unwrap();
        }
        let env = envelope(&info, "text", "hello team", all.clone());
        let a = wrapped(&owner, &h.keys, &env).await;
        let b = wrapped(&owner, &h.keys, &env).await; // second copy of the same message
        assert_eq!(h.ingest(vec![a, b]).await.unwrap().len(), 1);
        // Outsider (refetch fails closed) and a message without the service copy: dropped.
        let o = wrapped(&outsider, &h.keys, &envelope(&info, "text", "x", all.clone())).await;
        let no_service = vec![info.owner.clone(), h.me()];
        let s = wrapped(&owner, &h.keys, &envelope(&info, "text", "y", no_service)).await;
        assert_eq!(h.ingest(vec![o, s]).await.unwrap().len(), 0);
        let inbox = h.take_inbox().unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].text, "hello team");
        assert!(h.take_inbox().unwrap().is_empty(), "marked read");
    }

    #[test]
    fn resolves_by_name_id_or_unique_prefix() {
        let mut s = State::default();
        for (name, id) in [("a", "ab".repeat(32)), ("b", "ac".repeat(32))] {
            s.teams.insert(id.clone(), Joined {
                name: name.into(),
                info: TeamInfo { owner: "11".repeat(32), team_id: id, members: vec![], service: "22".repeat(32), version: 1, repo: String::new() },
            });
        }
        assert_eq!(s.resolve("a").unwrap().name, "a");
        assert_eq!(s.resolve(&"ac".repeat(32)).unwrap().name, "b");
        assert_eq!(s.resolve("acacacac").unwrap().name, "b");
        assert!(s.resolve("zz").is_err());
        assert!(validate_name("../x").is_err() && validate_name("crew-1").is_ok());
    }

    #[test]
    fn relay_url_maps_to_http_api_base() {
        assert_eq!(http_base("wss://relay.example/").unwrap(), "https://relay.example");
        assert_eq!(http_base("ws://100.1.2.3:7790").unwrap(), "http://100.1.2.3:7790");
        assert!(http_base("https://x").is_err());
    }
}
