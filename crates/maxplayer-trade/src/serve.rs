//! Local hand-off from a CLI invocation to a running `serve` over `<home>/serve.sock`.
//!
//! `serve` holds `owner.lock` and stays the only writer: a handed-off `list`, `take`, `cancel`
//! or `receive` is queued to serve's own loop and executed there, between inbox messages and recovery ticks,
//! by the same coordinator functions (journal before effect) the CLI would have called, with
//! exactly the bounds the client sent. The client only relays output. A client that disconnects
//! never cancels a queued or running request; a take keeps being driven by serve.
//!
//! A `receive` carries the token text over the socket only. serve logs [`Command::public`]
//! (kind, mint and the token's public receive id), never the request; [`TokenText`] never prints
//! through `Debug`; refusals sent back are redacted. Idempotency is the receive journal's: the
//! same token always maps to the same attempt id, so a retry after a lost reply or a restart
//! resumes that attempt and never credits twice.
//!
//! The socket is mode 0600 inside the 0700 home. Both ends check the other with SO_PEERCRED
//! against their own effective uid; the client also requires the socket file to be its own.
//! serve refuses a request whose market or mint relay set differs from its own wherever the
//! command depends on it ([`Command::relay_needs`]). One request per connection; the request is at most
//! [`MAX_REQUEST_BYTES`]; reads and writes are bounded by [`IO_TIMEOUT`]; at most
//! [`MAX_CONNECTIONS`] connections are served at once.
use crate::{Asset, Leg, coordinator, journal::Journal, market::Market, observe, receive};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{mpsc, oneshot},
};

pub const SOCKET: &str = "serve.sock";
/// Room for the largest token `receive` accepts plus the request envelope (mint, relays).
pub const MAX_REQUEST_BYTES: usize = receive::MAX_TOKEN_BYTES + 16 * 1024;
pub const MAX_REPLY_LINE_BYTES: usize = 64 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_CONNECTIONS: usize = 8;
/// Server keepalive cadence while a request runs or a take is streamed.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// The client gives up on a silent serve after this long (keepalives arrive every 15 s).
pub const CLIENT_IDLE: Duration = Duration::from_secs(90);
/// How long a client whose home is locked keeps looking for a serve socket (serve may be starting).
pub const CONNECT_WAIT: Duration = Duration::from_secs(5);
/// 2: requests carry the client's effective `nostr://` mint relays (`mint_relays`).
/// 3: `send` and `fund` hand-offs (T3b).
pub const PROTOCOL_V: u8 = 3;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    List {
        give_mint: String,
        give: u64,
        want_mint: String,
        want: u64,
        max_fees: u64,
    },
    Take {
        lot: String,
        max_give: u64,
        min_receive: u64,
        max_fees: u64,
    },
    Cancel {
        lot: String,
    },
    Receive {
        /// The mint exactly as given on the command line; serve canonicalizes it as the CLI does.
        mint: String,
        token: TokenText,
    },
    /// `send <mint> --amount N --out PATH [--max-fees F]`. `out` is absolute (the client resolved
    /// it): serve, running as the same user, creates exactly that file. The send is keyed on it,
    /// so the same request again resumes that attempt and never sends twice.
    Send {
        mint: String,
        amount: u64,
        out: String,
        /// Exactly as given: `None` refuses a mint that charges input fees, as the CLI does.
        max_fees: Option<u64>,
    },
    /// `fund <mint> --amount N` under the client's operation id (a UUID printed before the
    /// hand-off). The same id again returns the same journaled quote and invoice. `quote`: resume
    /// polling a journaled quote (`--quote`).
    Fund {
        mint: String,
        amount: u64,
        id: String,
        quote: Option<String>,
    },
}
impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::List { .. } => "list",
            Command::Take { .. } => "take",
            Command::Cancel { .. } => "cancel",
            Command::Receive { .. } => "receive",
            Command::Send { .. } => "send",
            Command::Fund { .. } => "fund",
        }
    }
    /// The serve work keys this request may write (see `sched`): proof selection and new
    /// swaps/listings take `admit`; a take also every taker swap; a cancel its lot; a receive
    /// only its mint's wallet journal.
    pub fn keys(&self) -> Vec<String> {
        match self {
            Command::List { .. } => vec!["admit".into()],
            Command::Take { .. } => vec!["admit".into(), "taker".into()],
            Command::Cancel { lot } => vec!["admit".into(), format!("lot:{lot}")],
            Command::Receive { mint, .. } | Command::Fund { mint, .. } => vec![wallet_key(mint)],
            // Selects proofs (`admit`: never alongside a list/take choosing inputs) and writes
            // its mint's wallet journal.
            Command::Send { mint, .. } => vec!["admit".into(), wallet_key(mint)],
        }
    }
    /// Which of the client's relay sets this command depends on, so serve can refuse a request
    /// it would otherwise run on different relays: `(market, mint)`. Market relays: list, take
    /// and cancel publish or read lots; a receive never touches the market. Mint relays: any
    /// command that may reach a `nostr://` mint. take and cancel act on a lot whose mints the
    /// client does not know, so they always compare them.
    pub fn relay_needs(&self) -> (bool, bool) {
        let nostr = crate::transport::is_nostr;
        match self {
            Command::List {
                give_mint,
                want_mint,
                ..
            } => (true, nostr(give_mint) || nostr(want_mint)),
            Command::Take { .. } | Command::Cancel { .. } => (true, true),
            Command::Receive { mint, .. } | Command::Send { mint, .. } => (false, nostr(mint)),
            // fund refuses nostr:// mints before the hand-off.
            Command::Fund { .. } => (false, false),
        }
    }
    /// What serve may log about a request: public bounds and ids only, never a payload. Each
    /// variant names its fields explicitly so a new secret-bearing field is not logged by default.
    /// Same `{"<kind>":{..}}` shape the log always had for list/take/cancel.
    pub fn public(&self) -> serde_json::Value {
        let fields = match self {
            Command::List {
                give_mint,
                give,
                want_mint,
                want,
                max_fees,
            } => serde_json::json!({"give_mint":give_mint,"give":give,"want_mint":want_mint,
                "want":want,"max_fees":max_fees}),
            Command::Take {
                lot,
                max_give,
                min_receive,
                max_fees,
            } => serde_json::json!({"lot":lot,"max_give":max_give,"min_receive":min_receive,
                "max_fees":max_fees}),
            Command::Cancel { lot } => serde_json::json!({"lot":lot}),
            Command::Receive { mint, token } => {
                serde_json::json!({"mint":mint,"receive":receive_id(mint, &token.0)})
            }
            Command::Send {
                mint,
                amount,
                out,
                max_fees,
            } => serde_json::json!({"mint":mint,"amount":amount,"out":out,"max_fees":max_fees}),
            Command::Fund {
                mint,
                amount,
                id,
                quote,
            } => serde_json::json!({"mint":mint,"amount":amount,"funding_id":id,"quote":quote}),
        };
        serde_json::json!({ self.name(): fields })
    }
}
/// The token text of a handed-off `receive`. It crosses only the 0600 same-uid socket; `Debug`
/// withholds it, so no `{:?}` of a request, job or panic payload can print it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenText(pub String);
impl std::fmt::Debug for TokenText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<token withheld>")
    }
}
/// The wallet work key of `mint` as the CLI names it (canonical form when it parses).
fn wallet_key(mint: &str) -> String {
    format!(
        "wallet:{}",
        Asset::from_cli(mint)
            .map(|a| a.mint_url)
            .unwrap_or_else(|_| mint.to_owned())
    )
}
/// The public receive attempt id of `token` (static parse, no network), when it parses.
pub fn receive_id(mint: &str, token: &str) -> Option<String> {
    let asset = Asset::from_cli(mint).ok()?;
    receive::inspect(&asset.mint_url, token).ok().map(|p| p.id)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u8,
    /// The client's resolved market relay set; serve refuses a request for a different set when
    /// the command uses the market ([`Command::relay_needs`]).
    pub relays: Vec<String>,
    /// The client's effective `nostr://` mint relays (`--mint-relay`, or the defaults). serve
    /// refuses a different set when the command may reach a `nostr://` mint, instead of silently
    /// running it on serve's own mint relays.
    pub mint_relays: Vec<String>,
    pub command: Command,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Stdout(String),
    Stderr(String),
    Keepalive,
    /// A handed-off send's token file as serve created it: (`st_dev`, `st_ino`). The client
    /// checks that its own `--out` is that same file before reporting success.
    Written {
        dev: u64,
        ino: u64,
    },
    Exit {
        code: u8,
        error: Option<String>,
    },
}

