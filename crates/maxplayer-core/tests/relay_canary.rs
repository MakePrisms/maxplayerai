//! Relay canary: a manual test that a person runs against a live relay-git server.
//!
//! The test answers two questions with evidence:
//!
//! - Requirement A: does the relay enforce the ref scope of a NIP-98 push token? A token
//!   carries one `["ref", "<refname>"]` tag. The relay must refuse a push to any other ref.
//! - Requirement B: does the relay accept a scoped token that is older than 60 s, when the
//!   token carries a NIP-40 `["expiration", "<unix>"]` tag in the future?
//!
//! The test is `#[ignore]`. It reads two environment variables:
//!
//! - `MAXPLAYER_CANARY_KEY_FILE`: a file that holds a nostr secret key (64 hex chars).
//! - `MAXPLAYER_CANARY_REMOTE`: the https URL of a relay-git repository (the repo root).
//!
//! When one variable is not set, the test prints one skip line and passes.
//!
//! Run:
//!
//! ```text
//! MAXPLAYER_CANARY_KEY_FILE=/path/to/key \
//! MAXPLAYER_CANARY_REMOTE=https://<relay>/git/<owner>/<repo> \
//!   cargo test -p maxplayer-core --features git-delivery --test relay_canary -- --ignored --nocapture
//! ```
//!
//! ## Output
//!
//! Each observation prints one line that starts with `CANARY `. The last line is
//! `CANARY VERDICT A=<enforced|not-enforced|inconclusive> B=<deployed|not-deployed|inconclusive>`.
//! A network error does not stop the test. The test records the error and continues. The test
//! asserts one fact only: the cleanup step ran for both refs.
//!
//! ## Part A (two short-lived refs)
//!
//! Part A pushes one commit with the production push path
//! (`delivery_orchestrator::push_delivery`). It writes at most two refs under
//! `refs/heads/canary/` and then deletes them. The cleanup and the verification use plain
//! smart-HTTP requests, because the production push function cannot delete a ref.
//!
//! ## Part B (read-only)
//!
//! Part B sends only GET requests to `info/refs?service=git-receive-pack`. It records the HTTP
//! status of each request.
//!
//! ## The `method` tag
//!
//! The production client mints one token with `method = POST`. It sends that token on both legs:
//! the `info/refs` GET and the service POST. The relay reads the method from the event's own tag.
//! It does not compare that tag with the HTTP method (`authenticate_git` in the relay). This test
//! mints its tokens in the same way, so a result here transfers to production.
//!
//! ## Secrets
//!
//! The test never prints the secret key. It prints a token only in redacted form: the first 12
//! characters of the base64 text.

#![cfg(feature = "git-delivery")]

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use maxplayer_core::delivery_orchestrator::{PushRetryPolicy, push_delivery};
use maxplayer_core::delivery_transport::assert_allowed_repo_locator;
use maxplayer_core::git_transport::{delivery_ref, nip98_authorization_header_with_keys};
use nostr_sdk::JsonUtil;
use nostr_sdk::nips::nip98::{HttpData, HttpMethod};
use nostr_sdk::prelude::{EventBuilder, Keys, Tag, Timestamp, Url};

const KEY_FILE_ENV: &str = "MAXPLAYER_CANARY_KEY_FILE";
const REMOTE_ENV: &str = "MAXPLAYER_CANARY_REMOTE";
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const CANARY_REF_PREFIX: &str = "refs/heads/canary/";
/// Age of the old tokens in Part B, in seconds. The value is far outside the ±60 s window.
const OLD_TOKEN_AGE_SECS: u64 = 300;
/// Distance of the expiration tag from now, in seconds.
const EXPIRATION_AHEAD_SECS: u64 = 3600;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Redact a `Nostr <base64>` header: keep the first 12 characters of the base64 text.
fn redact(header: &str) -> String {
    let token = header.strip_prefix("Nostr ").unwrap_or(header);
    let head: String = token.chars().take(12).collect();
    format!("Nostr {head}…")
}

