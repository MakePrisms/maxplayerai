//! The one plain-language hint for a relay refusal caused by the email-registration gate.
//!
//! Email signup spec v2.12 §6: once relay.maxplayer.ai runs with `BUZZ_REQUIRE_RELAY_MEMBERSHIP`,
//! only registered keys may sign in (NIP-42) and therefore write. Every client that is refused must
//! say "run `maxplayer account link`". This module owns the recognised refusal texts and the hint, so
//! the CLI, the daemons, the mint server and the sidecars all print the same words.
//!
//! The recognised texts come from `crates/buzz` (relay) and nostr-sdk 0.44 (client):
//!
//! | Text | Where it comes from |
//! |---|---|
//! | `restricted: not a relay member` | buzz NIP-42 AUTH refusal (`handlers/auth.rs`) and relay-git 403 (`api/git/transport.rs`) |
//! | `restricted: you were removed from this relay` | buzz closes a live session after a NIP-43 remove (`handlers/relay_admin.rs`) |
//! | `relay_membership_required` / `You must be a relay member` | buzz HTTP API 403 (`api/mod.rs`) |
//! | `relay membership required` | buzz media API (`buzz-media/src/error.rs`) |
//! | `auth-required:` | buzz refuses an EVENT from a session that never signed in (`handlers/event.rs`); a refused sign-in leaves the session there. Except `auth-required: verification failed` (a bad or clock-skewed AUTH event), `auth-required: already authenticated` and `auth-required: agent owner recorded; reconnect`, which linking does not fix |
//! | `authentication failed` | nostr-sdk `Error::AuthenticationFailed` and the NIP-42 messages in `relay_auth`/`doctor`. Not git's `Authentication failed for '<url>'` nor this workspace's git `auth failed: …` (HTTP credentials, not the relay key) |
//! | `NIP-42 sign-in failed`, `sign-in (NIP-42) failed`, `NIP-42 rejected`, `NIP-42 failed`, `auth rejected` | this workspace's own wording of the same refusal (resubscribe task, mint connector, chat sidecar, test relay) |
//!
//! nostr-sdk drops the relay's AUTH refusal text and only reports `AuthenticationFailed`, so a
//! client cannot tell "not a member" from any other refused sign-in. The hint therefore says the key
//! *may* not be registered. Bans (`blocked: …`) and timeouts are not matched: linking does not lift
//! them. Plain `restricted:` is not matched either, because buzz also uses it for p-gated reads and
//! channel rules that have nothing to do with registration.
//!
//! Ungated on purpose: the mint server, the chat and swarm sidecars build `maxplayer-core` without
//! `wallet`, and they must print the same hint.

/// The command every refused client points at.
pub const LINK_COMMAND: &str = "maxplayer account link";

/// Appended to a refusal from a client that signs with the agent's home key.
pub const LINK_HINT: &str = "this agent's key may not be registered yet: run `maxplayer account link` \
     to link it to your email (public reads still work without it; private messages and payments \
     need it)";

/// Appended to a refusal seen by the mint server, which signs with its own `<home>/mint/nostr.key`.
pub const MINT_LINK_HINT: &str = "this mint's key may not be registered yet: run \
     `maxplayer account link --mint` to link it to your email";

/// Lowercased fragments that identify a registration (membership / sign-in) refusal.
const MARKERS: &[&str] = &[
    "not a relay member",
    "you were removed from this relay",
    "relay_membership_required",
    "must be a relay member",
    "relay membership required",
    "auth-required:",
    "nip-42 sign-in failed",
    "sign-in (nip-42) failed",
    "nip-42 rejected",
    "nip-42 failed",
    "auth rejected",
];

/// Lowercased refusals that contain a marker but are not about registration. They are cut out of
/// the text before matching, so a message listing several relays still matches on the others.
const NOT_REGISTRATION: &[&str] = &[
    "auth-required: verification failed",
    "auth-required: already authenticated",
    "auth-required: agent owner recorded",
];

/// nostr-sdk's bare `authentication failed` (its `Error::AuthenticationFailed` text, which is all a
/// client learns of a refused NIP-42 sign-in). git uses the same words for HTTP credentials:
/// `Authentication failed for '<url>'` from the git CLI, and `auth failed: …` is this workspace's
/// git transport error, so those two forms are not counted.
fn sdk_authentication_failed(lower: &str) -> bool {
    const TEXT: &str = "authentication failed";
    lower.match_indices(TEXT).any(|(at, _)| {
        !lower[at + TEXT.len()..].starts_with(" for ") && !lower[..at].ends_with("auth failed: ")
    })
}

