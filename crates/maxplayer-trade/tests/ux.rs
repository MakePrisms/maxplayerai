//! Agent-UX safety tests: lock-free discover, serve hand-off, unresponsive warning,
//! readable status (with a byte-for-byte `status --json` golden) and fee dry-runs.
//! Every SAFETY assertion comes first in its test.
mod support;
use maxplayer_trade::{coordinator, coordinator::Swap, market::Market, money};
use nostr_sdk::prelude::{Filter, Keys};
use std::{collections::BTreeSet, path::Path, process::Stdio, time::Duration};
use support::*;
use tokio::process::{Child, Command};

const BIN: &str = env!("CARGO_BIN_EXE_maxplayer-trade");

fn cmd(home: &Path, relay: &str, args: &[&str], envs: &[(&str, &str)]) -> Command {
    let mut c = Command::new(BIN);
    c.arg("--home")
        .arg(home)
        .args(["--relay", relay])
        .args(args);
    for (k, v) in envs {
        c.env(k, v);
    }
    c.stdin(Stdio::null()).kill_on_drop(true);
    c
}
async fn run(home: &Path, relay: &str, args: &[&str]) -> std::process::Output {
    tokio::time::timeout(
        Duration::from_secs(120),
        cmd(home, relay, args, &[]).output(),
    )
    .await
    .expect("command timed out")
    .unwrap()
}
fn text(o: &std::process::Output) -> (String, String) {
    (
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

type KvRow = (String, String, Vec<u8>, i64, i64);
/// Every journal row including its timestamps: any write shows up here.
fn kv_dump(home: &Path) -> Vec<KvRow> {
    let path = home.join("trade.sqlite");
    if !path.exists() {
        return vec![];
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut s = db
        .prepare("SELECT secondary_namespace, key, value, created_time, updated_time FROM kv_store ORDER BY 1, 2")
        .unwrap();
    s.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}
/// Every wallet proof row (state and reservation) for every wallet DB in the home.
fn wallet_dump(home: &Path) -> Vec<(String, Vec<u8>, String, Option<String>)> {
    let mut out = vec![];
    for f in files(home) {
        if f.ends_with(".sqlite") && f != "trade.sqlite" {
            let db = rusqlite::Connection::open_with_flags(
                home.join(&f),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut s = db
                .prepare("SELECT y, state, used_by_operation FROM proof ORDER BY y")
                .unwrap();
            out.extend(
                s.query_map([], |r| Ok((f.clone(), r.get(0)?, r.get(1)?, r.get(2)?)))
                    .unwrap()
                    .map(Result::unwrap),
            );
        }
    }
    out
}
fn files(home: &Path) -> BTreeSet<String> {
    std::fs::read_dir(home)
        .map(|d| {
            d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| !n.ends_with("-wal") && !n.ends_with("-shm"))
                .collect()
        })
        .unwrap_or_default()
}
async fn relay_events(url: &str) -> BTreeSet<String> {
    let m = Market::connect(Keys::generate(), &[url.to_string()])
        .await
        .unwrap();
    let (events, partial) = m.query(Filter::new().limit(4000)).await.unwrap();
    assert_eq!(partial, 0);
    events.iter().map(|e| e.id.to_hex()).collect()
}
struct Snapshot {
    kv: Vec<KvRow>,
    wallet: Vec<(String, Vec<u8>, String, Option<String>)>,
    files: BTreeSet<String>,
}
fn snapshot(home: &Path) -> Snapshot {
    Snapshot {
        kv: kv_dump(home),
        wallet: wallet_dump(home),
        files: files(home),
    }
}
fn assert_unchanged(home: &Path, before: &Snapshot, what: &str) {
    let after = snapshot(home);
    assert!(after.kv == before.kv, "SAFETY: {what} changed the journal");
    assert!(
        after.wallet == before.wallet,
        "SAFETY: {what} changed wallet proofs/reservations"
    );
    assert_eq!(after.files, before.files, "SAFETY: {what} created files");
}

async fn start_serve(home: &Path, relay: &str, log: &Path, envs: &[(&str, &str)]) -> Child {
    let mut c = cmd(home, relay, &["serve"], envs);
    c.stdout(Stdio::null())
        .stderr(std::fs::File::create(log).unwrap());
    let child = c.spawn().unwrap();
    let start = std::time::Instant::now();
    while !std::fs::read_to_string(log)
        .unwrap_or_default()
        .contains("{\"serve_socket\":\"")
    {
        assert!(start.elapsed() < Duration::from_secs(30), "serve socket");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    child
}
/// Step the in-process maker (and its recovery ticks) until `child` exits.
async fn drive_maker(f: &mut Fixture, child: &mut Child, limit: u64) -> std::process::ExitStatus {
    let start = std::time::Instant::now();
    let mut next = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        f.step(true).await;
        if let Some(s) = child.try_wait().unwrap() {
            return s;
        }
        if std::time::Instant::now() >= next {
            let _ = coordinator::recover(&f.maker, &f.jm, &f.mm).await;
            next = std::time::Instant::now() + Duration::from_secs(3);
        }
        assert!(start.elapsed().as_secs() < limit, "client did not finish");
    }
}

/// (debit, total fees) the taker's dry run reports for `lot` with generous bounds.
async fn dry(f: &Fixture, lot: &str) -> (u64, u64) {
    let out = run(
        &f.taker,
        &f.relay_url,
        &[
            "take",
            lot,
            "--max-give",
            "1000",
            "--min-receive",
            "1",
            "--max-fees",
            "100",
            "--dry-run",
        ],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out).1);
    let v: serde_json::Value = serde_json::from_str(&text(&out).0).unwrap();
    (
        v["max_total_debit"].as_u64().unwrap(),
        v["fees_total"].as_u64().unwrap(),
    )
}
/// SAFETY: discover takes no lock, runs no recovery and writes nothing, even when the home
/// holds a swap that recovery WOULD change (a `requested` swap past its 60 s window), both
/// while another process owns `owner.lock` (serve mid-swap) and when nothing does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_is_lock_free_and_never_touches_the_journal() {
    let f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    let mut s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    s.created = coordinator::now() - 120;
    f.jt.put("swap", &id, &s).await.unwrap();
    let relay_before = relay_events(&f.relay_url).await;
    std::fs::File::create(f.taker.join("owner.lock")).unwrap();
    let before = snapshot(&f.taker);
    {
        use fs2::FileExt;
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(f.taker.join("owner.lock"))
            .unwrap();
        held.lock_exclusive().unwrap();
        let before = snapshot(&f.taker);
        let out = run(&f.taker, &f.relay_url, &["discover"]).await;
        let (stdout, stderr) = text(&out);
        assert!(
            out.status.success(),
            "SAFETY: serve must not block discover: {stderr}"
        );
        assert_unchanged(&f.taker, &before, "discover while the home is owned");
        let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let l = &v["listings"][0];
        assert_eq!(l["lot_id"], lot);
        assert_eq!(l["expected_fees"]["give"]["gross"], 32);
        assert_eq!(l["expected_fees"]["want"]["claim_fee"], 0);
    }
    let out = run(&f.taker, &f.relay_url, &["discover"]).await;
    assert!(out.status.success());
    assert_unchanged(&f.taker, &before, "discover on an unowned home");
    assert_eq!(
        f.state(false, &id).await.as_deref(),
        Some("requested"),
        "SAFETY: discover ran no recovery"
    );
    assert_eq!(
        relay_events(&f.relay_url).await,
        relay_before,
        "SAFETY: discover published"
    );
    // A fresh home is not created, keyed or journaled by discover.
    let fresh = f.root.path().join("fresh");
    let out = run(&fresh, &f.relay_url, &["discover"]).await;
    assert!(out.status.success());
    assert!(!fresh.exists(), "SAFETY: discover created a home");
}

/// Mint keysets unreachable or hanging: discover still answers, fees marked unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discover_marks_unreachable_mint_fees_unknown_without_blocking() {
    let f = Fixture::new(100).await;
    let lot = f.list(false).await;
    f.b.faults
        .hang_keysets
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let start = std::time::Instant::now();
    let out = run(&f.root.path().join("x"), &f.relay_url, &["discover"]).await;
    assert!(out.status.success(), "{}", text(&out).1);
    assert!(
        start.elapsed() < Duration::from_secs(30),
        "discover blocked on a mint"
    );
    let v: serde_json::Value = serde_json::from_str(&text(&out).0).unwrap();
    let l = &v["listings"][0];
    assert_eq!(l["lot_id"], lot);
    assert_eq!(l["expected_fees"]["want"], "unknown");
    // 32 net at 100 ppk: gross 33, claim fee 1 (sender-funds-net).
    assert_eq!(l["expected_fees"]["give"]["gross"], 33);
    assert_eq!(l["expected_fees"]["give"]["claim_fee"], 1);
}