/// A request queued to serve's loop.
pub struct Job {
    pub command: Command,
    diagnostics: crate::diagnostics::Lines,
    reply: oneshot::Sender<std::result::Result<Outcome, Failure>>,
}
#[derive(Debug)]
pub struct Outcome {
    pub stdout: Vec<String>,
    pub swap: Option<String>,
    /// A funding whose issuance serve's recovery keeps checking: streamed after `stdout`.
    pub funding: Option<(String, String)>,
    /// Printed to the client's stderr after `stdout`.
    pub stderr: Vec<String>,
    /// A send's written token file (`st_dev`, `st_ino`) in serve's filesystem view.
    pub written: Option<(u64, u64)>,
    /// Exit code and message after `stdout` was printed (the CLI's code for that result).
    pub failure: Option<Failure>,
}
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub message: String,
}
/// Only a peer with the socket owner's uid may talk to serve (both directions).
fn peer_allowed(peer: u32, owner: u32) -> Result<()> {
    ensure!(peer == owner, "peer uid {peer} refused");
    Ok(())
}
/// Client side: hand a request (possibly a token) only to a serve whose socket peer and socket
/// file are both owned by the client's effective uid.
fn trusted_serve(peer: u32, socket_owner: u32, me: u32) -> Result<()> {
    peer_allowed(socket_owner, me).context("serve socket is owned by another user; refusing")?;
    peer_allowed(peer, me).context("serve socket peer runs as another user; refusing")
}
/// Same exit-code mapping as the CLI.
pub fn exit_code(error: &anyhow::Error) -> u8 {
    if error.is::<coordinator::RecoveryIncomplete>() {
        3
    } else if error.is::<coordinator::ManualRecovery>() {
        4
    } else {
        1
    }
}

/// Path used to bind/connect. Unix socket paths are limited to ~107 bytes; a longer home is
/// reached through its open directory descriptor (`/proc/self/fd/N/serve.sock`, Linux).
struct SocketPath {
    path: PathBuf,
    _dir: Option<std::fs::File>,
}
fn socket_path(home: &Path) -> Result<SocketPath> {
    let direct = home.join(SOCKET);
    if direct.as_os_str().len() < 100 {
        return Ok(SocketPath {
            path: direct,
            _dir: None,
        });
    }
    use std::os::fd::AsRawFd;
    let dir = std::fs::File::open(home).context("open home for socket")?;
    Ok(SocketPath {
        path: PathBuf::from(format!("/proc/self/fd/{}/{SOCKET}", dir.as_raw_fd())),
        _dir: Some(dir),
    })
}

