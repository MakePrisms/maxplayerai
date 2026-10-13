//! Local hand-off from a CLI invocation to a running `serve` over `<home>/serve.sock`.
//!
//! `serve` holds `owner.lock` and stays the only writer: a handed-off `list`, `take` or `cancel`
//! is queued to serve's own loop and executed there, between inbox messages and recovery ticks,
//! by the same coordinator functions (journal before effect) the CLI would have called, with
//! exactly the bounds the client sent. The client only relays output. A client that disconnects
//! never cancels a queued or running request; a take keeps being driven by serve.
//!
//! The socket is mode 0600 inside the 0700 home, and every peer must have the uid that owns the
//! socket (SO_PEERCRED). One request per connection; the request is at most
//! [`MAX_REQUEST_BYTES`]; reads and writes are bounded by [`IO_TIMEOUT`]; at most
//! [`MAX_CONNECTIONS`] connections are served at once.
use crate::{Asset, Leg, coordinator, journal::Journal, market::Market, observe};
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
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_REPLY_LINE_BYTES: usize = 64 * 1024;
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_CONNECTIONS: usize = 8;
/// Server keepalive cadence while a request runs or a take is streamed.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// The client gives up on a silent serve after this long (keepalives arrive every 15 s).
pub const CLIENT_IDLE: Duration = Duration::from_secs(90);
/// How long a client whose home is locked keeps looking for a serve socket (serve may be starting).
pub const CONNECT_WAIT: Duration = Duration::from_secs(5);
pub const PROTOCOL_V: u8 = 1;

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
}
impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::List { .. } => "list",
            Command::Take { .. } => "take",
            Command::Cancel { .. } => "cancel",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u8,
    /// The client's resolved relay set; serve refuses a request for a different set.
    pub relays: Vec<String>,
    pub command: Command,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Stdout(String),
    Stderr(String),
    Keepalive,
    Exit { code: u8, error: Option<String> },
}

/// A request queued to serve's loop.
pub struct Job {
    pub command: Command,
    reply: oneshot::Sender<std::result::Result<Outcome, Failure>>,
}
#[derive(Debug)]
pub struct Outcome {
    pub stdout: Vec<String>,
    pub swap: Option<String>,
}
#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub message: String,
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
    let owner = std::fs::symlink_metadata(&file)?.uid();
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
                        serde_json::json!({"serve_client":"closed","reason":e.to_string()})
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

async fn connection(
    mut stream: UnixStream,
    owner: u32,
    home: &Path,
    relays: &[String],
    tx: mpsc::Sender<Job>,
) -> Result<()> {
    let peer = stream.peer_cred()?;
    ensure!(peer.uid() == owner, "peer uid {} refused", peer.uid());
    // One request per connection: nothing after its line is read.
    let line = tokio::time::timeout(
        IO_TIMEOUT,
        read_line_bounded(&mut BufReader::new(&mut stream), MAX_REQUEST_BYTES),
    )
    .await
    .context("request read timed out")??
    .context("empty request")?;
    let request: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let error = format!("invalid serve request: {e}");
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
    if request.v != PROTOCOL_V || request.relays != relays {
        let error = if request.v != PROTOCOL_V {
            "serve protocol version mismatch".to_string()
        } else {
            format!(
                "serve uses relays {relays:?}; this command asked for {:?}. Use the same --relay set as serve",
                request.relays
            )
        };
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
    tx.send(Job {
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

/// Stream one swap's state from the journal (read-only) until it is terminal.
async fn stream_take(stream: &mut UnixStream, home: &Path, id: &str) -> Result<()> {
    let mut last = String::new();
    let mut warned = observe::Warned::default();
    let mut quiet = tokio::time::Instant::now();
    loop {
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

/// Execute one queued request inside serve's loop. Never panics the loop.
pub async fn execute(home: &Path, j: &Journal, m: &Market, job: Job) {
    let name = job.command.name();
    eprintln!(
        "{}",
        serde_json::json!({"serve_request":name,"bounds":job.command})
    );
    let result = run_command(home, j, m, &job.command)
        .await
        .map_err(|e| Failure {
            code: exit_code(&e),
            message: e.to_string(),
        });
    match &result {
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
            })
        }
        Command::Cancel { lot } => {
            coordinator::cancel(home, j, m, lot).await?;
            let l: coordinator::Listing = j.get("listing", lot).await?.context("listing")?;
            Ok(Outcome {
                stdout: vec![serde_json::json!({"lot_id":l.event.id,"status_event_ids":l.statuses.iter().map(|e|e.id.to_hex()).collect::<Vec<_>>(),"status":crate::lifecycle(&l.event,&l.statuses)?}).to_string()],
                swap: None,
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
            })
        }
    }
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
    let owner = std::fs::symlink_metadata(home.join(SOCKET))?.uid();
    let peer = stream.peer_cred()?;
    ensure!(
        peer.uid() == owner,
        "serve socket peer uid {} does not own the socket; refusing",
        peer.uid()
    );
    let mut line = serde_json::to_string(request)?;
    ensure!(line.len() <= MAX_REQUEST_BYTES, "request too large");
    line.push('\n');
    tokio::time::timeout(IO_TIMEOUT, stream.write_all(line.as_bytes()))
        .await
        .context("serve write timed out")??;
    eprintln!(
        "{}",
        serde_json::json!({"handed_to_serve":request.command.name()})
    );
    let mut swap: Option<String> = None;
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
                match &swap {
                    Some(id) => eprintln!(
                        "{why}; swap {id} is journaled and stays with the home: keep serve running or run recover"
                    ),
                    None => eprintln!(
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
            Reply::Exit { code, error } => {
                if let Some(e) = error {
                    eprintln!("{e}");
                }
                return Ok(Some(code));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_rejects_unknown_fields_and_keeps_bounds() {
        let r = Request {
            v: 1,
            relays: vec!["ws://127.0.0.1:1".into()],
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