/// SAFETY: a take handed to serve uses EXACTLY the client's bounds. Each looser-than-needed
/// value is refused by serve (with serve's own default 16-sat fee cap never substituted), and
/// the accepted take journals the client's exact numbers. serve then drives it to settlement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn take_via_serve_enforces_client_bounds_exactly_and_settles() {
    let mut f = Fixture::new(100).await;
    let lot = f.list(false).await;
    let (debit, fees) = dry(&f, &lot).await;
    assert!(fees >= 2, "fee-bearing fixture");
    let (d, d1, fz, fz1) = (
        debit.to_string(),
        (debit - 1).to_string(),
        fees.to_string(),
        (fees - 1).to_string(),
    );
    let log = f.root.path().join("serve.log");
    let mut serve = start_serve(&f.taker, &f.relay_url, &log, &[]).await;
    // Exactly one bound one sat too tight each time: serve must refuse, never loosen it
    // (serve's own default of 16 would admit every one of these).
    for (bounds, why) in [
        ([d1.as_str(), "32", "16"], "maximum give cap"),
        ([d.as_str(), "33", "16"], "minimum receive cap"),
        ([d.as_str(), "32", fz1.as_str()], "fee cap exceeded"),
    ] {
        let out = run(
            &f.taker,
            &f.relay_url,
            &[
                "take",
                &lot,
                "--max-give",
                bounds[0],
                "--min-receive",
                bounds[1],
                "--max-fees",
                bounds[2],
            ],
        )
        .await;
        let (_, stderr) = text(&out);
        assert!(stderr.contains("handed_to_serve"), "{stderr}");
        assert_eq!(
            out.status.code(),
            Some(1),
            "SAFETY: {why} must refuse: {stderr}"
        );
        assert!(stderr.contains(why), "{stderr}");
        assert!(
            f.jt.all::<Swap>("swap").await.unwrap().is_empty(),
            "SAFETY: refused take journaled a swap"
        );
    }
    assert!(
        serve.try_wait().unwrap().is_none(),
        "serve survives refusals"
    );
    // While serve owns the home mid-take, discover still works.
    let mut client = cmd(
        &f.taker,
        &f.relay_url,
        &[
            "take",
            &lot,
            "--max-give",
            &d,
            "--min-receive",
            "32",
            "--max-fees",
            &fz,
        ],
        &[],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let mut id = None;
    for _ in 0..200 {
        f.step(true).await;
        if let Some(s) = f.jt.all::<Swap>("swap").await.unwrap().pop() {
            id = Some(s.id);
            break;
        }
    }
    let id = id.expect("serve journaled the take");
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    assert_eq!(
        (
            s.request.max_give,
            s.request.min_receive,
            s.request.max_fees,
            s.max_fees
        ),
        (debit, 32, fees, fees),
        "SAFETY: serve journaled bounds other than the client's"
    );
    let d = run(&f.taker, &f.relay_url, &["discover"]).await;
    assert!(d.status.success(), "discover during serve: {}", text(&d).1);
    let status = drive_maker(&mut f, &mut client, 150).await;
    let out = client.wait_with_output().await.unwrap();
    let (stdout, stderr) = text(&out);
    assert!(status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains(&format!("serve is watching swap {id}")),
        "{stderr}"
    );
    // Same lines (serde_json orders keys) as a CLI-driven take prints.
    assert!(
        stdout.contains(&serde_json::json!({"swap_id":id,"state":"requested"}).to_string()),
        "{stdout}"
    );
    assert!(
        stdout.contains(&serde_json::json!({"swap_id":id,"state":"complete"}).to_string()),
        "{stdout}"
    );
    assert_eq!(f.state(false, &id).await.as_deref(), Some("complete"));
    serve.kill().await.unwrap();
    let log = std::fs::read_to_string(&log).unwrap();
    assert!(
        log.contains(&format!("\"max_give\":{debit}"))
            && log.contains(&format!("\"max_fees\":{fees}")),
        "{log}"
    );
}