/// Bind the socket. Call only while holding `owner.lock`: any existing socket file is then
/// stale (its serve no longer holds the lock) and is replaced.
pub fn bind(home: &Path, relays: Vec<String>) -> Result<mpsc::Receiver<Job>> {
    let file = home.join(SOCKET);
    match std::fs::symlink_metadata(&file) {
        Ok(meta) => {
            ensure!(
                meta.file_type().is_socket_like(),
                "{} exists and is not a socket; refusing to replace it",
                file.display()
            );
            std::fs::remove_file(&file).context("remove stale serve socket")?;
            eprintln!("{}", serde_json::json!({"serve_socket":"stale_removed"}));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let sp = socket_path(home)?;
    let listener = UnixListener::bind(&sp.path).context("bind serve socket")?;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))?;
    let owner = rustix::process::geteuid().as_raw();
    ensure!(
        std::fs::symlink_metadata(&file)?.uid() == owner,
        "serve socket is not owned by serve's user; refusing"
    );
    let (tx, rx) = mpsc::channel(MAX_CONNECTIONS);
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let home = home.to_path_buf();
    tokio::spawn(async move {
        let _dir = sp;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let tx = tx.clone();
            let relays = relays.clone();
            let home = home.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = connection(stream, owner, &home, &relays, tx).await {
                    eprintln!(
                        "{}",
                        serde_json::json!({"serve_client":"closed","reason":crate::redact_reason(&e.to_string())})
                    );
                }
            });
        }
    });
    eprintln!("{}", serde_json::json!({"serve_socket":file}));
    Ok(rx)
}
trait SocketLike {
    fn is_socket_like(&self) -> bool;
}
impl SocketLike for std::fs::FileType {
    fn is_socket_like(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        self.is_socket()
    }
}

/// One bounded line from a PERSISTENT buffered reader (a per-call reader would drop any
/// read-ahead lines).
async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<Option<String>> {
    let mut buf = Vec::new();
    let n = (&mut *r)
        .take(max as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    ensure!(
        buf.last() == Some(&b'\n'),
        "unterminated line or line exceeds {max} bytes"
    );
    buf.pop();
    ensure!(buf.len() <= max, "line exceeds {max} bytes");
    Ok(Some(String::from_utf8(buf)?))
}
async fn write(stream: &mut UnixStream, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_string(reply)?;
    line.push('\n');
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .context("client write timed out")??;
    Ok(())
}

/// Why serve refuses `request` before queueing it, if it does: a protocol mismatch, or a relay
/// set the command depends on ([`Command::relay_needs`]) that differs from serve's own. Serve
/// never runs a request on relays other than the ones the client resolved. The requested relays
/// are caller text (a mistyped flag may hold a token), so the message is redacted.
fn refusal(request: &Request, relays: &[String], mint_relays: &[String]) -> Option<String> {
    if request.v != PROTOCOL_V {
        return Some(format!(
            "serve protocol version mismatch (serve {PROTOCOL_V}, client {}); run the same \
             maxplayer-trade version as serve",
            request.v
        ));
    }
    let (market, mint) = request.command.relay_needs();
    if market && request.relays != relays {
        return Some(crate::redact_reason(&format!(
            "serve uses relays {relays:?}; this command asked for {:?}. Use the same --relay set as serve",
            request.relays
        )));
    }
    if mint && !crate::transport::same_mint_relays(&request.mint_relays, mint_relays) {
        return Some(crate::redact_reason(&format!(
            "serve uses mint relays {mint_relays:?}; this command asked for {:?}. Use the same \
             --mint-relay set as serve",
            request.mint_relays
        )));
    }
    None
}

async fn connection(
    mut stream: UnixStream,
    owner: u32,
    home: &Path,
    relays: &[String],
    tx: mpsc::Sender<Job>,
) -> Result<()> {
    peer_allowed(stream.peer_cred()?.uid(), owner)?;
    // One request per connection: nothing after its line is read.
    let read = tokio::time::timeout(
        IO_TIMEOUT,
        read_line_bounded(&mut BufReader::new(&mut stream), MAX_REQUEST_BYTES),
    )
    .await
    .context("request read timed out")?;
    let line = match read {
        Ok(line) => line.context("empty request")?,
        Err(e) => {
            // Never echoes request bytes: the bound and decode errors carry no content.
            let error = format!("serve request refused, nothing executed: {e}");
            let _ = write(
                &mut stream,
                &Reply::Exit {
                    code: 1,
                    error: Some(error.clone()),
                },
            )
            .await;
            bail!(error)
        }
    };
    let request: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            // serde can quote a mistyped value, which may be token text: redact it.
            let error = format!(
                "invalid serve request: {}",
                crate::redact_reason(&e.to_string())
            );
            write(
                &mut stream,
                &Reply::Exit {
                    code: 1,
                    error: Some(error.clone()),
                },
            )
            .await?;
            bail!(error)
        }
    };
    if let Some(error) = refusal(&request, relays, &crate::transport::mint_relays()) {
        write(
            &mut stream,
            &Reply::Exit {
                code: 1,
                error: Some(error.clone()),
            },
        )
        .await?;
        bail!(error)
    }
    let (reply, rx) = oneshot::channel();
    let diagnostics = crate::diagnostics::Lines::default();
    tx.send(Job {
        diagnostics: diagnostics.clone(),
        command: request.command,
        reply,
    })
    .await
    .context("serve loop stopped")?;
    // The job runs in serve's loop whether or not this client stays connected.
    tokio::pin!(rx);
    let outcome = loop {
        tokio::select! {
            r = &mut rx => break r.context("serve loop dropped the request")?,
            _ = tokio::time::sleep(KEEPALIVE) => write(&mut stream, &Reply::Keepalive).await?,
        }
    };
    let lines = diagnostics
        .lock()
        .map(|lines| lines.clone())
        .unwrap_or_default();
    for line in lines {
        write(&mut stream, &Reply::Stderr(line)).await?;
    }
    let outcome = match outcome {
        Ok(o) => o,
        Err(f) => {
            return write(
                &mut stream,
                &Reply::Exit {
                    code: f.code,
                    error: Some(f.message),
                },
            )
            .await;
        }
    };
    for line in outcome.stdout {
        write(&mut stream, &Reply::Stdout(line)).await?;
    }
    for line in outcome.stderr {
        write(&mut stream, &Reply::Stderr(line)).await?;
    }
    if let Some((dev, ino)) = outcome.written {
        write(&mut stream, &Reply::Written { dev, ino }).await?;
    }
    if let Some(f) = outcome.failure {
        return write(
            &mut stream,
            &Reply::Exit {
                code: f.code,
                error: Some(f.message),
            },
        )
        .await;
    }
    if let Some((id, mint)) = outcome.funding {
        return stream_funding(&mut stream, home, &id, &mint).await;
    }
    let Some(id) = outcome.swap else {
        return write(
            &mut stream,
            &Reply::Exit {
                code: 0,
                error: None,
            },
        )
        .await;
    };
    write(
        &mut stream,
        &Reply::Stderr(format!(
            "serve is watching swap {id}; it keeps driving it to settlement even if this command exits. Follow it with `status`."
        )),
    )
    .await?;
    stream_take(&mut stream, home, &id).await
}

