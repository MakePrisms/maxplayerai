//! strfry write-policy plugin for the maxplayer launch relay.
//!
//! strfry spawns this as a child of its own systemd unit and speaks JSONL over
//! stdin/stdout: one JSON object per candidate event on stdin, one verdict per line on
//! stdout. The unit's sandbox sets `MemoryDenyWriteExecute = true`, which is why this is
//! a compiled binary and not a script in a JIT runtime — a runtime that maps W+X pages
//! dies at spawn under that directive, and it dies at spawn, not at build.
//!
//! Policy: accept an event iff it is inside the configured namespace — it carries a
//! `["t", <tag>]` tag whose value equals `MAXPLAYER_RELAY_TAG` — OR it is one of the
//! identity/discovery kinds a seat must publish to be found. Everything else is rejected.
//! This is the mechanism that makes "born v1/maxplayer-only" a property the relay
//! *enforces* rather than a state it *hopes* holds: born-empty is only about day-one
//! contents, and says nothing about what day two accepts.
//!
//! The tag is parameterised on its VALUE on purpose: the nix module supplies it from
//! `services.maxplayer.relay.namespaceTag`, so the relay's namespace is configuration rather
//! than a recompile. A hardcoded value would silently reject every event outside it while the
//! relay looked like a healthy quiet box.

use std::io::{self, BufRead, Write};

use serde::Serialize;
use serde_json::Value;

/// Kinds a seat publishes to be discovered, which `profile.rs` does not yet tag with
/// `t` (kind 0 metadata, NIP-89 handler 31990, NIP-34 git repo announcement 30617).
///
/// ⚠ TEMPORARY allowlist — delete once #365 adds `["t", <tag>]` to those builders, after
/// which the predicate is uniform. It is here because a relay that required the `t` tag
/// on *every* event would reject kind-0 and kind-31990, which is exactly how a seat
/// pubkey is resolved — the relay would serve normally and look healthy while making its
/// own participants undiscoverable.
const DISCOVERY_KINDS: &[u64] = &[0, 31990, 30617];

/// NIP-17 private-DM kinds, admitted by KIND rather than by namespace tag. A NIP-59 gift wrap
/// (1059) is signed by a throwaway ephemeral key and carries ONLY a recipient `p` tag — stamping
/// `["t", <ns>]` on it would deanonymise every payment on the relay and break the NIP-59 structure,
/// so no `t`-predicate can ever gate it. NAMED TRADE-OFF: for these kinds the relay accepts
/// content-blind, anonymous DM traffic — the single-namespace guarantee does not hold here, and
/// nothing in this predicate can distinguish a real payment from spam. Abuse is BOUNDED, not
/// filtered (see `nip17_admissible`): a gift wrap must be addressed (a `p` tag) and within a size
/// ceiling; coarser limits (per-connection rate, strfry `maxEventSize`, optional PoW) belong at the
/// strfry/proxy layer.
const NIP17_KINDS: &[u64] = &[
    1059,  // NIP-59 gift wrap — the buyer→seller NUT-18 ecash payment DM (this is the payment path)
    10050, // NIP-17 DM-relay-list — not published today; pre-allowed for future DM-inbox discovery
];

/// Generous ceiling on a NIP-17 event's `content`. A real NUT-18 payment wrap is a few KB; this
/// sits far above that, so it only refuses blob-carrying wraps, never a payment. strfry's global
/// `maxEventSize` is the harder backstop.
const MAX_NIP17_CONTENT_BYTES: usize = 128 * 1024;

/// strfry's verdict shape: the event id it decided on, an action, and a message returned
/// to the client on rejection.
#[derive(Serialize)]
struct Verdict {
    id: String,
    action: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    msg: String,
}

impl Verdict {
    fn accept(id: String) -> Self {
        Verdict { id, action: "accept", msg: String::new() }
    }
    fn reject(id: String, msg: &str) -> Self {
        Verdict { id, action: "reject", msg: msg.to_string() }
    }
}

fn main() {
    let namespace_tag = match std::env::var("MAXPLAYER_RELAY_TAG") {
        Ok(t) if !t.is_empty() => t,
        _ => {
            // No default. A policy with an empty tag would accept nothing that matches and
            // reject every real event — indistinguishable in the relay's logs from a
            // correctly-running strict relay. Refuse to run instead, so preflight/strfry
            // surface a spawn failure rather than a silent deny-all.
            eprintln!("maxplayer-write-policy: MAXPLAYER_RELAY_TAG is unset or empty — refusing to run");
            std::process::exit(1);
        }
    };

    // One line at startup so the relay's journal shows the plugin came up and with which
    // namespace — the difference an operator needs between "policy rejected it" and
    // "the plugin never ran".
    eprintln!("maxplayer-write-policy: online, namespace tag = {namespace_tag:?}");

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("maxplayer-write-policy: stdin read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let verdict = decide(&line, &namespace_tag);

        // One verdict per input line, flushed immediately. strfry reads responses
        // synchronously per event; a buffered verdict stalls the relay's ingestion.
        if serde_json::to_writer(&mut out, &verdict).and_then(|()| Ok(out.write_all(b"\n"))).is_err() {
            // stdout closed: strfry has gone away. Nothing left to serve.
            break;
        }
        if out.flush().is_err() {
            break;
        }
    }
}