/// A client that disconnects right after handing off never half-applies the request:
/// serve completes the journaled take on its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnected_client_leaves_take_with_serve() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let log = f.root.path().join("serve.log");
    let mut serve = start_serve(&f.taker, &f.relay_url, &log, &[]).await;
    let mut client = cmd(
        &f.taker,
        &f.relay_url,
        &["take", &lot, "--max-give", "40", "--min-receive", "32"],
        &[],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let mut id = None;
    for _ in 0..200 {
        f.step(true).await;
        if let Some(s) = f.jt.all::<Swap>("swap").await.unwrap().pop() {
            id = Some(s.id);
            break;
        }
    }
    client.kill().await.unwrap();
    let id = id.unwrap();
    let start = std::time::Instant::now();
    let mut next = std::time::Instant::now();
    while f.state(false, &id).await.as_deref() != Some("complete") {
        f.step(true).await;
        if std::time::Instant::now() >= next {
            let _ = coordinator::recover(&f.maker, &f.jm, &f.mm).await;
            next = std::time::Instant::now() + Duration::from_secs(3);
        }
        assert!(
            start.elapsed() < Duration::from_secs(150),
            "serve did not settle"
        );
    }
    assert_eq!(balance(&f.taker, &f.a.url).await, 32);
    serve.kill().await.unwrap();
}

