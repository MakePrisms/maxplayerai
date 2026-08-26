//! `maxplayer sandbox-reap --seat <hex>` — reap the containment holders a RETIRED seat left behind.
//!
//! The boot reaper (#876) removes only holders carrying the BOOTING seat's own key, because on a
//! host running several seller daemons ownership is the one thing that makes a removal safe. That
//! leaves a gap it cannot close by itself: a seat that never boots again never reaps, so its holders
//! survive forever (#905).
//!
//! ── Why this is an operator command and not automatic ────────────────────────────────────────────
//! The missing fact is not measurable on the host. "This seat is retired" and "this seat is slow to
//! start" produce identical evidence — a labelled holder with nothing attached — and no local query
//! separates them. The operator is the only party who holds that fact, so the operator supplies it,
//! by naming the seat. The seat id is therefore EVIDENCE, not a parameter:
//!
//!   * There is no default and no fallback to the local identity. A seat id nobody typed is a seat
//!     nobody vouched for, and the boot path already covers the local seat.
//!   * `--all` is REFUSED, with a message, rather than left unrecognised. See [`parse_options`].
//!   * Nothing here reads config or identity, so a boot has nothing to invoke it with. That is the
//!     "never at boot" constraint held structurally rather than by a comment.
//!
//! ── The asymmetry that sets the whole tone ───────────────────────────────────────────────────────
//! A wrong reap kills a live job at launch and costs a real seat its award and a reputation record.
//! The leak it fixes costs one container and one namespace — disk, not money. The failure is far
//! more expensive than the bug, so every ambiguity here resolves toward refusing rather than
//! removing, and `--dry-run` exists so the expensive direction can be inspected before it is taken.
//!
//! ── One reap predicate, one place ────────────────────────────────────────────────────────────────
//! This module adds NO selection logic. Both legs — ownership match AND idle namespace — stay in
//! `sandbox_netns::reapable_holders`, reached through `reapable_holders_live`; the removal stays in
//! `sandbox_netns::reap_orphans`. `--dry-run` calls the SAME selection and stops before removal, so
//! it is a second output, not a second predicate. A copy of the selection here is how the host-wide
//! predicate #876 removed would find its way back in.

use std::io::Write;

const SUCCESS: i32 = 0;
const USAGE_ERROR: i32 = 1;
const RUNTIME_ERROR: i32 = 2;

/// A seller public key hex: 32 bytes, lowercase, as `home::public_key_hex` renders it and as the
/// holder's seat label carries it. The label comparison in `reapable_holders` is an exact string
/// match, so a value in any other shape cannot match a holder and is a typo, never a narrower reap.
const SEAT_HEX_LEN: usize = 64;

/// Flags asking to reap every seat on the host, in each spelling worth naming. Refused, not ignored.
const HOST_WIDE_FLAGS: [&str; 3] = ["--all", "--all-seats", "--every-seat"];

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ReapOptions {
    /// The retired seat's public key hex, exactly as the operator typed it.
    pub(crate) seat: String,
    /// Select and print; remove nothing.
    pub(crate) dry_run: bool,
}

/// Help that was asked for goes to stdout and succeeds (issue #570).
fn write_usage(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "Usage:\n  maxplayer sandbox-reap --seat <64-hex> [--dry-run]\n\nRemove the containment holders left behind by a RETIRED seat, which that seat can no longer reap\nfor itself because it never boots again. You name the seat: only you can know it is retired, and\nthe host cannot tell a retired seat from one that is merely slow to start.\n\n  --seat <64-hex>   the retired seat's public key hex. Required; there is no default.\n  --dry-run         print what would be removed and remove nothing.\n\nA holder is removed only when it is BOTH labelled with that seat AND has no container joined to its\nnamespace. Exit codes: 0 success, 1 usage error, 2 runtime error."
    );
}

