//! maxplayer-team (docs/specs/maxplayer-team.md rev 2): a team is a Buzz private channel plus
//! one private repo; a follow is an owner's posts in an open channel. Every incoming message and
//! every pulled commit is screened by the content service before it is shown or merged. It makes
//! no model call and starts no harness.
use base64::Engine as _;
use maxplayer_core::home::MaxplayerConfig;
use maxplayer_core::review::state::failure_state;
use maxplayer_core::review::team as review;
use maxplayer_core::review::Decision;
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
use std::process::Command;
use std::time::Duration;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Untrusted text made safe for a terminal: control characters, line/paragraph separators and
/// bidi/zero-width format characters are escaped, everything else is shown as is.
pub fn display_text(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || visual_format(c) {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
fn visual_format(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        | '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{200b}' | '\u{200c}'
        | '\u{feff}' | '\u{2060}')
}
/// JSON output with bidi/zero-width format characters escaped (serde already escapes controls).
pub fn display_json(value: serde_json::Value) -> String {
    value
        .to_string()
        .chars()
        .map(|c| if visual_format(c) { format!("\\u{:04x}", c as u32) } else { c.to_string() })
        .collect()
}
/// A team message is 1–8000 characters.
pub fn validate_text(text: &str) -> Result<()> {
    if text.is_empty() || text.chars().count() > 8000 {
        return Err("message must contain 1–8000 characters".into());
    }
    Ok(())
}
pub const DEFAULT_RELAY: &str = "wss://relay.maxplayer.ai";
/// `about` marker of a team channel: `maxplayer-team/1 <creator hex>` (spec §3).
pub const MARKER: &str = "maxplayer-team/1";
pub const SEND_DAILY_CAP: u32 = 50;
/// First sync of a join or follow takes the newest messages only.
pub const FIRST_SYNC: usize = 50;
/// Team review requests per key per rolling window (spec §5 pacing).
pub const REVIEW_CAP: usize = 1000;
pub const REVIEW_WINDOW: u64 = 1800;
/// Default follow on the default relay (spec §6): Team Maxplayer, posted by maxie-agent for now.
/// The channel must exist on the relay before a release ships this.
pub const TEAM_MAXPLAYER: (&str, &str) = (
    "93f1b02d-f8ac-4dea-aae0-576e6d9d5ee8",
    "5b298ab6c880322489d638b1923ebd299c98b50156f09681f17990edb7c8807e",
);

fn is_hex(s: &str, bytes: usize) -> bool {
    s.len() == bytes * 2 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn now() -> u64 {
    Timestamp::now().as_secs()
}
fn short(hex: &str) -> &str {
    &hex[..hex.len().min(8)]
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Team {
    pub name: String,
    pub channel: String,
    pub creator: String,
    /// `create --open`: an open feed channel; no repo, no invite/remove.
    #[serde(default)]
    pub open: bool,
}
impl Team {
    pub fn repo(&self) -> String {
        review::repo_id(&self.channel).expect("validated channel id")
    }
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Follow {
    pub channel: String,
    pub owner: String,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Retry {
    pub next: u64,
    pub delay: u64,
}
/// One stored message. Its signed verdict is kept and re-decided at read time, so a threshold or
/// reviewer-key change applies to old messages too.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Stored {
    pub channel: String,
    pub event: Event,
    #[serde(default)]
    pub verdict: Option<Event>,
    /// A final review error (permanent input error): never retried.
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub teams: BTreeMap<String, Team>,
    /// `None` = never edited: the default follow applies.
    #[serde(default)]
    pub follows: Option<Vec<Follow>>,
    #[serde(default)]
    pub cursors: BTreeMap<String, u64>,
    /// Shown inbox lines: a delivered message id, or `<id>:<state>` for a withheld one.
    #[serde(default)]
    pub read: BTreeSet<String>,
    #[serde(default)]
    pub sent_day: u64,
    #[serde(default)]
    pub sent: BTreeMap<String, u32>,
    /// Messages withheld for availability, by id.
    #[serde(default)]
    pub retry: BTreeMap<String, Retry>,
    /// `accept-verdict`: subject event (message id or commit subject) -> accepted verdict id.
    #[serde(default)]
    pub accepted: BTreeMap<String, String>,
    /// Signed commit verdicts by subject event.
    #[serde(default)]
    pub commit_verdicts: BTreeMap<String, Event>,
    #[serde(default)]
    pub review_times: Vec<u64>,
}
impl State {
    /// Exact channel id, local name, or a unique channel prefix of at least 8 characters.
    pub fn resolve(&self, query: &str) -> Result<&Team> {
        if let Some(t) = self.teams.get(query) {
            return Ok(t);
        }
        let hits: Vec<_> = self
            .teams
            .values()
            .filter(|t| t.name == query || (query.len() >= 8 && t.channel.starts_with(query)))
            .collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("no joined team named {}", display_text(query)).into()),
            _ => Err("several teams match; use the channel id".into()),
        }
    }
}

/// How a stored message reads under the current config.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Delivered,
    Withheld {
        state: String,
        unsafe_ppm: Option<u32>,
        verdict: Option<String>,
    },
}
fn withheld(state: &str) -> Status {
    Status::Withheld { state: state.into(), unsafe_ppm: None, verdict: None }
}

enum Outcome {
    Verdict(Option<Event>),
    Final(String),
    Retry,
}

/// Shape checks before anything is stored or reviewed (spec §5, §6): kind 9, valid signature,
/// one canonical `h` naming this channel, bounded text and size; a followed post must come from
/// its owner (checked here, not trusted to the relay filter).
pub fn admit(event: &Event, channel: &str, owner: Option<&str>) -> std::result::Result<(), String> {
    if owner.is_some_and(|o| event.pubkey.to_hex() != o) {
        return Err("not the followed owner".into());
    }
    if Some(review::message_repo(event)?) != review::repo_id(channel) {
        return Err("wrong channel".into());
    }
    Ok(())
}

/// Parse a 39000 `about` marker strictly: exactly `maxplayer-team/1 <64 lowercase hex>`.
pub fn parse_marker(about: &str) -> Option<String> {
    let (m, creator) = about.split_once(' ')?;
    (m == MARKER && is_hex(creator, 32)).then(|| creator.to_owned())
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

pub fn nip98(keys: &Keys, url: &str, method: HttpMethod) -> Result<String> {
    let event = EventBuilder::http_auth(HttpData::new(Url::parse(url)?, method))
        .tag(Tag::parse(["nonce", &Keys::generate().public_key().to_hex()])?)
        .sign_with_keys(keys)?;
    Ok(format!("Nostr {}", base64::engine::general_purpose::STANDARD.encode(event.as_json())))
}

fn tag_value(event: &Event, name: &str) -> Option<String> {
    event
        .tags
        .iter()
        .find(|t| t.as_slice().first().map(String::as_str) == Some(name))
        .and_then(|t| t.as_slice().get(1).cloned())
}
fn messages(channel: &str) -> Filter {
    Filter::new()
        .kind(Kind::Custom(9))
        .custom_tag(SingleLetterTag::lowercase(Alphabet::H), channel)
}

/// Spawns `--notify <cmd>` with `MAXPLAYER_TEAM=<channel id>`; one child at a time, a burst
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
            match Command::new(&self.cmd[0])
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

pub struct Home {
    pub root: PathBuf,
    pub keys: Keys,
    pub relay: String,
    pub config: MaxplayerConfig,
}

impl Home {
    /// The maxplayer home (created on first use, exactly as every other `maxplayer` command does):
    /// its key and its config (review settings).
    pub fn open(root: impl AsRef<Path>, relay: &str) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("home directories must not be symlinks".into());
        }
        let home = maxplayer_core::home::bootstrap(&root).map_err(|e| e.to_string())?;
        let secret = maxplayer_core::home::read_secret_key_hex(&home).map_err(|e| e.to_string())?;
        let keys = Keys::parse(&secret).map_err(|_| "invalid home key")?;
        let config = home.config;
        let dir = root.join("team");
        if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("home directories must not be symlinks".into());
        }
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir.join("quarantine"))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        http_base(relay)?;
        Ok(Self { root, keys, relay: relay.to_owned(), config })
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
    /// Stored messages in arrival order; a later line for the same id (a verdict after a retry)
    /// replaces the earlier one in place.
    pub fn stored(&self) -> Result<Vec<Stored>> {
        let file = match File::open(self.path("messages.jsonl")) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out: Vec<Stored> = vec![];
        let mut at: BTreeMap<EventId, usize> = BTreeMap::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let s: Stored = serde_json::from_str(&line)?;
            match at.get(&s.event.id) {
                Some(&i) => out[i] = s,
                None => {
                    at.insert(s.event.id, out.len());
                    out.push(s);
                }
            }
        }
        Ok(out)
    }
    fn append(&self, s: &Stored) -> Result<()> {
        let mut f = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path("messages.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(s)?)?;
        f.sync_all()?;
        Ok(())
    }
    pub fn follows(&self, state: &State) -> Vec<Follow> {
        state.follows.clone().unwrap_or_else(|| {
            if self.relay.trim_end_matches('/') == DEFAULT_RELAY {
                vec![Follow { channel: TEAM_MAXPLAYER.0.into(), owner: TEAM_MAXPLAYER.1.into() }]
            } else {
                vec![]
            }
        })
    }
    /// Local label for a channel (team name, or `follow:<id8>`).
    pub fn label(&self, state: &State, channel: &str) -> String {
        state
            .teams
            .get(channel)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| format!("follow:{}", short(channel)))
    }

    /// Re-decide a stored message against the current config and accepted verdicts.
    pub fn status(&self, state: &State, s: &Stored) -> Status {
        let author = s.event.pubkey.to_hex();
        if author == self.me() {
            return Status::Delivered;
        }
        let Ok(request) = review::Request::message(s.event.clone()) else {
            return withheld("invalid");
        };
        match self.config.review.enabled(request.subject.kind, &author) {
            Ok(false) => return Status::Delivered,
            Ok(true) => {}
            Err(_) => return withheld("config_error"),
        }
        if s.error.is_some() {
            return withheld("error");
        }
        let Some(verdict) = &s.verdict else {
            return withheld("pending");
        };
        let accepted = state.accepted.get(&request.subject.event).map(String::as_str);
        match review::decide(&self.config, &self.relay, verdict, &request.subject, accepted) {
            Ok(Decision::Passed) => Status::Delivered,
            Ok(Decision::Refused { unsafe_ppm }) => Status::Withheld {
                state: "refused".into(),
                unsafe_ppm: Some(unsafe_ppm),
                verdict: Some(verdict.id.to_hex()),
            },
            Ok(Decision::Error(_)) => withheld("error"),
            Err(_) => withheld("invalid_verdict"),
        }
    }

    // ---- relay ---------------------------------------------------------------------------

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
                        return Err("relay refused sign-in (NIP-42); run `maxplayer account link`".to_string())
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
            _ => Ok(client),
        }
    }
    async fn publish(&self, client: &Client, kind: u16, content: &str, tags: Vec<Vec<String>>) -> Result<Event> {
        let tags = tags.into_iter().map(Tag::parse).collect::<std::result::Result<Vec<_>, _>>()?;
        let event = EventBuilder::new(Kind::Custom(kind), content).tags(tags).sign_with_keys(&self.keys)?;
        let out = client.send_event(&event).await?;
        if out.success.is_empty() {
            let why = out.failed.values().next().cloned().unwrap_or_else(|| "refused".into());
            return Err(format!("relay refused: {}", display_text(&why)).into());
        }
        Ok(event)
    }
    /// Channel metadata (39000): (private, creator from the strict marker).
    async fn metadata(&self, client: &Client, channel: &str) -> Result<(bool, Option<String>)> {
        let events = client
            .fetch_events(Filter::new().kind(Kind::Custom(39000)).identifier(channel), Duration::from_secs(10))
            .await?;
        let event = events.into_iter().max_by_key(|e| e.created_at).ok_or("channel not found")?;
        let private = event.tags.iter().any(|t| t.as_slice() == ["private"]);
        Ok((private, tag_value(&event, "about").as_deref().and_then(parse_marker)))
    }

    // ---- team management -----------------------------------------------------------------

    pub async fn create(&self, name: &str, open: bool) -> Result<Team> {
        validate_name(name)?;
        if self.load()?.teams.values().any(|t| t.name == name) {
            return Err("you already have a team with that name".into());
        }
        let bytes = Keys::generate().secret_key().to_secret_bytes();
        let mut random = [0u8; 16];
        random.copy_from_slice(&bytes[..16]);
        let channel = uuid::Builder::from_random_bytes(random).into_uuid().hyphenated().to_string();
        let mut tags = vec![
            vec!["h".into(), channel.clone()],
            vec!["name".into(), name.into()],
            vec!["visibility".into(), if open { "open" } else { "private" }.into()],
            vec!["channel_type".into(), "stream".into()],
        ];
        if !open {
            tags.push(vec!["about".into(), format!("{MARKER} {}", self.me())]);
        }
        let client = self.connect().await?;
        let result = self.publish(&client, 9007, "", tags).await;
        client.shutdown().await;
        result?;
        let team = Team { name: name.into(), channel: channel.clone(), creator: self.me(), open };
        let _lock = self.lock()?;
        let mut state = self.load()?;
        state.teams.insert(channel, team.clone());
        self.save(&state)?;
        Ok(team)
    }
    async fn member_event(&self, team: &str, who: &str, kind: u16) -> Result<Team> {
        let team = self.load()?.resolve(team)?.clone();
        if team.open {
            return Err("an open channel has no members to manage".into());
        }
        let who = PublicKey::parse(who).map_err(|_| "not an npub or hex public key")?.to_hex();
        let client = self.connect().await?;
        let result = self
            .publish(&client, kind, "", vec![vec!["h".into(), team.channel.clone()], vec!["p".into(), who]])
            .await;
        client.shutdown().await;
        result?;
        Ok(team)
    }
    /// Any member may invite (kind 9000); repo access follows channel membership at once.
    pub async fn invite(&self, team: &str, who: &str) -> Result<Team> {
        self.member_event(team, who, 9000).await
    }
    /// Owner/admin only (kind 9001); the relay decides.
    pub async fn remove(&self, team: &str, who: &str) -> Result<Team> {
        self.member_event(team, who, 9001).await
    }
    pub async fn leave(&self, team: &str) -> Result<String> {
        let t = self.load()?.resolve(team)?.clone();
        if !t.open {
            let me = self.me();
            self.member_event(&t.channel, &me, 9001).await?;
        }
        let _lock = self.lock()?;
        let mut state = self.load()?;
        state.teams.remove(&t.channel);
        state.cursors.remove(&t.channel);
        self.save(&state)?;
        Ok(t.channel)
    }
    /// Private team channels this key was added to and has not joined: (channel, creator).
    pub async fn invites(&self) -> Result<Vec<(String, String)>> {
        let state = self.load()?;
        let client = self.connect().await?;
        let adds = client
            .fetch_events(Filter::new().kind(Kind::Custom(9000)).pubkey(self.keys.public_key()).limit(500), Duration::from_secs(10))
            .await;
        let mut out = vec![];
        let mut seen = BTreeSet::new();
        for event in adds.map(|e| e.into_iter().collect::<Vec<_>>()).unwrap_or_default() {
            let Some(channel) = tag_value(&event, "h") else { continue };
            if review::repo_id(&channel).is_none() || state.teams.contains_key(&channel) || !seen.insert(channel.clone()) {
                continue;
            }
            if let Ok((true, Some(creator))) = self.metadata(&client, &channel).await {
                out.push((channel, creator));
            }
        }
        client.shutdown().await;
        Ok(out)
    }
    /// Join a private team channel. Refuses an open channel, one without the team marker, and a
    /// followed channel. Names are local labels.
    pub async fn join(&self, channel: &str, name: Option<&str>) -> Result<Team> {
        review::repo_id(channel).ok_or("not a canonical lowercase channel UUID")?;
        let state = self.load()?;
        if self.follows(&state).iter().any(|f| f.channel == channel) {
            return Err("this channel is followed; `unfollow` it first".into());
        }
        let client = self.connect().await?;
        let meta = self.metadata(&client, channel).await;
        client.shutdown().await;
        let creator = match meta? {
            (true, Some(creator)) => creator,
            (false, _) => return Err("not a private channel; use `follow` for an open channel".into()),
            (true, None) => return Err("not a Maxplayer team channel".into()),
        };
        let name = name.map(str::to_owned).unwrap_or_else(|| channel[..8].to_owned());
        validate_name(&name)?;
        let _lock = self.lock()?;
        let mut state = self.load()?;
        if state.teams.values().any(|t| t.name == name) {
            return Err("you already use that name; pass --name".into());
        }
        let team = Team { name, channel: channel.into(), creator, open: false };
        state.teams.insert(channel.into(), team.clone());
        self.save(&state)?;
        Ok(team)
    }
    pub fn follow(&self, channel: &str, owner: &str) -> Result<()> {
        review::repo_id(channel).ok_or("not a canonical lowercase channel UUID")?;
        if !is_hex(owner, 32) {
            return Err("owner must be a 64-character lowercase hex public key".into());
        }
        let _lock = self.lock()?;
        let mut state = self.load()?;
        if state.teams.contains_key(channel) {
            return Err("this channel is joined; a channel is joined or followed, not both".into());
        }
        let mut follows = self.follows(&state);
        follows.retain(|f| f.channel != channel);
        follows.push(Follow { channel: channel.into(), owner: owner.into() });
        state.follows = Some(follows);
        self.save(&state)
    }
    pub fn unfollow(&self, channel: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        let mut follows = self.follows(&state);
        let before = follows.len();
        follows.retain(|f| f.channel != channel);
        if follows.len() == before {
            return Err("not followed".into());
        }
        state.follows = Some(follows);
        state.cursors.remove(channel);
        self.save(&state)
    }

    // ---- messages ------------------------------------------------------------------------

    pub async fn send(&self, team: &str, text: &str) -> Result<String> {
        validate_text(text)?;
        let channel = self.load()?.resolve(team)?.channel.clone();
        {
            let _lock = self.lock()?;
            let mut state = self.load()?;
            let day = now() / 86400;
            if state.sent_day != day {
                state.sent_day = day;
                state.sent.clear();
            }
            let count = state.sent.entry(channel.clone()).or_default();
            if *count >= SEND_DAILY_CAP {
                return Err(format!("daily send cap reached ({SEND_DAILY_CAP} per team per UTC day)").into());
            }
            *count += 1;
            self.save(&state)?;
        }
        let client = self.connect().await?;
        let result = self.publish(&client, 9, text, vec![vec!["h".into(), channel.clone()]]).await;
        client.shutdown().await;
        let event = result?;
        let _lock = self.lock()?;
        self.append(&Stored { channel, event: event.clone(), verdict: None, error: None })?;
        Ok(event.id.to_hex())
    }

    async fn review(&self, client: &Client, state: &mut State, request: &review::Request, counterparty: &str) -> Outcome {
        let t = now();
        state.review_times.retain(|x| x + REVIEW_WINDOW > t);
        if state.review_times.len() >= REVIEW_CAP {
            return Outcome::Retry;
        }
        state.review_times.push(t);
        match review::check(&self.config, &self.relay, client, &self.keys, request, counterparty).await {
            Ok(v) => Outcome::Verdict(v),
            Err(e) if failure_state(&e) == "error" => {
                eprintln!("review unavailable: {}", display_text(&e));
                Outcome::Retry
            }
            Err(e) => Outcome::Final(e),
        }
    }

    /// Admit, store and screen one message. `available` turns false at the first availability
    /// error, after which the pass stores messages for retry without asking the reviewer.
    /// Returns true when the message is delivered.
    async fn ingest(
        &self,
        client: &Client,
        state: &mut State,
        known: &mut BTreeSet<EventId>,
        channel: &str,
        owner: Option<&str>,
        event: Event,
        available: &mut bool,
    ) -> Result<bool> {
        state.cursors.insert(channel.into(), state.cursors.get(channel).copied().unwrap_or(0).max(event.created_at.as_secs().min(now())));
        if known.contains(&event.id) || admit(&event, channel, owner).is_err() {
            return Ok(false);
        }
        known.insert(event.id);
        let author = event.pubkey.to_hex();
        let mut stored = Stored { channel: channel.into(), event, verdict: None, error: None };
        if author != self.me() && self.config.review.enabled(9, &author)? {
            let request = review::Request::message(stored.event.clone()).map_err(|e| e.to_string())?;
            let outcome = if *available { self.review(client, state, &request, &author).await } else { Outcome::Retry };
            match outcome {
                Outcome::Verdict(v) => stored.verdict = v,
                Outcome::Final(e) => stored.error = Some(e),
                Outcome::Retry => {
                    *available = false;
                    state.retry.insert(stored.event.id.to_hex(), Retry { next: now() + 30, delay: 30 });
                }
            }
        }
        self.append(&stored)?;
        Ok(self.status(state, &stored) == Status::Delivered)
    }

    /// Retry due availability-withheld messages, each on its own back-off (30 s doubling to 1 h).
    async fn retry_due(&self, client: &Client, state: &mut State) -> Result<Vec<String>> {
        let due: Vec<String> = state.retry.iter().filter(|(_, r)| r.next <= now()).map(|(id, _)| id.clone()).collect();
        if due.is_empty() {
            return Ok(vec![]);
        }
        let all = self.stored()?;
        let mut delivered = vec![];
        for id in due {
            let Some(mut s) = all.iter().find(|s| s.event.id.to_hex() == id).cloned() else {
                state.retry.remove(&id);
                continue;
            };
            let request = review::Request::message(s.event.clone()).map_err(|e| e.to_string())?;
            match self.review(client, state, &request, &s.event.pubkey.to_hex()).await {
                Outcome::Retry => {
                    let r = state.retry.get_mut(&id).expect("due");
                    r.delay = (r.delay * 2).min(3600);
                    r.next = now() + r.delay;
                    break;
                }
                outcome => {
                    match outcome {
                        Outcome::Verdict(v) => s.verdict = v,
                        Outcome::Final(e) => s.error = Some(e),
                        Outcome::Retry => unreachable!(),
                    }
                    state.retry.remove(&id);
                    self.append(&s)?;
                    if self.status(state, &s) == Status::Delivered {
                        delivered.push(s.channel.clone());
                    }
                }
            }
        }
        Ok(delivered)
    }

    /// Every channel to read: joined teams (all authors) and follows (owner only).
    fn sources(&self, state: &State) -> Vec<(String, Option<String>)> {
        let mut out: Vec<_> = state.teams.keys().map(|c| (c.clone(), None)).collect();
        out.extend(self.follows(state).into_iter().map(|f| (f.channel, Some(f.owner))));
        out
    }
    fn source_filter(channel: &str, owner: Option<&str>) -> Result<Filter> {
        let mut f = messages(channel);
        if let Some(owner) = owner {
            f = f.author(PublicKey::from_hex(owner)?);
        }
        Ok(f)
    }

    /// Catch up every source from its cursor (inclusive `until` paging, dedupe by id); a first
    /// sync takes the newest [`FIRST_SYNC`]. Returns channels with newly delivered messages.
    async fn catch_up(&self, client: &Client, state: &mut State) -> Result<Vec<String>> {
        let mut known: BTreeSet<EventId> = self.stored()?.into_iter().map(|s| s.event.id).collect();
        let mut available = true;
        let mut delivered = vec![];
        for (channel, owner) in self.sources(state) {
            let base = Self::source_filter(&channel, owner.as_deref())?;
            let mut events: BTreeMap<EventId, Event> = BTreeMap::new();
            match state.cursors.get(&channel) {
                None => {
                    for e in client.fetch_events(base.limit(FIRST_SYNC), Duration::from_secs(10)).await? {
                        events.insert(e.id, e);
                    }
                }
                Some(&since) => {
                    let mut until = None;
                    for _ in 0..20 {
                        let mut f = base.clone().since(Timestamp::from(since)).limit(1000);
                        if let Some(u) = until {
                            f = f.until(u);
                        }
                        let page: Vec<Event> = client.fetch_events(f, Duration::from_secs(10)).await?.into_iter().collect();
                        let full = page.len() >= 1000;
                        let oldest = page.iter().map(|e| e.created_at).min();
                        let before = events.len();
                        for e in page {
                            events.insert(e.id, e);
                        }
                        match oldest {
                            Some(o) if full && events.len() > before => until = Some(o),
                            Some(_) if full => {
                                eprintln!("warning: more than 1000 messages in one second; some skipped");
                                break;
                            }
                            _ => break,
                        }
                    }
                }
            }
            let mut ordered: Vec<Event> = events.into_values().collect();
            ordered.sort_by_key(|e| e.created_at);
            for event in ordered {
                if self.ingest(client, state, &mut known, &channel, owner.as_deref(), event, &mut available).await? {
                    delivered.push(channel.clone());
                }
            }
        }
        if available {
            delivered.extend(self.retry_due(client, state).await?);
        }
        Ok(delivered)
    }

    /// One-shot catch-up (used by `inbox`).
    pub async fn sync(&self) -> Result<usize> {
        let client = self.connect().await?;
        let _lock = self.lock()?;
        let mut state = self.load()?;
        let result = self.catch_up(&client, &mut state).await;
        self.save(&state)?;
        client.shutdown().await;
        Ok(result?.len())
    }

    /// Stay connected; reconnect fresh every five minutes. Never returns except on a fatal error.
    pub async fn watch(&self, notify: Vec<String>) -> Result<()> {
        let mut notifier = Notifier::new(notify);
        let mut failures = 0u32;
        loop {
            match self.watch_once(&mut notifier).await {
                Ok(()) => failures = 0,
                Err(e) => {
                    failures += 1;
                    eprintln!("maxplayer-team watch: {}", display_text(&e.to_string()));
                    let until = tokio::time::Instant::now() + Duration::from_secs((2u64 << failures.min(5)).min(60));
                    while tokio::time::Instant::now() < until {
                        notifier.tick();
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        }
    }
    async fn watch_once(&self, notifier: &mut Notifier) -> Result<()> {
        let client = self.connect().await?;
        let start = Timestamp::now();
        let sources = {
            let _lock = self.lock()?;
            let mut state = self.load()?;
            let caught = self.catch_up(&client, &mut state).await;
            self.save(&state)?;
            caught?.into_iter().for_each(|c| notifier.note(c));
            self.sources(&state)
        };
        // One filter per source, never merged; at most 10 filters per REQ.
        let filters = sources
            .iter()
            .map(|(c, o)| Self::source_filter(c, o.as_deref()).map(|f| f.since(start)))
            .collect::<Result<Vec<_>>>()?;
        let mut notes = client.notifications();
        for chunk in filters.chunks(10) {
            for f in chunk {
                client.subscribe(f.clone(), None).await?;
            }
        }
        let refresh = tokio::time::Instant::now() + Duration::from_secs(300);
        let mut retry_tick = tokio::time::Instant::now() + Duration::from_secs(30);
        let result = loop {
            notifier.tick();
            if tokio::time::Instant::now() > refresh {
                break Ok(());
            }
            if tokio::time::Instant::now() > retry_tick {
                retry_tick = tokio::time::Instant::now() + Duration::from_secs(30);
                let _lock = self.lock()?;
                let mut state = self.load()?;
                let delivered = self.retry_due(&client, &mut state).await;
                self.save(&state)?;
                delivered?.into_iter().for_each(|c| notifier.note(c));
            }
            match tokio::time::timeout(Duration::from_millis(500), notes.recv()).await {
                Err(_) => {}
                Ok(Ok(RelayPoolNotification::Event { event, .. })) => {
                    let Some(channel) = tag_value(&event, "h") else { continue };
                    let Some((_, owner)) = sources.iter().find(|(c, _)| *c == channel) else { continue };
                    let _lock = self.lock()?;
                    let mut state = self.load()?;
                    let mut known: BTreeSet<EventId> = self.stored()?.into_iter().map(|s| s.event.id).collect();
                    let mut available = true;
                    let delivered = self
                        .ingest(&client, &mut state, &mut known, &channel, owner.as_deref(), *event, &mut available)
                        .await;
                    self.save(&state)?;
                    match delivered {
                        Ok(true) => notifier.note(channel),
                        Ok(false) => {}
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

    /// Inbox lines not shown before, oldest first: delivered messages, and each withheld state
    /// once. Marks them shown.
    pub fn take_inbox(&self) -> Result<Vec<(Stored, Status)>> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        let mut out = vec![];
        for s in self.stored()? {
            if s.event.pubkey.to_hex() == self.me() {
                continue;
            }
            let status = self.status(&state, &s);
            let key = match &status {
                Status::Delivered => s.event.id.to_hex(),
                Status::Withheld { state: st, .. } => format!("{}:{st}", s.event.id),
            };
            if state.read.insert(key) {
                out.push((s, status));
            }
        }
        self.save(&state)?;
        Ok(out)
    }
    pub fn log(&self, team: &str) -> Result<Vec<(Stored, Status)>> {
        let state = self.load()?;
        let channel = match state.resolve(team) {
            Ok(t) => t.channel.clone(),
            Err(e) => self.follows(&state).into_iter().find(|f| f.channel == team).map(|f| f.channel).ok_or(e)?,
        };
        Ok(self
            .stored()?
            .into_iter()
            .filter(|s| s.channel == channel)
            .map(|s| {
                let st = self.status(&state, &s);
                (s, st)
            })
            .collect())
    }
    /// Override a `Refused` verdict for one message id or commit oid. Errors stay final.
    pub fn accept_verdict(&self, team: &str, id: &str, verdict: &str) -> Result<()> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        let t = state.resolve(team)?.clone();
        let subject = if id.len() == 40 {
            let event = state
                .commit_verdicts
                .get(&maxplayer_core::review::team_commit_event(&t.repo(), id))
                .filter(|v| v.id.to_hex() == verdict)
                .ok_or("no refused verdict with that id for this commit")?;
            let request = review::Request::commit(&t.repo(), &t.creator, id).map_err(|e| e.to_string())?;
            match review::decide(&self.config, &self.relay, event, &request.subject, None)? {
                Decision::Refused { .. } => request.subject.event,
                _ => return Err("only a Refused verdict can be accepted".into()),
            }
        } else {
            let s = self
                .stored()?
                .into_iter()
                .find(|s| s.channel == t.channel && s.event.id.to_hex() == id)
                .ok_or("no such message in this team")?;
            match self.status(&state, &s) {
                Status::Withheld { state: st, verdict: Some(v), .. } if st == "refused" && v == verdict => id.to_owned(),
                _ => return Err("only a Refused verdict with that id can be accepted".into()),
            }
        };
        state.accepted.insert(subject, verdict.into());
        self.save(&state)
    }

    // ---- git -----------------------------------------------------------------------------

    pub fn repo_url(&self, team: &Team) -> Result<String> {
        if team.open {
            return Err("an open channel has no repo".into());
        }
        Ok(format!("{}/git/{}/{}", http_base(&self.relay)?, team.creator, team.repo()))
    }
    /// Git with no global/system config and no prompts; the NIP-98 header goes only to `auth`.
    fn git(&self, dir: &Path, auth: Option<&str>, args: &[&str]) -> Result<std::process::Output> {
        let mut cmd = Command::new("git");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("-C")
            .arg(dir);
        if let Some(url) = auth {
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", format!("http.{url}.extraHeader"))
                .env("GIT_CONFIG_VALUE_0", format!("Authorization: {}", nip98(&self.keys, url, HttpMethod::POST)?));
        }
        Ok(cmd.args(args).output()?)
    }
    fn git_ok(&self, dir: &Path, auth: Option<&str>, args: &[&str]) -> Result<String> {
        let out = self.git(dir, auth, args)?;
        if !out.status.success() {
            return Err(format!("git {}: {}", args[0], display_text(String::from_utf8_lossy(&out.stderr).trim())).into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
    fn worktree(&self, team: &Team, dir: &Path) -> Result<String> {
        let url = self.repo_url(team)?;
        if self.git_ok(dir, None, &["remote", "get-url", "origin"])? != url {
            return Err("this directory is not a clone of that team's repo (origin differs)".into());
        }
        Ok(url)
    }

    /// Clone = init + origin + a screened pull of the default branch.
    pub async fn clone_repo(&self, team: &str, dir: Option<&str>) -> Result<String> {
        let t = self.load()?.resolve(team)?.clone();
        let url = self.repo_url(&t)?;
        let dir = PathBuf::from(dir.unwrap_or(&t.name));
        if dir.exists() {
            return Err("target directory already exists".into());
        }
        fs::create_dir_all(&dir)?;
        self.git_ok(&dir, None, &["init", "-q"])?;
        self.git_ok(&dir, None, &["remote", "add", "origin", &url])?;
        let head = self.git_ok(&dir, Some(&url), &["ls-remote", "--symref", &url, "HEAD"])?;
        let Some(branch) = head
            .lines()
            .find_map(|l| l.strip_prefix("ref: refs/heads/").and_then(|r| r.split('\t').next()))
            .map(str::to_owned)
        else {
            return Ok(format!("cloned an empty repo into {}", dir.display()));
        };
        self.pull_in(&t, &dir, &branch).await
    }
    pub async fn pull(&self, team: &str, branch: Option<&str>) -> Result<String> {
        let t = self.load()?.resolve(team)?.clone();
        let dir = std::env::current_dir()?;
        let branch = match branch {
            Some(b) => b.to_owned(),
            None => self.git_ok(&dir, None, &["symbolic-ref", "--short", "HEAD"])?,
        };
        self.pull_in(&t, &dir, &branch).await
    }
    /// Fetch into the quarantine repo, require every new commit to pass review, then move the
    /// passed tip to `origin/<branch>` and fast-forward.
    async fn pull_in(&self, t: &Team, dir: &Path, branch: &str) -> Result<String> {
        let url = self.worktree(t, dir)?;
        let q = self.path(&format!("quarantine/{}.git", t.repo()));
        if !q.exists() {
            Command::new("git").args(["init", "--bare", "-q"]).arg(&q).status()?;
        }
        let qref = format!("refs/quarantine/{branch}");
        self.git_ok(&q, Some(&url), &["fetch", "-q", "--no-tags", &url, &format!("+refs/heads/{branch}:{qref}")])?;
        let tip = self.git_ok(&q, None, &["rev-parse", &format!("{qref}^{{commit}}")])?;
        let origin = format!("refs/remotes/origin/{branch}");
        let base = self.git_ok(dir, None, &["rev-parse", "--verify", "-q", &format!("{origin}^{{commit}}")]).ok();
        if base.as_deref() == Some(tip.as_str()) {
            return Ok(format!("{branch} is up to date"));
        }
        if let Some(base) = &base {
            if !self.git(&q, None, &["merge-base", "--is-ancestor", base, &tip])?.status.success() {
                return Err(format!("origin/{branch} was rewritten; refusing (start a new branch from the last passed commit)").into());
            }
        }
        let range = match &base {
            Some(b) => vec!["rev-list".to_owned(), "--reverse".into(), tip.clone(), format!("^{b}")],
            None => vec!["rev-list".to_owned(), "--reverse".into(), tip.clone()],
        };
        let commits = self.git_ok(&q, None, &range.iter().map(String::as_str).collect::<Vec<_>>())?;
        let commits: Vec<&str> = commits.lines().filter(|l| !l.is_empty()).collect();
        let client = self.connect().await?;
        let mut failure = None;
        {
            let _lock = self.lock()?;
            let mut state = self.load()?;
            for c in &commits {
                if let Err(e) = self.screen_commit(&client, &mut state, t, c).await {
                    failure = Some(e);
                    break;
                }
            }
            self.save(&state)?;
        }
        client.shutdown().await;
        if let Some(e) = failure {
            return Err(e);
        }
        self.git_ok(dir, None, &["fetch", "-q", "--no-tags", &q.to_string_lossy(), &format!("+{qref}:{origin}")])?;
        if self.git_ok(dir, None, &["rev-parse", &origin])? != tip {
            return Err("quarantine changed during pull; run pull again".into());
        }
        let current = self.git_ok(dir, None, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
        let has_head = self.git(dir, None, &["rev-parse", "--verify", "-q", "HEAD"])?.status.success();
        if !has_head {
            self.git_ok(dir, None, &["checkout", "-q", "-B", branch, &origin])?;
        } else if current == branch {
            if !self.git(dir, None, &["merge", "--ff-only", "-q", &origin])?.status.success() {
                return Ok(format!("{} passed review; origin/{branch} updated; your {branch} has local commits: rebase onto origin/{branch}", commits.len()));
            }
        }
        Ok(format!("{} new commit(s) passed review; origin/{branch} at {}", commits.len(), short(&tip)))
    }
    async fn screen_commit(&self, client: &Client, state: &mut State, t: &Team, commit: &str) -> Result<()> {
        if !self.config.review.enabled(maxplayer_core::kinds::TEAM_COMMIT_SUBJECT_KIND, "")? {
            return Ok(());
        }
        let request = review::Request::commit(&t.repo(), &t.creator, commit).map_err(|e| e.to_string())?;
        let key = request.subject.event.clone();
        let verdict = match state.commit_verdicts.get(&key) {
            Some(v) => v.clone(),
            None => match self.review(client, state, &request, "").await {
                Outcome::Verdict(Some(v)) => {
                    state.commit_verdicts.insert(key.clone(), v.clone());
                    v
                }
                Outcome::Verdict(None) => return Ok(()),
                Outcome::Final(e) => return Err(format!("commit {}: {}", short(commit), display_text(&e)).into()),
                Outcome::Retry => return Err(format!("commit {}: review unavailable; pull again later", short(commit)).into()),
            },
        };
        let accepted = state.accepted.get(&key).map(String::as_str);
        match review::decide(&self.config, &self.relay, &verdict, &request.subject, accepted)? {
            Decision::Passed => Ok(()),
            Decision::Refused { unsafe_ppm } => Err(format!(
                "commit {commit} withheld: unsafe {unsafe_ppm} ppm >= threshold {} ppm; verdict {}; `accept-verdict {} {commit} {}` overrides",
                self.config.review.reject_at_or_above_ppm, verdict.id, display_text(&t.name), verdict.id
            )
            .into()),
            Decision::Error(code) => Err(format!("commit {}: review error {code}", short(commit)).into()),
        }
    }

    /// Push after refusing what the reviewer cannot read: binary/NUL/non-UTF-8 blobs, NUL or
    /// non-UTF-8 commit text, symlinks and submodules.
    pub fn push(&self, team: &str, branch: Option<&str>) -> Result<String> {
        let t = self.load()?.resolve(team)?.clone();
        let dir = std::env::current_dir()?;
        let url = self.worktree(&t, &dir)?;
        let branch = match branch {
            Some(b) => b.to_owned(),
            None => self.git_ok(&dir, None, &["symbolic-ref", "--short", "HEAD"])?,
        };
        let head = self.git_ok(&dir, None, &["rev-parse", &format!("refs/heads/{branch}^{{commit}}")])?;
        let origin = format!("refs/remotes/origin/{branch}");
        let base = self.git_ok(&dir, None, &["rev-parse", "--verify", "-q", &origin]).ok();
        let mut args = vec!["rev-list".to_owned(), head.clone()];
        if let Some(b) = &base {
            args.push(format!("^{b}"));
        }
        let commits = self.git_ok(&dir, None, &args.iter().map(String::as_str).collect::<Vec<_>>())?;
        for c in commits.lines().filter(|l| !l.is_empty()) {
            self.check_pushable(&dir, c)?;
        }
        self.git_ok(&dir, Some(&url), &["push", "-q", &url, &format!("{head}:refs/heads/{branch}")])?;
        self.git_ok(&dir, None, &["update-ref", &origin, &head])?;
        Ok(format!("pushed {branch} at {}", short(&head)))
    }
    fn check_pushable(&self, dir: &Path, commit: &str) -> Result<()> {
        let refuse = |why: &str| -> Result<()> { Err(format!("commit {} not pushed: {why}", short(commit)).into()) };
        let raw = self.git(dir, None, &["cat-file", "commit", commit])?.stdout;
        if raw.contains(&0) || std::str::from_utf8(&raw).is_err() {
            return refuse("commit header or message has NUL or non-UTF-8 bytes");
        }
        let out = self.git(dir, None, &["diff-tree", "-r", "-z", "--raw", "--root", "-m", "--no-commit-id", commit])?;
        let fields: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
        let mut i = 0;
        while i + 1 < fields.len() {
            let meta = std::str::from_utf8(fields[i]).unwrap_or("");
            let path = fields[i + 1];
            i += 2;
            let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            if parts.len() < 5 {
                continue;
            }
            if std::str::from_utf8(path).is_err() {
                return refuse("non-UTF-8 path");
            }
            match parts[1] {
                "120000" => return refuse("symlinks are not supported"),
                "160000" => return refuse("submodules are not supported"),
                "000000" => continue,
                _ => {}
            }
            let blob = self.git(dir, None, &["cat-file", "blob", parts[3]])?.stdout;
            if blob.contains(&0) || std::str::from_utf8(&blob).is_err() {
                return refuse(&format!("{} is binary or not UTF-8", display_text(&String::from_utf8_lossy(path))));
            }
        }
        Ok(())
    }
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
mod tests;