/// A dead serve's socket file never blocks the CLI; a new serve replaces it; a non-socket
/// file is never removed; an oversized or malformed request is refused and serve stays up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_socket_and_bounded_protocol() {
    let f = Fixture::new(0).await;
    let lot = f.list(false).await;
    // Stale: a bound-then-dropped listener leaves a socket file nobody serves.
    drop(std::os::unix::net::UnixListener::bind(f.maker.join("serve.sock")).unwrap());
    assert!(f.maker.join("serve.sock").exists());
    let out = run(&f.maker, &f.relay_url, &["cancel", &lot]).await;
    let (stdout, stderr) = text(&out);
    assert!(
        out.status.success(),
        "stale socket blocked cancel: {stderr}"
    );
    assert!(!stderr.contains("handed_to_serve"));
    assert!(stdout.contains("cancelled"), "{stdout}");
    let log = f.root.path().join("serve.log");
    let mut serve = start_serve(&f.maker, &f.relay_url, &log, &[]).await;
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(f.maker.join("serve.sock"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "SAFETY: socket mode");
    // Oversized request: refused, connection closed, serve alive and still serving.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::UnixStream::connect(f.maker.join("serve.sock"))
            .await
            .unwrap();
        let _ = s.write_all(&vec![b'x'; 64 * 1024]).await;
        let mut buf = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(15), s.read_to_end(&mut buf)).await;
        let mut s = tokio::net::UnixStream::connect(f.maker.join("serve.sock"))
            .await
            .unwrap();
        s.write_all(
            b"{\"v\":1,\"relays\":[],\"command\":{\"cancel\":{\"lot\":\"x\",\"extra\":1}}}\n",
        )
        .await
        .unwrap();
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(15), s.read_to_string(&mut reply))
            .await
            .unwrap()
            .unwrap();
        assert!(reply.contains("invalid serve request"), "{reply}");
    }
    // A request for a different relay set is refused, not silently re-routed.
    let other = run(&f.maker, "ws://127.0.0.1:9", &["cancel", &lot]).await;
    assert_eq!(other.status.code(), Some(1));
    assert!(
        text(&other).1.contains("same --relay set"),
        "{}",
        text(&other).1
    );
    let out = run(&f.maker, &f.relay_url, &["cancel", &lot]).await;
    let (_, stderr) = text(&out);
    assert!(stderr.contains("handed_to_serve"), "{stderr}");
    assert_eq!(
        out.status.code(),
        Some(1),
        "terminal lot: same exit as the CLI"
    );
    assert!(stderr.contains("lot is terminal"), "{stderr}");
    assert!(serve.try_wait().unwrap().is_none());
    serve.kill().await.unwrap();
    serve.wait().await.unwrap();
    // A non-socket file at the socket path is never removed.
    std::fs::remove_file(f.maker.join("serve.sock")).unwrap();
    std::fs::write(f.maker.join("serve.sock"), b"not a socket").unwrap();
    let out = run(&f.maker, &f.relay_url, &["serve"]).await;
    assert!(!out.status.success());
    assert!(text(&out).1.contains("not a socket"));
    assert_eq!(
        std::fs::read(f.maker.join("serve.sock")).unwrap(),
        b"not a socket"
    );
}