/// Make a text safe for one output line: replace control characters, cut at `max_chars`.
fn clean(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in text.chars().enumerate() {
        if count >= max_chars {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------------------------

struct Setup {
    keys: Keys,
    remote: String,
    run_id: u64,
}

/// Read the environment. Return `None` after one skip line when a variable is not set. Stop the
/// test with a clear message when the key or the URL is not valid. The message never holds the key.
fn setup() -> Option<Setup> {
    let key_file = std::env::var(KEY_FILE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let remote = std::env::var(REMOTE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let (Some(key_file), Some(remote)) = (key_file, remote) else {
        println!("CANARY SKIP set {KEY_FILE_ENV} and {REMOTE_ENV} to run the relay canary");
        return None;
    };

    let secret = std::fs::read_to_string(&key_file)
        .unwrap_or_else(|error| panic!("CANARY SETUP cannot read {key_file}: {error}"));
    let keys = Keys::parse(secret.trim()).unwrap_or_else(|_| {
        panic!("CANARY SETUP {key_file} does not hold a valid nostr secret key (64 hex chars)")
    });
    drop(secret);

    let remote = remote.trim().trim_end_matches('/').to_owned();
    if let Err(error) = assert_allowed_repo_locator(&remote) {
        panic!("CANARY SETUP {REMOTE_ENV} refused by the transport allowlist: {error}");
    }
    if let Err(error) = Url::parse(&remote) {
        panic!("CANARY SETUP {REMOTE_ENV} is not a URL: {error}");
    }

    Some(Setup {
        keys,
        remote,
        run_id: now_unix(),
    })
}

// ---------------------------------------------------------------------------------------------
// Token mint
// ---------------------------------------------------------------------------------------------

/// Mint a token through the production seam. `created_at` is now.
fn mint(keys: &Keys, remote: &str, scope: Option<&str>, expiration: Option<u64>) -> String {
    nip98_authorization_header_with_keys(remote, keys, scope, expiration.map(|value| value as i64))
        .expect("production mint of a canary token")
}

/// Mint a token with a custom `created_at`. The tag layout equals the production layout:
/// `u`, `method = POST`, then the optional `ref` tag, then the optional `expiration` tag.
/// This function lives in the test only. Production has no need for a custom `created_at`.
fn mint_at(
    keys: &Keys,
    remote: &str,
    scope: Option<&str>,
    expiration: Option<u64>,
    created_at: u64,
) -> String {
    let url = Url::parse(remote).expect("remote url was checked at setup");
    let mut builder = EventBuilder::http_auth(HttpData::new(url, HttpMethod::POST))
        .custom_created_at(Timestamp::from_secs(created_at));
    if let Some(refname) = scope {
        builder = builder.tag(Tag::parse(["ref", refname]).expect("ref tag"));
    }
    if let Some(expiry) = expiration {
        let value = expiry.to_string();
        builder = builder.tag(Tag::parse(["expiration", value.as_str()]).expect("expiration tag"));
    }
    let event = builder.sign_with_keys(keys).expect("sign a canary token");
    let encoded = base64::engine::general_purpose::STANDARD.encode(event.as_json());
    format!("Nostr {encoded}")
}

// ---------------------------------------------------------------------------------------------
// Plain smart-HTTP requests (read probes and cleanup)
// ---------------------------------------------------------------------------------------------

struct Http {
    client: reqwest::blocking::Client,
}

struct HttpReply {
    status: u16,
    www_authenticate: Option<String>,
    body: Vec<u8>,
}

impl Http {
    fn new() -> Self {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            // The production transport honours the same variable for a self-signed test relay.
            .danger_accept_invalid_certs(std::env::var_os("GIT_SSL_NO_VERIFY").is_some())
            .build()
            .expect("build reqwest blocking client");
        Self { client }
    }

    /// GET `<remote>/info/refs?service=git-receive-pack` with one `Authorization` header. The
    /// request headers equal the production transport headers.
    fn get_receive_pack_advertisement(
        &self,
        remote: &str,
        header: &str,
    ) -> Result<HttpReply, String> {
        let url = format!("{remote}/info/refs?service=git-receive-pack");
        let response = self
            .client
            .get(&url)
            .header("Accept", "*/*")
            .header("Accept-Encoding", "identity")
            .header("Authorization", header)
            .send()
            .map_err(|error| format!("http request: {error}"))?;
        Self::reply(response)
    }

    /// POST `<remote>/git-receive-pack` with a pkt-line body and one `Authorization` header.
    fn post_receive_pack(
        &self,
        remote: &str,
        header: &str,
        body: Vec<u8>,
    ) -> Result<HttpReply, String> {
        let url = format!("{remote}/git-receive-pack");
        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/x-git-receive-pack-request")
            .header("Accept", "application/x-git-receive-pack-result")
            .header("Accept-Encoding", "identity")
            .header("Authorization", header)
            .body(body)
            .send()
            .map_err(|error| format!("http request: {error}"))?;
        Self::reply(response)
    }

    fn reply(response: reqwest::blocking::Response) -> Result<HttpReply, String> {
        let status = response.status().as_u16();
        let www_authenticate = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .bytes()
            .map_err(|error| format!("http body: {error}"))?
            .to_vec();
        Ok(HttpReply {
            status,
            www_authenticate,
            body,
        })
    }
}

/// Frame one pkt-line: a 4-digit hex length (payload + 4) and the payload.
fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{:04x}", payload.len() + 4).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// Split a pkt-line stream into payloads. Skip the control packets (`0000`, `0001`, `0002`).
/// Stop at the first malformed length or truncated frame.
fn pkt_payloads(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= buf.len() {
        let len = std::str::from_utf8(&buf[i..i + 4])
            .ok()
            .and_then(|text| usize::from_str_radix(text, 16).ok());
        let Some(len) = len else { break };
        if len < 4 {
            i += 4;
            continue;
        }
        if i + len > buf.len() {
            break;
        }
        out.push(&buf[i + 4..i + len]);
        i += len;
    }
    out
}

/// Read the `(oid, refname)` pairs from a smart-HTTP ref advertisement.
fn parse_advertisement(body: &[u8]) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    for payload in pkt_payloads(body) {
        if payload.starts_with(b"# service=") {
            continue;
        }
        // The first ref line carries the capability list after a NUL byte.
        let line = payload.split(|byte| *byte == 0).next().unwrap_or(&[]);
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\n');
        let Some((oid, name)) = text.split_once(' ') else {
            continue;
        };
        if oid.len() != 40 || name == "capabilities^{}" {
            continue;
        }
        refs.push((oid.to_owned(), name.to_owned()));
    }
    refs
}

fn text_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim_end_matches('\n')
        .to_owned()
}

/// Read the report-status lines of a receive-pack reply. Handle a side-band frame too, in case
/// the server sends one; this test does not ask for side-band.
fn report_status_lines(body: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    for payload in pkt_payloads(body) {
        match payload.first() {
            Some(1) => lines.extend(pkt_payloads(&payload[1..]).into_iter().map(text_line)),
            Some(band @ (2 | 3)) => lines.push(format!("band{band}: {}", text_line(&payload[1..]))),
            _ => lines.push(text_line(payload)),
        }
    }
    lines
}

/// List the refs under `refs/heads/canary/` from an authenticated receive-pack advertisement.
fn list_canary_refs(http: &Http, remote: &str, header: &str) -> Result<Vec<String>, String> {
    let reply = http.get_receive_pack_advertisement(remote, header)?;
    if reply.status != 200 {
        return Err(format!(
            "advertisement http {} body=\"{}\"",
            reply.status,
            clean(&String::from_utf8_lossy(&reply.body), 100)
        ));
    }
    Ok(parse_advertisement(&reply.body)
        .into_iter()
        .filter(|(_, name)| name.starts_with(CANARY_REF_PREFIX))
        .map(|(_, name)| name)
        .collect())
}

fn presence(refs: &[String], name: &str) -> &'static str {
    if refs.iter().any(|candidate| candidate == name) {
        "present"
    } else {
        "absent"
    }
}

/// Delete one remote ref with one token. Send a delete command and no pack. Return `absent` when
/// the ref is not on the remote, `deleted` on an `ok` status line, and an error otherwise.
fn delete_remote_ref(
    http: &Http,
    remote: &str,
    refname: &str,
    header: &str,
) -> Result<String, String> {
    let advertisement = http.get_receive_pack_advertisement(remote, header)?;
    if advertisement.status != 200 {
        return Err(format!(
            "advertisement http {} body=\"{}\"",
            advertisement.status,
            clean(&String::from_utf8_lossy(&advertisement.body), 100)
        ));
    }
    let refs = parse_advertisement(&advertisement.body);
    let Some((oid, _)) = refs.iter().find(|(_, name)| name == refname) else {
        return Ok("absent".to_owned());
    };

    let command = format!("{oid} {ZERO_OID} {refname}\0report-status");
    let mut body = pkt_line(command.as_bytes());
    body.extend_from_slice(b"0000");
    let reply = http.post_receive_pack(remote, header, body)?;
    if reply.status != 200 {
        return Err(format!(
            "receive-pack http {} body=\"{}\"",
            reply.status,
            clean(&String::from_utf8_lossy(&reply.body), 120)
        ));
    }
    let lines = report_status_lines(&reply.body);
    let ok_line = format!("ok {refname}");
    if lines.iter().any(|line| line == &ok_line) {
        Ok(format!("deleted (was {oid})"))
    } else {
        Err(format!(
            "report-status=[{}]",
            clean(&lines.join(" | "), 200)
        ))
    }
}

/// Delete every ref in `refs`. Try a token scoped to the ref first. On a failure, try once more
/// with an unscoped token, so a leftover ref does not depend on the scope rules. Print one line
/// for each ref. Return the number of refs processed.
fn cleanup(http: &Http, keys: &Keys, remote: &str, refs: &[&str]) -> usize {
    for refname in refs {
        let scoped = mint(keys, remote, Some(refname), None);
        let observed = match delete_remote_ref(http, remote, refname, &scoped) {
            Ok(text) => format!("scoped-token {text}"),
            Err(first) => {
                let unscoped = mint(keys, remote, None, None);
                match delete_remote_ref(http, remote, refname, &unscoped) {
                    Ok(text) => format!("scoped-token failed ({first}); unscoped-token {text}"),
                    Err(second) => format!("failed scoped=({first}) unscoped=({second})"),
                }
            }
        };
        println!(
            "CANARY A5 cleanup ref={refname} token={} observed={observed}",
            redact(&scoped)
        );
    }
    refs.len()
}

// ---------------------------------------------------------------------------------------------
// Part A: pushes through the production path
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PushOutcome {
    Accepted,
    Refused,
    Other,
    Skipped,
}

/// Create a git repository with one commit. Point both canary refs at that commit. Use git2 only.
fn make_temp_repo(
    workdir: &Path,
    scope_ref: &str,
    other_ref: &str,
    run_id: u64,
) -> Result<String, String> {
    std::fs::create_dir_all(workdir).map_err(|error| format!("create workdir: {error}"))?;
    let repo = git2::Repository::init(workdir).map_err(|error| format!("git init: {error}"))?;
    std::fs::write(
        workdir.join("canary.txt"),
        format!("maxplayer relay canary {run_id}\n"),
    )
    .map_err(|error| format!("write file: {error}"))?;
    let mut index = repo.index().map_err(|error| format!("index: {error}"))?;
    index
        .add_path(Path::new("canary.txt"))
        .map_err(|error| format!("index add: {error}"))?;
    index
        .write()
        .map_err(|error| format!("index write: {error}"))?;
    let tree_oid = index
        .write_tree()
        .map_err(|error| format!("write tree: {error}"))?;
    let tree = repo
        .find_tree(tree_oid)
        .map_err(|error| format!("find tree: {error}"))?;
    let signature = git2::Signature::now("maxplayer relay canary", "canary@maxplayer.invalid")
        .map_err(|error| format!("signature: {error}"))?;
    let commit = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            &format!("relay canary {run_id}"),
            &tree,
            &[],
        )
        .map_err(|error| format!("commit: {error}"))?;
    repo.reference(scope_ref, commit, true, "relay canary")
        .map_err(|error| format!("create {scope_ref}: {error}"))?;
    repo.reference(other_ref, commit, true, "relay canary")
        .map_err(|error| format!("create {other_ref}: {error}"))?;
    Ok(commit.to_string())
}

/// Push `branch` with the production push path and classify the answer of the relay.
///
/// One attempt: the canary records the first answer. Production retries a transient failure; a
/// retry adds no evidence here. A hook refusal reaches the client in-band as
/// `ng <ref> pre-receive hook declined` (HTTP 200); the transport reports it as a rejected ref.
fn run_push(workdir: &Path, remote: &str, branch: &str, header: String) -> (PushOutcome, String) {
    let policy = PushRetryPolicy {
        max_attempts: 1,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };
    // The canary measures the relay's answer only. The C6 oid guard (`expected_oid`) has its own
    // unit tests in `delivery_orchestrator`; `None` keeps this probe about the relay.
    match push_delivery(workdir, remote, branch, Some(header), &policy, None) {
        Ok(oid) => (PushOutcome::Accepted, format!("accepted oid={oid}")),
        Err(error) => {
            let text = clean(&error.to_string(), 300);
            let lowered = text.to_ascii_lowercase();
            let refused = lowered.contains("declined")
                || lowered.contains("403")
                || lowered.contains("forbidden")
                || lowered.contains("rejected ref");
            if refused {
                (PushOutcome::Refused, format!("refused error=\"{text}\""))
            } else {
                (PushOutcome::Other, format!("error=\"{text}\""))
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Part B: read probes
// ---------------------------------------------------------------------------------------------

/// GET the receive-pack advertisement with `header`. Print one line. Return the HTTP status, or
/// `None` on a transport error.
fn probe(http: &Http, remote: &str, label: &str, header: &str, expected: &str) -> Option<u16> {
    match http.get_receive_pack_advertisement(remote, header) {
        Ok(reply) => {
            let mut detail = format!("http={}", reply.status);
            if reply.status != 200 {
                if let Some(challenge) = &reply.www_authenticate {
                    detail.push_str(&format!(" www-authenticate=\"{}\"", clean(challenge, 80)));
                }
                detail.push_str(&format!(
                    " body=\"{}\"",
                    clean(&String::from_utf8_lossy(&reply.body), 100)
                ));
            }
            println!(
                "CANARY {label} expected={expected} observed={detail} token={}",
                redact(header)
            );
            Some(reply.status)
        }
        Err(error) => {
            println!(
                "CANARY {label} expected={expected} observed=error \"{}\" token={}",
                clean(&error, 200),
                redact(header)
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Verdict
// ---------------------------------------------------------------------------------------------

fn verdict_a(a2: PushOutcome, a3: PushOutcome) -> (&'static str, &'static str) {
    match (a2, a3) {
        (PushOutcome::Accepted, _) => (
            "not-enforced",
            "A2: the relay accepted a push to a ref outside the token scope",
        ),
        (PushOutcome::Refused, PushOutcome::Accepted) => (
            "enforced",
            "A2: the relay refused the push outside the scope; A3: the relay accepted the push inside the scope",
        ),
        (PushOutcome::Refused, _) => (
            "inconclusive",
            "A2 was refused, but the positive control A3 was not accepted; the refusal can have another cause",
        ),
        _ => (
            "inconclusive",
            "A2 did not run, or it failed for a reason other than a refusal",
        ),
    }
}

fn verdict_b([b0, b1, b2, b3, b4]: [Option<u16>; 5]) -> (&'static str, String) {
    let show = |status: Option<u16>| {
        status
            .map(|value| value.to_string())
            .unwrap_or_else(|| "error".to_owned())
    };
    if b0 != Some(200) || b1 != Some(200) || b4 != Some(200) {
        return (
            "inconclusive",
            format!(
                "a fresh control token was not accepted (B0={}, B1={}, B4={}; all must be 200)",
                show(b0),
                show(b1),
                show(b4)
            ),
        );
    }
    if b3 != Some(401) {
        return (
            "inconclusive",
            format!(
                "the old unscoped token was not refused (B3={}; expected 401), so the age check is not visible",
                show(b3)
            ),
        );
    }
    match b2 {
        Some(200) => (
            "deployed",
            "the old scoped token with an expiration tag was accepted (B2=200); the old unscoped token was refused (B3=401)".to_owned(),
        ),
        Some(401) => (
            "not-deployed",
            "the old scoped token with an expiration tag was refused (B2=401), the same as the old unscoped token".to_owned(),
        ),
        other => (
            "inconclusive",
            format!("B2 returned {} (expected 200 or 401)", show(other)),
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "manual relay canary: set MAXPLAYER_CANARY_KEY_FILE and MAXPLAYER_CANARY_REMOTE"]
fn relay_canary() {
    let Some(Setup {
        keys,
        remote,
        run_id,
    }) = setup()
    else {
        return;
    };
    println!(
        "CANARY SETUP remote={remote} pubkey={} run={run_id}",
        keys.public_key().to_hex()
    );
    let http = Http::new();

    // ---- Part A ----
    let scope_branch = format!("canary/{run_id}-scope");
    let other_branch = format!("canary/{run_id}-other");
    // The same function names the ref the production push writes, so scope and push agree.
    let scope_ref = delivery_ref(&scope_branch);
    let other_ref = delivery_ref(&other_branch);

    let workdir = std::env::temp_dir().join(format!(
        "maxplayer-relay-canary-{}-{run_id}",
        std::process::id()
    ));
    let (a2, a3) = match make_temp_repo(&workdir, &scope_ref, &other_ref, run_id) {
        Ok(commit) => {
            println!(
                "CANARY A1 temp-repo observed=ok commit={commit} scope-ref={scope_ref} other-ref={other_ref}"
            );

            // A2: a token scoped to `-scope`, a push to `-other`. Expected: refused.
            let token = mint(&keys, &remote, Some(&scope_ref), None);
            let (a2, detail) = run_push(&workdir, &remote, &other_branch, token.clone());
            println!(
                "CANARY A2 push-other-ref token-scope={scope_ref} pushed-ref={other_ref} expected=refused observed={detail} token={}",
                redact(&token)
            );

            // A3: positive control. The same scope, a push to `-scope`. Expected: accepted.
            let token = mint(&keys, &remote, Some(&scope_ref), None);
            let (a3, detail) = run_push(&workdir, &remote, &scope_branch, token.clone());
            println!(
                "CANARY A3 push-scoped-ref token-scope={scope_ref} pushed-ref={scope_ref} expected=accepted observed={detail} token={}",
                redact(&token)
            );
            (a2, a3)
        }
        Err(error) => {
            println!(
                "CANARY A1 temp-repo observed=error \"{}\"",
                clean(&error, 200)
            );
            println!("CANARY A2 push-other-ref expected=refused observed=skipped");
            println!("CANARY A3 push-scoped-ref expected=accepted observed=skipped");
            (PushOutcome::Skipped, PushOutcome::Skipped)
        }
    };

    // A4: the remote view before the cleanup. An independent check of A2 and A3.
    match list_canary_refs(&http, &remote, &mint(&keys, &remote, None, None)) {
        Ok(refs) => println!(
            "CANARY A4 remote-refs-before-cleanup other={} scope={} canary-refs=[{}]",
            presence(&refs, &other_ref),
            presence(&refs, &scope_ref),
            refs.join(",")
        ),
        Err(error) => println!(
            "CANARY A4 remote-refs-before-cleanup observed=error \"{}\"",
            clean(&error, 200)
        ),
    }

    // A5: the cleanup. It runs for both refs, whatever the earlier outcomes were.
    let cleaned = cleanup(&http, &keys, &remote, &[&other_ref, &scope_ref]);

    // A6: verification. List every ref that remains under `refs/heads/canary/`.
    match list_canary_refs(&http, &remote, &mint(&keys, &remote, None, None)) {
        Ok(refs) => println!(
            "CANARY A6 remaining-canary-refs count={} refs=[{}]",
            refs.len(),
            refs.join(",")
        ),
        Err(error) => println!(
            "CANARY A6 remaining-canary-refs observed=error \"{}\"",
            clean(&error, 200)
        ),
    }
    let _ = std::fs::remove_dir_all(&workdir);

    // ---- Part B ----
    let now = now_unix();
    let old = now.saturating_sub(OLD_TOKEN_AGE_SECS);
    let expiration = now + EXPIRATION_AHEAD_SECS;
    // B0: a control for the test-local minter. A fresh unscoped token from `mint_at` must pass.
    let b0 = probe(
        &http,
        &remote,
        "B0 fresh-unscoped-testmint created_at=now scope=no expiration=no",
        &mint_at(&keys, &remote, None, None, now),
        "200",
    );
    let b1 = probe(
        &http,
        &remote,
        "B1 fresh-scoped-with-expiration created_at=now scope=yes expiration=now+3600",
        &mint(&keys, &remote, Some(&scope_ref), Some(expiration)),
        "200",
    );
    let b2 = probe(
        &http,
        &remote,
        "B2 old-scoped-with-expiration created_at=now-300 scope=yes expiration=now+3600",
        &mint_at(&keys, &remote, Some(&scope_ref), Some(expiration), old),
        "200-if-deployed",
    );
    let b3 = probe(
        &http,
        &remote,
        "B3 old-unscoped created_at=now-300 scope=no expiration=no",
        &mint_at(&keys, &remote, None, None, old),
        "401",
    );
    let b4 = probe(
        &http,
        &remote,
        "B4 fresh-unscoped created_at=now scope=no expiration=no",
        &mint(&keys, &remote, None, None),
        "200",
    );

    // ---- Verdict ----
    let (verdict_a, reason_a) = verdict_a(a2, a3);
    let (verdict_b, reason_b) = verdict_b([b0, b1, b2, b3, b4]);
    println!("CANARY REASON A: {reason_a}");
    println!("CANARY REASON B: {reason_b}");
    println!("CANARY VERDICT A={verdict_a} B={verdict_b}");

    assert_eq!(cleaned, 2, "the cleanup step must run for both canary refs");
}

// ---------------------------------------------------------------------------------------------
// Offline checks of the helpers the live run depends on. These run without the env vars.
// ---------------------------------------------------------------------------------------------

#[test]
fn pkt_line_frames_and_parses() {
    let mut stream = pkt_line(b"unpack ok\n");
    stream.extend_from_slice(b"0000");
    stream.extend_from_slice(&pkt_line(b"ok refs/heads/canary/1-scope\n"));
    let payloads = pkt_payloads(&stream);
    assert_eq!(
        payloads,
        vec![&b"unpack ok\n"[..], &b"ok refs/heads/canary/1-scope\n"[..]]
    );
    assert_eq!(&pkt_line(b"abc")[..4], b"0007");
}

#[test]
fn advertisement_parse_skips_service_line_and_capabilities() {
    let oid_a = "a".repeat(40);
    let oid_b = "b".repeat(40);
    let mut body = pkt_line(b"# service=git-receive-pack\n");
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&pkt_line(
        format!("{oid_a} refs/heads/main\0report-status delete-refs\n").as_bytes(),
    ));
    body.extend_from_slice(&pkt_line(
        format!("{oid_b} refs/heads/canary/1-scope\n").as_bytes(),
    ));
    body.extend_from_slice(b"0000");
    assert_eq!(
        parse_advertisement(&body),
        vec![
            (oid_a, "refs/heads/main".to_owned()),
            (oid_b, "refs/heads/canary/1-scope".to_owned()),
        ]
    );

    // An empty repository advertises only the capability marker.
    let mut empty = pkt_line(b"# service=git-receive-pack\n");
    empty.extend_from_slice(b"0000");
    empty.extend_from_slice(&pkt_line(
        format!("{ZERO_OID} capabilities^{{}}\0report-status\n").as_bytes(),
    ));
    empty.extend_from_slice(b"0000");
    assert!(parse_advertisement(&empty).is_empty());
}

#[test]
fn report_status_lines_read_plain_and_side_band_frames() {
    let mut plain = pkt_line(b"unpack ok\n");
    plain.extend_from_slice(&pkt_line(
        b"ng refs/heads/canary/1-other pre-receive hook declined\n",
    ));
    plain.extend_from_slice(b"0000");
    assert_eq!(
        report_status_lines(&plain),
        vec![
            "unpack ok",
            "ng refs/heads/canary/1-other pre-receive hook declined"
        ]
    );

    let mut inner = pkt_line(b"unpack ok\n");
    inner.extend_from_slice(&pkt_line(b"ok refs/heads/canary/1-scope\n"));
    inner.extend_from_slice(b"0000");
    let mut band1 = vec![1u8];
    band1.extend_from_slice(&inner);
    let mut framed = pkt_line(&band1);
    framed.extend_from_slice(b"0000");
    assert_eq!(
        report_status_lines(&framed),
        vec!["unpack ok", "ok refs/heads/canary/1-scope"]
    );
}

#[test]
fn redact_keeps_only_a_prefix_of_the_token() {
    let header = format!("Nostr {}", "x".repeat(200));
    assert_eq!(redact(&header), format!("Nostr {}…", "x".repeat(12)));
}

#[test]
fn verdicts_follow_the_observations() {
    use PushOutcome::*;
    assert_eq!(verdict_a(Refused, Accepted).0, "enforced");
    assert_eq!(verdict_a(Accepted, Accepted).0, "not-enforced");
    assert_eq!(verdict_a(Accepted, Refused).0, "not-enforced");
    assert_eq!(verdict_a(Refused, Refused).0, "inconclusive");
    assert_eq!(verdict_a(Other, Accepted).0, "inconclusive");
    assert_eq!(verdict_a(Skipped, Skipped).0, "inconclusive");

    let ok = Some(200);
    let denied = Some(401);
    assert_eq!(verdict_b([ok, ok, ok, denied, ok]).0, "deployed");
    assert_eq!(verdict_b([ok, ok, denied, denied, ok]).0, "not-deployed");
    assert_eq!(verdict_b([ok, ok, ok, ok, ok]).0, "inconclusive");
    assert_eq!(verdict_b([ok, denied, ok, denied, ok]).0, "inconclusive");
    assert_eq!(verdict_b([None, ok, ok, denied, ok]).0, "inconclusive");
    assert_eq!(verdict_b([ok, ok, Some(500), denied, ok]).0, "inconclusive");
}
