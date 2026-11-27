# Implementation status

[`protocol-v1.md`](protocol-v1.md) defines the v1 wire. This file records where the code stands
against it today.

Every row was verified by reading the tree at the cited line. A row leaves this file when the code
matches the spec.

<!-- Re-pin at 6cf28f9bedaf. Two §6.5 ACCEPT rows left: since #697/#718, protocol-v1.md §6.5 specifies two `e` tags (root / unmarked claim) and no job-hash; `accept_draft` (gateway.rs:480) and `parse_offer_and_claim_tags` (gateway.rs:534) match that. No other row qualified. seller_node/run.rs 1739→1837 (`capture_job_checks`), 1751→1840 (`resolve_backend`), 4767→5025 (`refusal_feedback`). gateway.rs:660→659 (`pub enum ReasonCode`). Adjacent, not expanded: `parent_count()` is also asserted in tests/no_system_git.rs:185; 9 comment lines still say §10 for feedback reason codes (now §7.1; current §10 is Rejection). -->

## Trade events

| Spec | v1 says | Code today | Issue |
|---|---|---|---|
| §6.7 | `FEEDBACK` `status` is one of `progress`, `claim_released`, `refusal`, or `error`. | Only `error` is emitted. `error_draft` sets it (`gateway.rs:712`), and the other three literals appear nowhere in `crates/`. | — |
| §7.1 | Seven reason codes are defined. | Four are emitted. `unsupported_version`, `mint_incompatible`, and `at_capacity` are constructed nowhere outside the enum that defines them (`gateway.rs:659`). The buyer reader already handles all three (`buyer/mod.rs:2357`). | — |
| §2.1 | A reader MUST reject an event whose `v` is not `1`. | Enforced at `parse_offer` (`gateway.rs:304`) and `parse_heartbeat`. The award, accept, and result parsers gate on kind. The offer gate covers trade entry, so this is defence in depth rather than an open hole. | — |

## Delivery

| Spec | v1 says | Code today | Issue |
|---|---|---|---|
| §8.1 | An implementation MUST assert a parent count of one in contribution mode, and zero in greenfield mode. | No production path asserts it. The buyer verifies descent and tip-match instead — `delivery_git.rs:254`, `delivery_git.rs:308`, `delivery.rs:93`. The only `parent_count()` assertions are tests — `seller_git.rs:952`, `seller_git.rs:1049`, both inside the `#[cfg(test)]` module that opens at `seller_git.rs:804`. | — |

Container-side delivery is the DEFAULT for a docker seat. `[sandbox] container_delivery` is
`Option<bool>` (`home.rs:680`): absent under `mode = "docker"` means ON, `Some(false)` is the opt-out
back to the host path, and `Some(true)` under `mode = "launcher"` is refused at boot. A launcher seat
is unaffected, because `SandboxMode::Launcher` is the default mode and creates no container. The one
place that reads the default is `SandboxConfig::container_delivery_enabled` (`home.rs:705`). When the
path is the container, `execute_job` calls `deliver_via_container` (`seller_node/run.rs`), and one
container runs the agent and every git step. On the host path the #937 host path runs unchanged. The
seller prints the effective path and its reason at boot (`delivery_path_line`, `seller_node/run.rs`),
and `maxplayer doctor` prints it in the `relay token policy` row. The default moved to the container
path after that path was proven live. On 2026-09-03 one live from-scratch job passed through it
(`fresh-after-agent` token mode) and the buyer paid. The relay canary (`tests/relay_canary.rs`) printed
`A=enforced B=deployed` on 2026-09-07: the relay enforces the token's ref scope and honors its
`expiration` tag (#968), so both token modes work; each ran a paid job that day, as did a contribution
job. The default is ON only for a non-root daemon; a uid-0 seat keeps the host path unless it sets
`container_delivery = true`. Open: #980 (uid separation, which replaces Task B10). Removal of the
interim host push is BLOCKED, not pending: `launcher` is the default sandbox mode, has no container,
and the host push is its only delivery path. The `home.rs` line
numbers are from the commit that moved the default.

## Verification checks and rejection

Production reads a checks declaration only to refuse it on host delivery. Since the RC3-01 hardening
(v0.7.3-rc3) no production path provisions a checks environment, runs the declared checks, or
produces or verifies their attestation.

| Spec | v1 says | Code today | Issue |
|---|---|---|---|
| §9.1 | A reader reads `.maxplayer/checks.toml` from the pinned base and validates it. | On host delivery, `refuse_host_checks_declaration` inspects the pinned base commit after checkout. If the declaration blob is absent, it returns `Ok(())`. If present, `capture_job_checks` calls `parse_declaration` and `validate_against_base` (so a malformed declaration keeps its own refusal), and the job is then refused — `seller_node/run.rs:3068`. Nothing records the declaration: `record_job_checks` (`seller_node/store.rs:1686`) has only a test caller. Container delivery never reads the declaration. | — |
| §9.2 | A checked delivery carries an attestation the buyer verifies. | No production path calls `render_attestation` or `parse_attestation`. | — |
| §9.1 | The runner resolves an environment backend and runs declared commands with the required network posture. | No production path provisions a buyer-declared checks environment (RC3-01). A host-delivery seat skips an offer tagged `param:checks-env` before claiming (`checks_env_claim_skip`, `seller_node/run.rs:3103`); a declaration that still reaches the host workdir is refused before any process is spawned, as `execution_failed` with detail `checks_env_refused_on_host` (a malformed declaration: `env_unprovisionable`) through `refusal_feedback` — `seller_node/run.rs:10820`. Container delivery ignores the declaration. `resolve_backend`, `provision` and the Nix and container provisioning code in `env_provision.rs` are `#[cfg(test)]`: test-only. No production path runs the declaration's `prepare` or `commands` arrays. | — |
| §10 | A buyer publishes `REJECT` on a deterministic verification failure. | No production path publishes it. `reject_draft` exists (`gateway.rs:725`) and its only caller is its own unit test (`gateway.rs:986`). | — |

The execution sentinel in §8.2 is a separate mechanism, and it **is** enforced. A buyer refuses to
spend on a delivery that carries no sentinel bound to that job (`authorize_pay.rs:441`).

## Documentation

| Item | State | Issue |
|---|---|---|
| Section citations in Rust comments | 40 comment lines across `crates/` cite section numbers from an earlier layout of `protocol-v1.md`, such as `§19` for the execution sentinel. Counted as lines whose first non-whitespace is `//` (covers `//`, `///`, and `//!`) matching retired numbers `§7.0`, `§7.5`, `§17`, `§18.1`, `§19`: `git grep -nE '^\s*//.*§(7\.0|7\.5|17|18\.1|19)' -- ':(glob)crates/**/*.rs'`. The restructure changed that numbering. Updating the comments touches `.rs` files. | — |