#[cfg(feature = "lab")]
fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
/// SAFETY: two clients and a starting serve race for one home. Writer tenures recorded by
/// the lab trace never overlap, every client either succeeded or was refused before doing
/// anything, and the journaled listings reserve disjoint inputs.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clients_and_serve_never_make_two_writers() {
    let f = Fixture::new(0).await;
    for _ in 0..3 {
        fund(&f.maker, &f.a.url, 64).await;
    }
    let trace = f.root.path().join("writers.trace");
    let trace_s = trace.to_str().unwrap().to_string();
    let env = [("TRADE_LAB_WRITER_TRACE", trace_s.as_str())];
    let list = [
        "list",
        "--give-mint",
        &f.a.url,
        "--give",
        "16",
        "--want-mint",
        &f.b.url,
        "--want",
        "8",
    ];
    let mut ok = 0;
    let mut killed = std::collections::BTreeMap::new();
    for round in 0..3 {
        let log = f.root.path().join(format!("serve{round}.log"));
        let mut serve = cmd(&f.maker, &f.relay_url, &["serve"], &env);
        serve
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        let mut serve = serve.spawn().unwrap();
        let c1 = cmd(&f.maker, &f.relay_url, &list, &env).output();
        let c2 = cmd(&f.maker, &f.relay_url, &list, &env).output();
        let (o1, o2) = tokio::join!(c1, c2);
        for o in [o1.unwrap(), o2.unwrap()] {
            let (_, stderr) = text(&o);
            if o.status.success() {
                ok += 1;
            } else {
                assert!(stderr.contains("home is already in use"), "{stderr}");
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let pid = serve.id().unwrap().to_string();
        if let Some(s) = serve.try_wait().unwrap() {
            assert!(!s.success());
            assert!(
                std::fs::read_to_string(&log)
                    .unwrap()
                    .contains("home is already in use")
            );
        } else {
            serve.kill().await.unwrap();
            serve.wait().await.unwrap();
            // SIGKILL logs no release; the kernel dropped the lock no later than now.
            killed.insert(pid, now_ns());
        }
    }
    // Tenures: (pid, acquire, release-or-end). A killed serve never logs release.
    let raw = std::fs::read_to_string(&trace).unwrap();
    let mut open: std::collections::BTreeMap<String, u128> = Default::default();
    let mut spans = vec![];
    for line in raw.lines() {
        let p: Vec<_> = line.split(' ').collect();
        let t: u128 = p[2].parse().unwrap();
        if p[0] == "acquire" {
            open.insert(p[1].into(), t);
        } else {
            spans.push((open.remove(p[1]).unwrap(), t));
        }
    }
    for (pid, start) in open {
        spans.push((
            start,
            *killed
                .get(&pid)
                .expect("unreleased tenure of a live process"),
        ));
    }
    spans.sort();
    for w in spans.windows(2) {
        assert!(
            w[1].0 >= w[0].1,
            "SAFETY: two writers: {:?} overlaps {:?}\n{raw}",
            w[0],
            w[1]
        );
    }
    let listings = f.jm.all::<coordinator::Listing>("listing").await.unwrap();
    // One listing from Fixture::list is absent here (none was made); every success journaled one.
    assert_eq!(
        listings.len(),
        ok,
        "SAFETY: success count equals journaled listings"
    );
    let mut ys = BTreeSet::new();
    for l in &listings {
        for p in &l.plan.inputs {
            assert!(
                ys.insert(p.secret.to_string()),
                "SAFETY: one input reserved twice"
            );
        }
    }
    assert!(ok >= 3, "races produced too few successful lists ({ok})");
}

/// While serve lives, its lock blocks every CLI writer: they hand off instead of acquiring.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_serve_owns_every_write() {
    let f = Fixture::new(0).await;
    fund(&f.maker, &f.a.url, 64).await;
    let trace = f.root.path().join("writers.trace");
    let trace_s = trace.to_str().unwrap().to_string();
    let env = [("TRADE_LAB_WRITER_TRACE", trace_s.as_str())];
    let log = f.root.path().join("serve.log");
    let mut serve = start_serve(&f.maker, &f.relay_url, &log, &env).await;
    let list = [
        "list",
        "--give-mint",
        &f.a.url,
        "--give",
        "16",
        "--want-mint",
        &f.b.url,
        "--want",
        "8",
    ];
    let (o1, o2) = tokio::join!(
        cmd(&f.maker, &f.relay_url, &list, &env).output(),
        cmd(&f.maker, &f.relay_url, &list, &env).output()
    );
    for o in [o1.unwrap(), o2.unwrap()] {
        let (stdout, stderr) = text(&o);
        assert!(o.status.success(), "{stderr}");
        assert!(stderr.contains("handed_to_serve"), "{stderr}");
        assert!(stdout.contains("\"status\":\"available\""), "{stdout}");
    }
    let raw = std::fs::read_to_string(&trace).unwrap();
    assert_eq!(
        raw.lines().filter(|l| l.starts_with("acquire")).count(),
        1,
        "SAFETY: only serve may acquire while it runs:\n{raw}"
    );
    serve.kill().await.unwrap();
}