/// Parse the argv tail. Every rejection is a refusal with a reason, never a silent narrowing.
///
/// **`--all` and its spellings are refused explicitly.** They are not merely absent, and the
/// difference is the point. The seat id is the operator's evidence that a seat is retired; nobody is
/// in a position to know that every OTHER seat on the host is retired, and the host cannot check it —
/// the claim is false the instant a co-tenant is booting. Host-wide selection is precisely the
/// predicate #876 removed from this code path. Refusing it by name teaches at the moment of the
/// mistake, and means a maintainer who wants the flag must DELETE a deliberate refusal rather than
/// fill a gap that reads like an oversight.
///
/// `--seat` is required, takes a 64-character lowercase hex value, and may appear once. The shape
/// check is what stops `--seat --all` swallowing a flag as its value, and what stops a truncated key
/// silently matching no holder while the command reports success. Repetition is an error rather than
/// last-one-wins: two seats named means the operator's intent is unknown, and this is a command where
/// an unknown intent must not be guessed at.
pub(crate) fn parse_options(args: &[String]) -> Result<ReapOptions, String> {
    let mut seat: Option<String> = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            flag if HOST_WIDE_FLAGS.contains(&flag) => {
                return Err(format!(
                    "refusing {flag}: this command reaps ONE retired seat, and you name it. \
                     The seat id is your evidence that the seat is retired — nobody can know that \
                     every other seat on this host is retired, and the host cannot check it (a \
                     co-tenant that is merely booting looks exactly the same). \
                     Reap one seat at a time: --seat <64-hex>"
                ));
            }
            "--seat" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    "--seat requires the retired seat's 64-character lowercase hex public key"
                        .to_owned()
                })?;
                if seat.is_some() {
                    return Err(
                        "--seat was given twice: name one retired seat per run, so that what is \
                         being reaped is never in doubt"
                            .to_owned(),
                    );
                }
                // Lowercase-only on purpose: `is_ascii_hexdigit` would accept an upper-case key,
                // which the exact label comparison then matches against nothing.
                if value.len() != SEAT_HEX_LEN
                    || !value.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
                {
                    return Err(format!(
                        "--seat must be exactly {SEAT_HEX_LEN} lowercase hex characters (a seller \
                         public key), got {value:?}. The seat label is matched exactly, so any other \
                         shape reaps nothing while looking like it worked."
                    ));
                }
                seat = Some(value.clone());
            }
            // Repetition is tolerated here and refused for `--seat`, deliberately: a second
            // `--dry-run` asks for the same safe thing, whereas a second seat leaves it unclear
            // WHAT is being removed.
            "--dry-run" => dry_run = true,
            other => {
                return Err(format!(
                    "unknown sandbox-reap option: {other}\nusage: maxplayer sandbox-reap --seat <64-hex> [--dry-run]"
                ));
            }
        }
        index += 1;
    }

    // No fallback to this seat's own identity. A boot already reaps the local seat, and a seat id
    // nobody typed is a seat nobody vouched for.
    let seat = seat.ok_or_else(|| {
        "--seat is required: name the RETIRED seat whose holders you are reaping.\nusage: maxplayer \
         sandbox-reap --seat <64-hex> [--dry-run]"
            .to_owned()
    })?;
    Ok(ReapOptions { seat, dry_run })
}