/// True when `text` reads like the relay refused this key at sign-in or write for lack of
/// registration (see the module table).
pub fn is_registration_refusal(text: &str) -> bool {
    let mut lower = text.to_ascii_lowercase();
    for excluded in NOT_REGISTRATION {
        lower = lower.replace(excluded, "");
    }
    MARKERS.iter().any(|marker| lower.contains(marker)) || sdk_authentication_failed(&lower)
}

/// `text`, plus [`LINK_HINT`] when it is a registration refusal that does not already name the
/// command. Anything else is returned unchanged, so this is safe to wrap around any relay error.
pub fn with_hint(text: impl Into<String>) -> String {
    with_hint_for(text, LINK_HINT)
}

/// [`with_hint`] with a caller-chosen hint (the mint server passes [`MINT_LINK_HINT`]).
pub fn with_hint_for(text: impl Into<String>, hint: &str) -> String {
    let text = text.into();
    if is_registration_refusal(&text) && !text.contains(LINK_COMMAND) {
        format!("{text} ({hint})")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every buzz refusal string the membership gate produces, copied from `crates/buzz`.
    const BUZZ_REFUSALS: &[&str] = &[
        "restricted: not a relay member",
        "restricted: you were removed from this relay",
        "auth-required: not authenticated",
        r#"{"error":"relay_membership_required","message":"You must be a relay member to access this relay"}"#,
        "relay membership required",
    ];

    #[test]
    fn every_buzz_membership_refusal_gets_the_hint() {
        for refusal in BUZZ_REFUSALS {
            let hinted = with_hint(*refusal);
            assert!(hinted.starts_with(refusal), "{hinted}");
            assert!(hinted.contains("run `maxplayer account link`"), "{hinted}");
        }
    }

    #[test]
    fn client_side_sign_in_failures_get_the_hint() {
        for text in [
            "authentication failed",
            "NIP-42 authentication failed (required for kind-1059 p-gated receive)",
            "maxplayer-mint: relay wss://r: NIP-42 sign-in failed (auth error)",
            "relay sign-in (NIP-42) failed on every reachable relay for mint nostr://x",
            "auth: NIP-42 rejected",
            "wss://relay.example: restricted: auth rejected",
            "no relay accepted event (wss://r: authentication failed)",
            "relay authentication failed",
            // A listed relay with an unrelated refusal does not hide a real one.
            "no relay accepted event (wss://a: auth-required: verification failed; wss://b: \
             restricted: not a relay member)",
            "wss://r: auth-required: authentication already failed",
        ] {
            assert!(is_registration_refusal(text), "{text}");
        }
    }

    #[test]
    fn unrelated_refusals_are_left_alone() {
        for text in [
            "blocked: you are banned from this community",
            "restricted: you are timed out until 2026-10-28T00:00:00Z",
            "restricted: p-gated events require #p matching your pubkey",
            "invalid: event timestamp too far from server time",
            "rate-limited: slow down",
            "error: internal error checking relay membership",
            "timeout",
            // git's HTTP credential failures: a GitHub token problem, not the relay key.
            "fatal: Authentication failed for 'https://github.com/acme/private.git/'",
            "collect: git fetch failed: remote: Authentication failed for 'https://github.com/a/b'",
            "auth failed: authentication failed",
            // buzz AUTH texts that linking does not fix.
            "auth-required: verification failed",
            "wss://r: auth-required: already authenticated",
            "auth-required: agent owner recorded; reconnect",
        ] {
            assert_eq!(with_hint(text), text, "{text}");
        }
    }

    /// Review L1: the hint is attached to sign-in failures, where private receive is broken, so it
    /// must not claim reading is unaffected.
    #[test]
    fn the_hint_says_private_messages_and_payments_need_the_link() {
        let hinted =
            with_hint("NIP-42 authentication failed (required for kind-1059 p-gated receive)");
        assert!(
            hinted.contains(
                "public reads still work without it; private messages and payments need it"
            ),
            "{hinted}"
        );
        assert!(!hinted.contains("reading still works"), "{hinted}");
    }

    #[test]
    fn the_hint_is_added_once() {
        let once = with_hint("restricted: not a relay member");
        assert_eq!(with_hint(once.clone()), once);
        assert_eq!(once.matches(LINK_COMMAND).count(), 1);
    }

    #[test]
    fn the_mint_hint_names_the_mint_key() {
        let hinted = with_hint_for("authentication failed", MINT_LINK_HINT);
        assert!(
            hinted.contains("`maxplayer account link --mint`"),
            "{hinted}"
        );
    }
}