/// SAFETY: `--dry-run` has zero side effects (journal, reservations, wallet DB, relay,
/// maker inbox, mint swaps, files), and its numbers equal the real plan built afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_has_zero_side_effects_and_matches_real_plan() {
    let mut f = Fixture::new(100).await;
    let lot = f.list(false).await;
    let relay_before = relay_events(&f.relay_url).await;
    let swaps = (
        f.a.faults.swaps.load(std::sync::atomic::Ordering::SeqCst),
        f.b.faults.swaps.load(std::sync::atomic::Ordering::SeqCst),
    );
    let (taker0, maker0) = (snapshot(&f.taker), snapshot(&f.maker));
    let take = run(
        &f.taker,
        &f.relay_url,
        &[
            "take",
            &lot,
            "--max-give",
            "1000",
            "--min-receive",
            "32",
            "--max-fees",
            "100",
            "--dry-run",
        ],
    )
    .await;
    let list = run(
        &f.maker,
        &f.relay_url,
        &[
            "list",
            "--give-mint",
            &f.a.url,
            "--give",
            "32",
            "--want-mint",
            &f.b.url,
            "--want",
            "24",
            "--max-fees",
            "16",
            "--dry-run",
        ],
    )
    .await;
    assert_unchanged(&f.taker, &taker0, "take --dry-run");
    assert_unchanged(&f.maker, &maker0, "list --dry-run");
    assert_eq!(
        relay_events(&f.relay_url).await,
        relay_before,
        "SAFETY: dry-run published"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(500), f.mm.inbox.recv())
            .await
            .is_err(),
        "SAFETY: dry-run sent the maker a message"
    );
    assert_eq!(
        (
            f.a.faults.swaps.load(std::sync::atomic::Ordering::SeqCst),
            f.b.faults.swaps.load(std::sync::atomic::Ordering::SeqCst)
        ),
        swaps,
        "SAFETY: dry-run swapped at a mint"
    );
    assert!(take.status.success(), "{}", text(&take).1);
    assert!(list.status.success(), "{}", text(&list).1);
    let t: serde_json::Value = serde_json::from_str(&text(&take).0).unwrap();
    assert_eq!(t["you_receive"]["leg"]["claim_fee"], 1);
    assert_eq!(t["you_receive"]["leg"]["gross"], 33);
    let l: serde_json::Value = serde_json::from_str(&text(&list).0).unwrap();
    assert_eq!(l["you_receive"]["leg"]["claim_fee"], 1);
    // The real paths build the same plans.
    let id = f.start(&lot).await;
    let s = f.jt.get::<Swap>("swap", &id).await.unwrap().unwrap();
    let g = &t["you_give"];
    assert_eq!(
        (
            g["gross"].as_u64(),
            g["lock_fee"].as_u64(),
            g["claim_fee"].as_u64(),
            t["max_total_debit"].as_u64()
        ),
        (
            Some(s.plan.gross),
            Some(s.plan.lock_fee),
            Some(s.plan.claim_fee),
            Some(s.plan.debit)
        )
    );
    // The real list right after the dry run, against the same (unchanged) wallet.
    let real = coordinator::list(
        &f.maker,
        &f.jm,
        &f.mm,
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.a.url).unwrap(),
            net: 32,
        },
        maxplayer_trade::Leg {
            asset: maxplayer_trade::Asset::new(&f.b.url).unwrap(),
            net: 24,
        },
        16,
    )
    .await
    .unwrap();
    let listing: coordinator::Listing = f.jm.get("listing", &real).await.unwrap().unwrap();
    let g = &l["you_give"];
    assert_eq!(
        (
            g["gross"].as_u64(),
            g["lock_fee"].as_u64(),
            l["max_total_debit"].as_u64()
        ),
        (
            Some(listing.plan.gross),
            Some(listing.plan.lock_fee),
            Some(listing.plan.debit)
        )
    );
}