/// Follow one funding in the journal (read-only) while serve's recovery issues it: exit 0 once
/// it is credited, 1 when the CLI's polling window passes first (the intent stays journaled and
/// serve keeps checking it). Never waits inline in serve's loop.
async fn stream_funding(stream: &mut UnixStream, home: &Path, id: &str, mint: &str) -> Result<()> {
    let window = crate::money::fund_poll(mint) * crate::money::FUND_POLLS + Duration::from_secs(10);
    let end = tokio::time::Instant::now() + window;
    let mut quiet = tokio::time::Instant::now();
    loop {
        let rows = match observe::journal_rows(home, "funding") {
            Ok(rows) => rows,
            // A busy journal: one retry on the next tick, then report it.
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                observe::journal_rows(home, "funding")?
            }
        };
        let row = rows.into_iter().find(|(k, _, _)| k == id);
        if let Some((_, v, _)) = row {
            let f: crate::money::Funding = serde_json::from_slice(&v)?;
            if f.expired_unpaid {
                return write(
                    stream,
                    &Reply::Exit {
                        code: 1,
                        error: Some(format!(
                            "funding {id}: quote expired unpaid; nothing to pay and nothing is \
                             checked further; start a new funding"
                        )),
                    },
                )
                .await;
            }
            if f.done {
                write(
                    stream,
                    &Reply::Stderr(
                        serde_json::json!({"funding_id":id,"state":"issued"}).to_string(),
                    ),
                )
                .await?;
                return write(
                    stream,
                    &Reply::Exit {
                        code: 0,
                        error: None,
                    },
                )
                .await;
            }
        }
        if tokio::time::Instant::now() >= end {
            return write(
                stream,
                &Reply::Exit {
                    code: 1,
                    error: Some(format!(
                        "funding remains pending; invoice and intent retained; serve keeps checking \
                         funding {id} (see `status`; re-run with --id {id} to follow it again)"
                    )),
                },
            )
            .await;
        }
        if quiet.elapsed() >= KEEPALIVE {
            write(stream, &Reply::Keepalive).await?;
            quiet = tokio::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Stream one swap's state from the journal (read-only) until it is terminal.
async fn stream_take(stream: &mut UnixStream, home: &Path, id: &str) -> Result<()> {
    let mut last = String::new();
    let mut warned = observe::Warned::default();
    let mut quiet = tokio::time::Instant::now();
    // This swap's progress lines (display only, already redacted), as serve prints them.
    let mut progress = crate::progress::subscribe();
    loop {
        while let Ok((swap, line)) = progress.try_recv() {
            if swap == id {
                write(stream, &Reply::Stderr(line)).await?;
                quiet = tokio::time::Instant::now();
            }
        }
        let row = observe::journal_rows(home, "swap")?
            .into_iter()
            .find(|(k, _, _)| k == id);
        if let Some((_, v, updated)) = row {
            let s: coordinator::Swap = serde_json::from_slice(&v)?;
            if s.state != last {
                last = s.state.clone();
                write(
                    stream,
                    &Reply::Stderr(serde_json::json!({"swap_id":id,"state":s.state}).to_string()),
                )
                .await?;
                quiet = tokio::time::Instant::now();
            }
            let now = coordinator::now();
            let current: Vec<_> = observe::derive(
                &s.id,
                &s.role,
                &s.state,
                updated,
                observe::refund_available_at(&s),
                now,
                observe::unresponsive_after(),
            )
            .into_iter()
            .collect();
            for u in warned.due(&current) {
                write(stream, &Reply::Stderr(u.json(now).to_string())).await?;
            }
            if coordinator::terminal(&s) {
                for line in terminal_lines(&s) {
                    write(stream, &Reply::Stdout(line)).await?;
                }
                let ok = ["complete", "complete_unclaimed"].contains(&s.state.as_str());
                return write(
                    stream,
                    &Reply::Exit {
                        code: if ok { 0 } else { 1 },
                        error: (!ok).then(|| format!("trade ended {}", s.state)),
                    },
                )
                .await;
            }
        }
        if quiet.elapsed() >= KEEPALIVE {
            write(stream, &Reply::Keepalive).await?;
            quiet = tokio::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
/// The stdout lines a CLI `take` prints when its swap ends in this state.
pub fn terminal_lines(s: &coordinator::Swap) -> Vec<String> {
    let mut out = vec![];
    if s.state == "complete" {
        out.push(serde_json::json!({"swap_id":s.id,"state":"complete","role":s.role}).to_string());
    }
    if s.state.ends_with("_quarantined") {
        out.push(
            serde_json::json!({"swap_id":s.id,"state":s.state,"manual_recovery":true}).to_string(),
        );
    }
    out.push(serde_json::json!({"swap_id":s.id,"state":s.state}).to_string());
    out
}

/// Answer a queued request that serve will not start (it is stopping): exit 1, nothing applied.
pub fn refuse(job: Job, why: &str) {
    eprintln!(
        "{}",
        serde_json::json!({"serve_request":job.command.name(),"result":"not_started","reason":why})
    );
    let _ = job.reply.send(Err(Failure {
        code: 1,
        message: why.to_owned(),
    }));
}

/// Execute one queued request inside serve's loop. Never panics the loop.
pub async fn execute(home: &Path, j: &Journal, m: &Market, job: Job) {
    let name = job.command.name();
    // Public view only: a request (a receive's token) is never serialized into the log.
    eprintln!(
        "{}",
        serde_json::json!({"serve_request":name,"bounds":job.command.public()})
    );
    let secret = matches!(job.command, Command::Receive { .. });
    let result = crate::diagnostics::scoped(job.diagnostics, run_command(home, j, m, &job.command))
        .await
        .map_err(|e| Failure {
            code: exit_code(&e),
            message: if secret {
                crate::redact_reason(&e.to_string())
            } else {
                e.to_string()
            },
        });
    match &result {
        Ok(Outcome {
            failure: Some(f), ..
        }) => eprintln!(
            "{}",
            serde_json::json!({"serve_request":name,"result":"exit","exit":f.code,"error":f.message})
        ),
        Ok(o) => eprintln!(
            "{}",
            serde_json::json!({"serve_request":name,"result":"ok","swap_id":o.swap})
        ),
        Err(f) => eprintln!(
            "{}",
            serde_json::json!({"serve_request":name,"result":"error","exit":f.code,"error":f.message})
        ),
    }
    let _ = job.reply.send(result);
}
async fn run_command(home: &Path, j: &Journal, m: &Market, c: &Command) -> Result<Outcome> {
    match c {
        Command::List {
            give_mint,
            give,
            want_mint,
            want,
            max_fees,
        } => {
            let id = coordinator::list(
                home,
                j,
                m,
                Leg {
                    asset: Asset::new(give_mint)?,
                    net: *give,
                },
                Leg {
                    asset: Asset::new(want_mint)?,
                    net: *want,
                },
                *max_fees,
            )
            .await?;
            let l: coordinator::Listing = j.get("listing", &id).await?.context("listing")?;
            Ok(Outcome {
                stdout: vec![serde_json::json!({"lot_id":id,"status_event_id":l.statuses[0].id,"status":"available","debit":l.plan.debit}).to_string()],
                swap: None,
                funding: None,
                stderr: vec![],
                written: None,
                failure: None,
            })
        }
        Command::Cancel { lot } => {
            coordinator::cancel(home, j, m, lot).await?;
            let l: coordinator::Listing = j.get("listing", lot).await?.context("listing")?;
            Ok(Outcome {
                stdout: vec![serde_json::json!({"lot_id":l.event.id,"status_event_ids":l.statuses.iter().map(|e|e.id.to_hex()).collect::<Vec<_>>(),"status":crate::lifecycle(&l.event,&l.statuses)?}).to_string()],
                swap: None,
                funding: None,
                stderr: vec![],
                written: None,
                failure: None,
            })
        }
        Command::Take {
            lot,
            max_give,
            min_receive,
            max_fees,
        } => {
            let id = coordinator::start_take(home, j, m, lot, *max_give, *min_receive, *max_fees)
                .await?;
            let s: coordinator::Swap = j.get("swap", &id).await?.context("swap")?;
            Ok(Outcome {
                stdout: vec![serde_json::json!({"swap_id":s.id,"state":s.state}).to_string()],
                swap: Some(id),
                funding: None,
                stderr: vec![],
                written: None,
                failure: None,
            })
        }
        Command::Receive { mint, token } => receive_command(home, j, mint, &token.0).await,
        Command::Send {
            mint,
            amount,
            out,
            max_fees,
        } => send_command(home, j, mint, *amount, out, *max_fees).await,
        Command::Fund {
            mint,
            amount,
            id,
            quote,
        } => fund_command(home, j, mint, *amount, id, quote.as_deref()).await,
    }
}

/// A handed-off `send`: the same `send::send` the CLI runs, bounded like one recovery item so a
/// slow mint holds only this mint's wallet work. Dropping it is safe: the attempt is journaled
/// before its reservation and swap, and recovery restores the same outputs. On expiry the
/// journaled attempt (if any) is reported; serve's recovery finishes it and writes the file.
async fn send_command(
    home: &Path,
    j: &Journal,
    mint: &str,
    amount: u64,
    out: &str,
    max_fees: Option<u64>,
) -> Result<Outcome> {
    let asset = Asset::from_cli(mint)?;
    asset.fence()?;
    let mint = asset.mint_url.as_str();
    ensure!(
        Path::new(out).is_absolute(),
        "send --out must reach serve as an absolute path"
    );
    let budget = Duration::from_secs(crate::send::RECOVERY_ITEM_SECONDS);
    let call = crate::send::send(home, j, mint, amount, Path::new(out), max_fees);
    let r = match tokio::time::timeout(budget, call).await {
        Ok(r) => r?,
        Err(_) => {
            crate::backoff::record(
                mint,
                crate::backoff::Outcome::Failed(crate::backoff::Reason::Timeout, None),
            );
            let target = crate::send::target(Path::new(out))?;
            let target = target.to_str().context("--out path must be UTF-8")?;
            crate::send::by_out(j, target).await?.with_context(|| {
                format!(
                    "send did not finish within {}s and no attempt for --out {out} was found yet; \
                     re-run the same command (same --out): it resumes any attempt and never sends twice",
                    budget.as_secs()
                )
            })?
        }
    };
    // The file as serve sees it: the client checks its own --out is this same file.
    let written = (r.state == crate::send::SendState::Sent)
        .then(|| std::fs::symlink_metadata(crate::send::target(Path::new(out)).ok()?).ok())
        .flatten()
        .map(|m| (m.dev(), m.ino()));
    Ok(Outcome {
        stdout: vec![r.summary().to_string()],
        swap: None,
        funding: None,
        stderr: vec![],
        written,
        failure: crate::send::verdict(&r, false).err().map(|e| Failure {
            code: exit_code(&e),
            message: e.to_string(),
        }),
    })
}

/// A handed-off `fund`: create (or, for the same id, look up) the journaled quote, try issuance
/// once, and print the invoice. The client then follows the funding read-only while serve's
/// recovery keeps checking it: no invoice wait runs inline in serve's loop. Quote creation and
/// the one issuance check hold this mint's wallet unit (at most one item budget each); other
/// mints' work never waits on them.
async fn fund_command(
    home: &Path,
    j: &Journal,
    mint: &str,
    amount: u64,
    id: &str,
    quote: Option<&str>,
) -> Result<Outcome> {
    let asset = Asset::from_cli(mint)?;
    asset.fence()?;
    let mint = asset.mint_url.as_str();
    crate::transport::require_http(mint, "fund")?;
    ensure!(
        amount > 0 && amount <= crate::real_money::CAP,
        "funding cap exceeds 100,000 sats"
    );
    let budget = Duration::from_secs(crate::send::RECOVERY_ITEM_SECONDS);
    let mut f = match quote {
        Some(q) => j
            .all::<crate::money::Funding>("funding")
            .await?
            .into_iter()
            .find(|f| f.quote.as_deref() == Some(q) && f.mint == mint && f.amount == amount)
            .context("quote not journaled for this mint and amount")?,
        None => tokio::time::timeout(
            budget,
            crate::money::fund_with_id(home, j, mint, amount, id),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("funding quote not answered in time; re-run with --id {id}")
        })??,
    };
    // One issuance check now; later ones are recovery's (a lost answer only defers it).
    let _ = tokio::time::timeout(budget, crate::money::resume_fund(home, j, &mut f)).await;
    let line = serde_json::json!({"funding_id":f.id,"quote":f.quote,"invoice":f.invoice,
        "amount":f.amount,"note":"pay externally; tool never auto-pays"});
    Ok(Outcome {
        stdout: vec![line.to_string()],
        swap: None,
        funding: (!f.done).then(|| (f.id.clone(), f.mint.clone())),
        stderr: vec![],
        written: None,
        failure: None,
    })
}

/// How long one handed-off receive may hold serve's loop: one recovery item's budget. Lab
/// builds may shorten it for loopback mints only (`TRADE_LAB_SERVE_RECEIVE_SECONDS`).
fn receive_budget(mint: &str) -> Duration {
    #[cfg(feature = "lab")]
    if let Some(s) = std::env::var("TRADE_LAB_SERVE_RECEIVE_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        if crate::transport::loopback_only(mint) {
            return Duration::from_secs(s);
        }
    }
    let _ = mint;
    Duration::from_secs(receive::RECOVERY_ITEM_SECONDS)
}
async fn journaled(j: &Journal, mint: &str, token: &str) -> Result<Option<receive::Receipt>> {
    match receive_id(mint, token) {
        Some(id) => j.get("receive", &id).await,
        None => Ok(None),
    }
}
/// [`receive::receive`] bounded by `budget` (`mint` canonical). Dropping the call is safe: the
/// attempt is journaled before its POST and later passes only restore and replay that identical
/// swap. On expiry the mint did not answer in time: that is recorded as a timeout for `mint`,
/// exactly like any other unanswered mint call ([`crate::backoff`]), so the recovery tick that
/// follows does not re-enter the same mint at once. The journaled attempt is returned.
pub async fn receive_within(
    home: &Path,
    j: &Journal,
    mint: &str,
    token: &str,
    budget: Duration,
) -> Result<receive::Receipt> {
    match tokio::time::timeout(budget, receive::receive(home, j, mint, token)).await {
        Ok(r) => r,
        Err(_) => {
            crate::backoff::record(
                mint,
                crate::backoff::Outcome::Failed(crate::backoff::Reason::Timeout, None),
            );
            // Journaled: report its state; serve's recovery ticks finish it.
            match journaled(j, mint, token).await? {
                Some(r) => Ok(r),
                None => bail!(
                    "receive did not finish within {}s and nothing was journaled: nothing was \
                     sent to the mint for this token; re-run it",
                    budget.as_secs()
                ),
            }
        }
    }
}
/// A handed-off `receive`: the CLI's own path (same canonicalization, fence, journal and
/// replay protection), plus one stderr line saying what THIS request did for the home.
async fn receive_command(home: &Path, j: &Journal, mint: &str, token: &str) -> Result<Outcome> {
    let asset = Asset::from_cli(mint)?;
    asset.fence()?;
    let mint = asset.mint_url.as_str();
    let prior = journaled(j, mint, token).await?;
    // Bounded like one recovery item, so a slow mint cannot hold serve's loop (and its claims and
    // refunds) for long.
    let r = receive_within(home, j, mint, token, receive_budget(mint)).await?;
    let outcome = if r.settled() {
        if prior.as_ref().is_some_and(receive::Receipt::settled) {
            "already_received"
        } else {
            "credited"
        }
    } else if r.unresolved() {
        "in_doubt"
    } else {
        "not_credited"
    };
    let note = match outcome {
        "credited" => format!(
            "credited {} sats; repeating this token resumes this attempt and credits nothing more",
            r.net
        ),
        "already_received" => {
            "this token was already received into this home; nothing new was credited".into()
        }
        "in_doubt" => "not settled yet: the attempt is journaled and serve keeps recovering it. \
            Check `status`, or re-run this same receive: it resumes this attempt and never credits twice"
            .into(),
        _ => format!(
            "nothing was credited (state {})",
            serde_json::json!(r.state).as_str().unwrap_or("unknown")
        ),
    };
    Ok(Outcome {
        stdout: vec![r.summary().to_string()],
        swap: None,
        funding: None,
        stderr: vec![serde_json::json!({"receive":r.id,"outcome":outcome,"note":note}).to_string()],
        written: None,
        failure: receive::verdict(&r).err().map(|e| Failure {
            code: exit_code(&e),
            message: e.to_string(),
        }),
    })
}

/// Exit code relayed from serve. The message was already printed.
#[derive(Debug)]
pub struct Forwarded(pub u8);
impl std::fmt::Display for Forwarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handled by serve; exit {}", self.0)
    }
}
impl std::error::Error for Forwarded {}

/// Hand `request` to a running serve. `Ok(None)`: nothing is listening (no serve).
pub async fn forward(home: &Path, request: &Request) -> Result<Option<u8>> {
    let sp = socket_path(home)?;
    let mut stream = match UnixStream::connect(&sp.path).await {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e).context("connect to serve socket"),
    };
    // The token goes only to a serve running as this user: the listening peer AND the socket
    // file must both be ours. Comparing the peer with the socket's owner alone would trust a
    // socket another uid planted in the home.
    trusted_serve(
        stream.peer_cred()?.uid(),
        std::fs::symlink_metadata(home.join(SOCKET))?.uid(),
        rustix::process::geteuid().as_raw(),
    )?;
    // Sized before anything is written: an over-limit request never reaches serve.
    let mut line = serde_json::to_string(request)?;
    ensure!(
        line.len() <= MAX_REQUEST_BYTES,
        "request exceeds serve's {MAX_REQUEST_BYTES}-byte limit; nothing was sent to serve"
    );
    line.push('\n');
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .context("serve write timed out")??;
    eprintln!(
        "{}",
        serde_json::json!({"handed_to_serve":request.command.name()})
    );
    let mut swap: Option<String> = None;
    // A handed-off send succeeds only when serve's file is the client's own --out.
    let mut written_ok = false;
    let mut stream = BufReader::new(stream);
    loop {
        let next = tokio::time::timeout(
            CLIENT_IDLE,
            read_line_bounded(&mut stream, MAX_REPLY_LINE_BYTES),
        )
        .await;
        let line = match next {
            Ok(Ok(Some(line))) => line,
            other => {
                let why = match other {
                    Err(_) => "serve went silent".to_string(),
                    Ok(Err(e)) => e.to_string(),
                    _ => "serve closed the connection".to_string(),
                };
                match (&swap, &request.command) {
                    (Some(id), _) => eprintln!(
                        "{why}; swap {id} is journaled and stays with the home: keep serve running or run recover"
                    ),
                    (None, Command::Receive { mint, token }) => eprintln!(
                        "{why} before reporting a result; receive {} may have been applied. Check \
                         `status`, or re-run this same receive: it resumes that attempt and never \
                         credits twice",
                        receive_id(mint, &token.0).unwrap_or_else(|| "attempt".into())
                    ),
                    (None, Command::Send { out, .. }) => eprintln!(
                        "{why} before reporting a result; the send to {out} may have been applied. \
                         Re-run this same send (same --out): it resumes that attempt and never \
                         sends twice; or check `status`"
                    ),
                    (None, Command::Fund { id, .. }) => eprintln!(
                        "{why} before reporting a result; funding {id} may have been created. \
                         Re-run with --id {id}: it returns that same invoice and never creates a \
                         second one; or check `status`"
                    ),
                    (None, _) => eprintln!(
                        "{why} before reporting a result; the request may have been applied. Check status"
                    ),
                }
                return Ok(Some(3));
            }
        };
        match serde_json::from_str::<Reply>(&line).context("invalid serve reply")? {
            Reply::Stdout(s) => {
                if swap.is_none() && matches!(request.command, Command::Take { .. }) {
                    swap = serde_json::from_str::<serde_json::Value>(&s)
                        .ok()
                        .and_then(|v| v["swap_id"].as_str().map(str::to_owned));
                }
                println!("{s}")
            }
            Reply::Stderr(s) => eprintln!("{s}"),
            Reply::Keepalive => {}
            Reply::Written { dev, ino } => {
                if let Command::Send { out, .. } = &request.command {
                    written_ok = same_file(Path::new(out), dev, ino);
                }
            }
            Reply::Exit { code, error } => {
                if let Some(e) = error {
                    eprintln!("{e}");
                }
                if code == 0 && !written_ok {
                    if let Command::Send { out, .. } = &request.command {
                        eprintln!(
                            "serve reports the send complete but {out} is not the file serve wrote: \
                             serve and this client do not share a filesystem view (private /tmp, \
                             container or chroot). The token is journaled, not lost; run send from \
                             serve's view or stop serve and use send --reclaim"
                        );
                        return Ok(Some(3));
                    }
                }
                return Ok(Some(code));
            }
        }
    }
}