/// `maxplayer sandbox-reap --seat <hex> [--dry-run]`.
pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    // #570: a sole `--help` prints usage to STDOUT and exits 0, before any parse and before docker
    // is reached at all.
    if crate::cli::is_help_request(args) {
        write_usage(out);
        return SUCCESS;
    }
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            return USAGE_ERROR;
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(err, "sandbox-reap runtime: {error}");
            return RUNTIME_ERROR;
        }
    };

    if options.dry_run {
        // The same selection the real reap uses, printed instead of acted on.
        match runtime.block_on(maxplayer_core::sandbox_netns::reapable_holders_live(&options.seat)) {
            Ok(holders) if holders.is_empty() => {
                let _ = writeln!(
                    out,
                    "dry run: no reapable containment holders for seat {}; nothing was removed",
                    options.seat
                );
                SUCCESS
            }
            Ok(holders) => {
                for holder in &holders {
                    let _ = writeln!(out, "would reap {holder}");
                }
                let _ = writeln!(
                    out,
                    "dry run: {} containment holder(s) would be removed; nothing was removed",
                    holders.len()
                );
                SUCCESS
            }
            Err(error) => {
                let _ = writeln!(err, "{error}");
                RUNTIME_ERROR
            }
        }
    } else {
        match runtime.block_on(maxplayer_core::sandbox_netns::reap_orphans(&options.seat)) {
            Ok(reaped) if reaped.is_empty() => {
                let _ = writeln!(
                    out,
                    "no reapable containment holders for seat {}",
                    options.seat
                );
                SUCCESS
            }
            Ok(reaped) => {
                for holder in &reaped {
                    let _ = writeln!(out, "reaped {holder}");
                }
                let _ = writeln!(
                    out,
                    "reaped {} containment holder(s) left by seat {}",
                    reaped.len(),
                    options.seat
                );
                SUCCESS
            }
            Err(error) => {
                let _ = writeln!(err, "{error}");
                RUNTIME_ERROR
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_owned()).collect()
    }

    fn seat_hex() -> String {
        "a".repeat(SEAT_HEX_LEN)
    }

    /// The refusal, at the parser rather than at dispatch: every spelling of "every seat on this
    /// host" is an error that NAMES itself and says why. `cli.rs` holds the same property end to end.
    #[test]
    fn host_wide_flags_are_refused_with_a_reason_in_every_spelling() {
        for flag in HOST_WIDE_FLAGS {
            let error = parse_options(&argv(&[flag])).expect_err("must refuse");
            assert!(error.contains(flag), "must name the flag it refuses: {error}");
            assert!(error.contains("retired"), "must say why: {error}");
            // And the refusal does not become permission when a seat is also named.
            let with_seat = parse_options(&argv(&["--seat", &seat_hex(), flag]));
            assert!(with_seat.is_err(), "{flag} after --seat must still refuse");
        }
    }

    /// #905: no default, and no fallback to the local identity. A seat id nobody typed is a seat
    /// nobody vouched for.
    #[test]
    fn seat_is_required_and_never_defaulted() {
        let error = parse_options(&argv(&[])).expect_err("must refuse");
        assert!(error.contains("--seat"), "{error}");
        let error = parse_options(&argv(&["--dry-run"])).expect_err("must refuse");
        assert!(error.contains("--seat"), "{error}");
        // A value-less `--seat` is a refusal, not an empty seat — an empty seat would match every
        // holder whose label failed to parse.
        assert!(parse_options(&argv(&["--seat"])).is_err());
    }

    /// The shape check is what stops a flag being swallowed as the seat value: `--seat --all` must
    /// not become a reap of a seat literally named "--all", nor quietly reach the host-wide path.
    #[test]
    fn a_flag_is_never_accepted_as_the_seat_value() {
        for value in ["--all", "--dry-run", "-x"] {
            let error = parse_options(&argv(&["--seat", value])).expect_err("must refuse");
            assert!(error.contains("hex"), "{error}");
        }
    }

    /// A truncated or upper-case key matches no holder while looking like a successful run. Refuse it
    /// rather than report a reap of nothing.
    #[test]
    fn a_seat_that_cannot_match_a_label_is_refused_not_run() {
        for value in ["abc", "A".repeat(SEAT_HEX_LEN).as_str(), "", "z".repeat(SEAT_HEX_LEN).as_str()] {
            assert!(
                parse_options(&argv(&["--seat", value])).is_err(),
                "must refuse a seat that cannot match a label: {value:?}"
            );
        }
        assert!(parse_options(&argv(&["--seat", &"0123456789abcdef".repeat(4)])).is_ok());
    }

    /// Two seats named means the intent is unknown. This is not a command that guesses.
    #[test]
    fn a_repeated_seat_is_an_error_not_last_one_wins() {
        let other = "b".repeat(SEAT_HEX_LEN);
        let error = parse_options(&argv(&["--seat", &seat_hex(), "--seat", &other]))
            .expect_err("must refuse");
        assert!(error.contains("twice"), "{error}");
    }

    #[test]
    fn a_well_formed_invocation_parses_to_that_seat() {
        let seat = seat_hex();
        assert_eq!(
            parse_options(&argv(&["--seat", &seat])).expect("parses"),
            ReapOptions { seat: seat.clone(), dry_run: false }
        );
        assert_eq!(
            parse_options(&argv(&["--seat", &seat, "--dry-run"])).expect("parses"),
            ReapOptions { seat: seat.clone(), dry_run: true }
        );
        // No positional seat: the id arrives through the named flag or not at all.
        assert!(parse_options(&argv(&[&seat])).is_err());
    }

    /// A sole `--help` answers from usage before any parse, so it cannot be turned into a reap.
    #[test]
    fn help_prints_usage_and_takes_no_action() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&argv(&["--help"]), &mut out, &mut err);
        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(code, SUCCESS);
        assert!(text.contains("maxplayer sandbox-reap"), "{text}");
        assert!(text.contains("--dry-run"), "{text}");
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }
}