/// A refused or unsolvable dry-run says why, exits 1, and still changes nothing; a dry-run
/// on a fresh home creates no home, key or wallet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_refusals_and_unsolvable_amounts_are_reported() {
    let f = Fixture::new(100).await;
    let lot = f.list(false).await;
    let (debit, _) = dry(&f, &lot).await;
    let before = snapshot(&f.taker);
    let tight = (debit - 1).to_string();
    let out = run(
        &f.taker,
        &f.relay_url,
        &[
            "take",
            &lot,
            "--max-give",
            &tight,
            "--min-receive",
            "32",
            "--dry-run",
        ],
    )
    .await;
    assert_unchanged(&f.taker, &before, "refused take --dry-run");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).1.contains(&format!(
            "maximum give cap: debit {debit} > --max-give {tight}"
        )),
        "{}",
        text(&out).1
    );
    let fresh = f.root.path().join("fresh");
    let out = run(
        &fresh,
        &f.relay_url,
        &[
            "take",
            &lot,
            "--max-give",
            "1000",
            "--min-receive",
            "32",
            "--dry-run",
        ],
    )
    .await;
    assert!(!fresh.exists(), "SAFETY: dry-run created a home");
    assert!(text(&out).1.contains("insufficient unreserved balance"));
    // 1000 ppk: 2 net sats has no fee-inclusive split.
    let m = MintFixture::start(1000).await;
    let out = run(
        &fresh,
        &f.relay_url,
        &[
            "list",
            "--give-mint",
            &m.url,
            "--give",
            "2",
            "--want-mint",
            &f.b.url,
            "--want",
            "8",
            "--dry-run",
        ],
    )
    .await;
    assert!(!fresh.exists(), "SAFETY: dry-run created a home");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out)
            .1
            .contains("cannot solve bounded fee-inclusive split"),
        "{}",
        text(&out).1
    );
}