/// Decide a single strfry write-policy message. The event is nested under `.event`; the
/// top-level `.type` ("new" / "lookback") does not change the verdict — a born-empty,
/// single-namespace relay only ever holds in-namespace events, so applying one uniform
/// predicate is both correct and the least surprising thing to audit.
fn decide(line: &str, namespace_tag: &str) -> Verdict {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            // strfry validates id + signature before consulting the plugin, so a
            // malformed line here is a protocol fault, not a hostile event. We have no
            // id to echo; reject with an empty id and let strfry log the mismatch.
            eprintln!("maxplayer-write-policy: unparseable input: {e}");
            return Verdict::reject(String::new(), "unparseable event json");
        }
    };

    let event = &msg["event"];
    let id = event["id"].as_str().unwrap_or_default().to_string();

    if let Some(kind) = event["kind"].as_u64() {
        if DISCOVERY_KINDS.contains(&kind) {
            return Verdict::accept(id);
        }
        if NIP17_KINDS.contains(&kind) {
            return match nip17_admissible(kind, event) {
                Ok(()) => Verdict::accept(id),
                Err(reason) => Verdict::reject(id, reason),
            };
        }
    }

    let in_namespace = event["tags"].as_array().is_some_and(|tags| {
        tags.iter().any(|tag| {
            tag.as_array().is_some_and(|parts| {
                parts.len() >= 2
                    && parts[0].as_str() == Some("t")
                    && parts[1].as_str() == Some(namespace_tag)
            })
        })
    });

    if in_namespace {
        Verdict::accept(id)
    } else {
        Verdict::reject(id, "event is outside this relay's namespace")
    }
}

/// Bounded, content-blind admission for the NIP-17 DM kinds (see `NIP17_KINDS`). We can neither
/// read nor namespace an encrypted wrap, so we enforce only that it is well-formed and small:
/// addressed to a recipient (`p` tag, gift wraps only) and within the size ceiling. Everything else
/// about it is deliberately unfiltered.
fn nip17_admissible(kind: u64, event: &Value) -> Result<(), &'static str> {
    if let Some(content) = event["content"].as_str() {
        if content.len() > MAX_NIP17_CONTENT_BYTES {
            return Err("nip-17 event exceeds size ceiling");
        }
    }
    // A gift wrap with no recipient is malformed, not a payment.
    if kind == 1059 {
        let addressed = event["tags"].as_array().is_some_and(|tags| {
            tags.iter().any(|tag| {
                tag.as_array()
                    .is_some_and(|parts| parts.len() >= 2 && parts[0].as_str() == Some("p"))
            })
        });
        if !addressed {
            return Err("nip-17 gift wrap missing recipient p-tag");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(kind: u64, tags: Value) -> String {
        serde_json::json!({
            "type": "new",
            "event": { "id": "abc", "kind": kind, "tags": tags }
        })
        .to_string()
    }

    fn msg_with_content(kind: u64, tags: Value, content: &str) -> String {
        serde_json::json!({
            "type": "new",
            "event": { "id": "abc", "kind": kind, "tags": tags, "content": content }
        })
        .to_string()
    }

    #[test]
    fn accepts_in_namespace() {
        let v = decide(&msg(3400, serde_json::json!([["t", "maxplayer"]])), "maxplayer");
        assert_eq!(v.action, "accept");
        assert_eq!(v.id, "abc");
    }

    #[test]
    fn rejects_foreign_namespace() {
        let v = decide(&msg(1, serde_json::json!([["t", "other"]])), "maxplayer");
        assert_eq!(v.action, "reject");
    }

    #[test]
    fn rejects_untagged_non_discovery() {
        let v = decide(&msg(1, serde_json::json!([])), "maxplayer");
        assert_eq!(v.action, "reject");
    }

    #[test]
    fn discovery_kinds_bypass_the_tag() {
        for kind in [0u64, 31990, 30617] {
            let v = decide(&msg(kind, serde_json::json!([])), "maxplayer");
            assert_eq!(v.action, "accept", "kind {kind} should be allowlisted");
        }
    }

    #[test]
    fn verdict_flips_with_the_configured_tag() {
        // The discriminator: the SAME event, two configured tags, opposite verdicts. A
        // policy that accepted everything and one that is not running are otherwise
        // indistinguishable from the relay's logs.
        let line = msg(3400, serde_json::json!([["t", "maxplayer"]]));
        assert_eq!(decide(&line, "maxplayer").action, "accept");
        assert_eq!(decide(&line, "maxplayer").action, "reject");
    }

    #[test]
    fn tag_with_trailing_elements_still_matches() {
        let v = decide(&msg(3400, serde_json::json!([["t", "maxplayer", "wss://relay"]])), "maxplayer");
        assert_eq!(v.action, "accept");
    }

    #[test]
    fn accepts_nip17_gift_wrap_addressed() {
        // RED-PROVE: the payment that failed pre-fix now passes. A real NIP-59 gift wrap carries
        // only a recipient `p` tag (never `["t","maxplayer"]`), so it must be admitted by kind.
        let v = decide(&msg(1059, serde_json::json!([["p", "deadbeef"]])), "maxplayer");
        assert_eq!(v.action, "accept");
    }

    #[test]
    fn control_untagged_non_dm_kind_still_rejected() {
        // The other half of the red-prove: widening for NIP-17 must NOT open a non-DM kind.
        let v = decide(&msg(1, serde_json::json!([])), "maxplayer");
        assert_eq!(v.action, "reject");
    }

    #[test]
    fn rejects_gift_wrap_without_recipient() {
        let v = decide(&msg(1059, serde_json::json!([])), "maxplayer");
        assert_eq!(v.action, "reject");
    }

    #[test]
    fn rejects_oversized_nip17_event() {
        let big = "x".repeat(MAX_NIP17_CONTENT_BYTES + 1);
        let v = decide(&msg_with_content(1059, serde_json::json!([["p", "deadbeef"]]), &big), "maxplayer");
        assert_eq!(v.action, "reject");
    }

    #[test]
    fn accepts_dm_relay_list() {
        let v = decide(&msg(10050, serde_json::json!([])), "maxplayer");
        assert_eq!(v.action, "accept");
    }
}