/// `path` (not followed) is the file with this (`st_dev`, `st_ino`).
fn same_file(path: &Path, dev: u64, ino: u64) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.dev() == dev && m.ino() == ino)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_file_must_be_the_clients_own_out() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        let m = std::fs::metadata(&a).unwrap();
        assert!(same_file(&a, m.dev(), m.ino()));
        // Same content elsewhere (serve's private /tmp) is not the same file.
        assert!(!same_file(&b, m.dev(), m.ino()));
        assert!(!same_file(&dir.path().join("missing"), m.dev(), m.ino()));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        assert!(
            !same_file(&link, m.dev(), m.ino()),
            "a symlink is not the file"
        );
    }
    #[test]
    fn request_rejects_unknown_fields_and_keeps_bounds() {
        let r = Request {
            v: PROTOCOL_V,
            relays: vec!["ws://127.0.0.1:1".into()],
            mint_relays: vec![],
            command: Command::Take {
                lot: "x".into(),
                max_give: 27,
                min_receive: 32,
                max_fees: 3,
            },
        };
        let text = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), r);
        let loose = text.replace("\"max_fees\":3", "\"max_fees\":3,\"max_fees_override\":99");
        assert!(serde_json::from_str::<Request>(&loose).is_err());
        let missing = text.replace(",\"max_fees\":3", "");
        assert!(
            serde_json::from_str::<Request>(&missing).is_err(),
            "SAFETY: no serve-side default may fill a missing bound"
        );
    }
    #[test]
    fn receive_request_round_trips_and_never_prints_its_token() {
        let token = format!("cashuB{}", "Zm9v".repeat(64));
        let r = Request {
            v: PROTOCOL_V,
            relays: vec!["ws://127.0.0.1:1".into()],
            mint_relays: vec![],
            command: Command::Receive {
                mint: "http://127.0.0.1:1".into(),
                token: TokenText(token.clone()),
            },
        };
        let text = serde_json::to_string(&r).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), r);
        assert!(
            !format!("{r:?}").contains(&token),
            "SAFETY: Debug prints the token"
        );
        let public = r.command.public().to_string();
        assert!(
            !public.contains(&token),
            "SAFETY: logged view has the token"
        );
        assert_eq!(
            public,
            r#"{"receive":{"mint":"http://127.0.0.1:1","receive":null}}"#
        );
        let loose = text.replace("\"token\":", "\"memo\":\"x\",\"token\":");
        assert!(serde_json::from_str::<Request>(&loose).is_err());
    }
    #[test]
    fn only_the_socket_owner_uid_is_allowed() {
        assert!(peer_allowed(1001, 1001).is_ok());
        assert!(
            peer_allowed(1002, 1001).is_err(),
            "SAFETY: another uid must be refused"
        );
        assert!(
            peer_allowed(0, 1001).is_err(),
            "SAFETY: root is not the owner"
        );
    }
    #[test]
    fn client_trusts_only_a_serve_of_its_own_uid() {
        assert!(trusted_serve(1001, 1001, 1001).is_ok());
        assert!(
            trusted_serve(1001, 1002, 1001).is_err(),
            "SAFETY: a socket file planted by another uid is refused even if its peer is ours"
        );
        assert!(
            trusted_serve(1002, 1002, 1001).is_err(),
            "SAFETY: another uid's serve (owning its own socket) is refused (#1140 F5)"
        );
        assert!(trusted_serve(0, 0, 1001).is_err(), "SAFETY: root's serve");
        assert!(trusted_serve(1002, 1001, 1001).is_err());
    }
    #[test]
    fn relay_refusals_follow_what_the_command_uses() {
        let market = vec!["ws://127.0.0.1:1".to_string()];
        let mint_relays = vec!["wss://m.example".to_string()];
        let npub = "nostr://npub10xlxvlhemja6c4dqv22uapctqupfhlxm9h8z3k2e72q4k9hcz7vqpkge6d";
        let req = |relays: &[&str], mints: &[&str], command: Command| Request {
            v: PROTOCOL_V,
            relays: relays.iter().map(|s| s.to_string()).collect(),
            mint_relays: mints.iter().map(|s| s.to_string()).collect(),
            command,
        };
        let receive = |mint: &str| Command::Receive {
            mint: mint.into(),
            token: TokenText("cashuBx".into()),
        };
        let take = || Command::Take {
            lot: "x".into(),
            max_give: 1,
            min_receive: 1,
            max_fees: 1,
        };
        let list = |give: &str| Command::List {
            give_mint: give.into(),
            give: 1,
            want_mint: "http://127.0.0.1:2".into(),
            want: 1,
            max_fees: 1,
        };
        let refused = |r: &Request| refusal(r, &market, &mint_relays);
        // A receive never uses the market: other (or default) --relay values are accepted.
        let r = req(
            &["wss://other.example"],
            &["wss://m.example"],
            receive("http://127.0.0.1:2"),
        );
        assert_eq!(refused(&r), None);
        // An HTTP receive does not use mint relays either.
        let r = req(
            &["wss://other.example"],
            &["wss://x.example"],
            receive("http://127.0.0.1:2"),
        );
        assert_eq!(refused(&r), None);
        // A nostr:// receive does: a different --mint-relay set is refused, never silently
        // replaced by serve's own (#1140 F4).
        let r = req(
            &["wss://other.example"],
            &["wss://x.example"],
            receive(npub),
        );
        assert!(refused(&r).unwrap().contains("--mint-relay"), "{r:?}");
        let r = req(&[], &["wss://M.EXAMPLE/"], receive(npub));
        assert_eq!(refused(&r), None, "spelling of the same relay set");
        // Market commands still compare --relay exactly.
        let r = req(&["wss://other.example"], &["wss://m.example"], take());
        assert!(refused(&r).unwrap().contains("--relay set"));
        let r = req(
            &["ws://127.0.0.1:1"],
            &["wss://m.example"],
            list("http://127.0.0.1:3"),
        );
        assert_eq!(refused(&r), None);
        // take/cancel may reach a lot's nostr:// mint: mint relays are compared.
        let r = req(&["ws://127.0.0.1:1"], &["wss://x.example"], take());
        assert!(refused(&r).unwrap().contains("--mint-relay"));
        let r = req(
            &["ws://127.0.0.1:1"],
            &["wss://x.example"],
            Command::Cancel { lot: "x".into() },
        );
        assert!(refused(&r).is_some());
        // list: only when one of its mints is nostr://.
        let r = req(
            &["ws://127.0.0.1:1"],
            &["wss://x.example"],
            list("http://127.0.0.1:3"),
        );
        assert_eq!(refused(&r), None);
        let r = req(&["ws://127.0.0.1:1"], &["wss://x.example"], list(npub));
        assert!(refused(&r).is_some());
        // Version mismatch names both versions.
        let mut r = req(&["ws://127.0.0.1:1"], &["wss://m.example"], take());
        r.v = 1;
        assert!(refused(&r).unwrap().contains("client 1"));
        // Redacted: a mistyped mint relay holding token text is not echoed.
        let leak = format!("cashuB{}", "o2FteBtodHRwOi8v".repeat(4));
        let r = req(&[], &[&leak], receive(npub));
        let e = refused(&r).unwrap();
        assert!(!e.contains(&leak) && e.contains("<redacted token>"), "{e}");
    }
    #[tokio::test]
    async fn bounded_lines() {
        let mut ok: &[u8] = b"abc\n";
        assert_eq!(read_line_bounded(&mut ok, 3).await.unwrap().unwrap(), "abc");
        let mut long: &[u8] = b"abcd\n";
        assert!(read_line_bounded(&mut long, 3).await.is_err());
        let mut open: &[u8] = b"abc";
        assert!(read_line_bounded(&mut open, 8).await.is_err());
        let mut empty: &[u8] = b"";
        assert!(read_line_bounded(&mut empty, 8).await.unwrap().is_none());
    }
}