/// `status` before this change, verbatim from 7d8cd74 `wallet::read_status` — the golden.
fn status_golden(home: &Path) -> String {
    let path = home.join("trade.sqlite");
    if !path.exists() {
        return format!("{}\n", serde_json::json!({"records":[]}));
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut stmt = db.prepare("SELECT secondary_namespace, value FROM kv_store WHERE primary_namespace='trade-v1' AND secondary_namespace IN ('swap','funding','withdrawal','receive')").unwrap();
    let mut records = vec![];
    for row in stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
    {
        let (kind, bytes) = row.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let public = if kind == "withdrawal" {
            serde_json::from_slice::<money::Withdrawal>(&bytes)
                .unwrap()
                .summary()
        } else {
            serde_json::json!({"kind":kind,"id":v["id"],"state":v["state"],"done":v["done"],"expired_unpaid":v["expired_unpaid"]})
        };
        records.push(public);
    }
    format!("{}\n", serde_json::json!({"records":records}))
}

/// SAFETY: `status --json` is byte-for-byte the old `status`; the default summary is
/// readable, lists unresolved work first, explains exit codes and prints no secrets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_json_matches_golden_and_summary_is_readable_and_secret_free() {
    let mut f = Fixture::new(0).await;
    let empty = f.root.path().join("empty");
    let out = std::process::Command::new(BIN)
        .args(["--home", empty.to_str().unwrap(), "status", "--json"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        status_golden(&empty)
    );
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.pump(&id, "complete").await;
    let _ = money::fund(&f.taker, &f.jt, &f.b.url, 10).await.unwrap();
    for home in [&f.taker, &f.maker] {
        let json = std::process::Command::new(BIN)
            .args(["--home", home.to_str().unwrap(), "status", "--json"])
            .output()
            .unwrap();
        assert!(json.status.success());
        assert_eq!(
            String::from_utf8(json.stdout.clone()).unwrap(),
            status_golden(home),
            "SAFETY: status --json drifted from the golden"
        );
        let human = std::process::Command::new(BIN)
            .args(["--home", home.to_str().unwrap(), "status"])
            .output()
            .unwrap();
        assert!(human.status.success());
        let h = String::from_utf8(human.stdout).unwrap();
        for j in [&f.jt, &f.jm] {
            for s in j.all::<Swap>("swap").await.unwrap() {
                assert!(!h.contains(&s.key), "SAFETY: key printed");
                if let Some(p) = &s.preimage {
                    assert!(!h.contains(p.as_str()), "SAFETY: preimage printed");
                }
                for p in s.outgoing.iter().chain(&s.incoming) {
                    assert!(!h.contains(&p.secret.to_string()), "SAFETY: proof printed");
                }
            }
        }
        assert!(h.contains(&format!("swap {id} complete")), "{h}");
        assert!(h.contains("Exit codes:"), "{h}");
        if home == &f.taker {
            assert!(
                h.contains(&format!("+32 {} / -24 {}; fees 0", f.a.url, f.b.url)),
                "{h}"
            );
            assert!(h.starts_with("UNRESOLVED (1):\n  funding "), "{h}");
            let invoice = f.jt.all::<money::Funding>("funding").await.unwrap()[0]
                .invoice
                .clone()
                .unwrap();
            assert!(!h.contains(&invoice), "SAFETY: invoice printed");
        } else {
            assert!(
                h.contains(&format!("lot {lot} sold; swap {id} complete (maker)")),
                "{h}"
            );
            assert!(
                h.contains(&format!("+24 {} / -32 {}; fees 0", f.b.url, f.a.url)),
                "{h}"
            );
            assert!(h.contains("UNRESOLVED: none"), "{h}");
        }
    }
}

/// counterparty_unresponsive is derived display state: it appears in status, in a
/// serve-handled take's live output and in serve's log, and changes no swap state.
#[cfg(feature = "lab")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counterparty_unresponsive_warning_is_display_only() {
    let mut f = Fixture::new(0).await;
    let lot = f.list(false).await;
    let id = f.start(&lot).await;
    f.step(true).await;
    f.step(false).await;
    assert_eq!(f.state(false, &id).await.as_deref(), Some("first_locked"));
    let before = kv_dump(&f.taker);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let status = |envs: &'static [(&'static str, &'static str)], home: std::path::PathBuf| {
        let mut c = std::process::Command::new(BIN);
        c.args(["--home", home.to_str().unwrap(), "status"]);
        for (k, v) in envs {
            c.env(k, v);
        }
        String::from_utf8(c.output().unwrap().stdout).unwrap()
    };
    let quiet = status(&[], f.taker.clone());
    assert!(
        !quiet.contains("counterparty_unresponsive"),
        "default threshold is 180 s"
    );
    let warned = status(&[("TRADE_LAB_UNRESPONSIVE_SECONDS", "1")], f.taker.clone());
    assert!(
        warned.starts_with(&format!(
            "WARNING counterparty_unresponsive: swap {id} (taker first_locked) waiting for maker_second_lock for "
        )),
        "{warned}"
    );
    assert!(warned.contains("; refund available at "), "{warned}");
    assert!(kv_dump(&f.taker) == before, "SAFETY: status wrote");
    assert_eq!(f.state(false, &id).await.as_deref(), Some("first_locked"));
    // Live: a serve-handled take whose maker never answers.
    let g = Fixture::new(0).await;
    let lot = g.list(false).await;
    let log = g.root.path().join("serve.log");
    let env = [("TRADE_LAB_UNRESPONSIVE_SECONDS", "1")];
    let mut serve = start_serve(&g.taker, &g.relay_url, &log, &env).await;
    let mut client = cmd(
        &g.taker,
        &g.relay_url,
        &["take", &lot, "--max-give", "40", "--min-receive", "32"],
        &env,
    )
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(client.stderr.take().unwrap()).lines();
    let found = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(l) = lines.next_line().await.unwrap() {
            if l.contains("\"warning\":\"counterparty_unresponsive\"") {
                return l;
            }
        }
        panic!("client ended without a warning")
    })
    .await
    .expect("no live warning");
    assert!(found.contains("\"waiting_for\":\"maker_quote\""), "{found}");
    assert!(found.contains("\"elapsed_seconds\""), "{found}");
    client.kill().await.unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        std::fs::read_to_string(&log)
            .unwrap()
            .contains("counterparty_unresponsive"),
        "serve log"
    );
    let s = g.jt.all::<Swap>("swap").await.unwrap();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].state, "requested", "SAFETY: warning changed state");
    serve.kill().await.unwrap();
}
