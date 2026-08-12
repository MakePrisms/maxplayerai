# Implementation status

[`protocol-v1.md`](protocol-v1.md) defines the v1 wire. This file records where the code stands
against it today.

Every row was verified by reading the tree at the cited line. A row leaves this file when the code
matches the spec.

## Trade events

| Spec | v1 says | Code today | Issue |
|---|---|---|---|
| §6.5 | `ACCEPT` carries a `["job-hash", hash]` tag and a reply-marked result `e` tag. | It carries neither. `accept_draft` builds the offer `e` tag, an unmarked claim `e` tag, and two `p` tags — `gateway.rs:480`. `status_draft` adds only `status`, `t`, and `v` — `gateway.rs:824`. | [#640](https://github.com/MakePrisms/maxplayerai/issues/640) |
| §6.5 | A reader resolves the three `e` tags by marker. | `parse_offer_and_claim_tags` reads the `root`-marked tag as the offer and the first unmarked tag as the claim — `gateway.rs:534`. It has no result branch. | [#640](https://github.com/MakePrisms/maxplayerai/issues/640) |
| §6.7 | `FEEDBACK` `status` is one of `progress`, `claim_released`, `refusal`, or `error`. | Only `error` is emitted. `error_draft` sets it (`gateway.rs:712`), and the other three literals appear nowhere in `crates/`. | — |
| §7.1 | Seven reason codes are defined. | Four are emitted. `unsupported_version`, `mint_incompatible`, and `at_capacity` are constructed nowhere outside the enum that defines them (`gateway.rs:660`). The buyer reader already handles all three (`buyer/mod.rs:2357`). | — |
| §2.1 | A reader MUST reject an event whose `v` is not `1`. | Enforced at `parse_offer` (`gateway.rs:304`) and `parse_heartbeat`. The award, accept, and result parsers gate on kind. The offer gate covers trade entry, so this is defence in depth rather than an open hole. | — |

## Delivery

| Spec | v1 says | Code today | Issue |
|---|---|---|---|
| §8.1 | An implementation MUST assert a parent count of one in contribution mode, and zero in greenfield mode. | No production path asserts it. The buyer verifies descent and tip-match instead — `delivery_git.rs:254`, `delivery_git.rs:308`, `delivery.rs:93`. The only `parent_count()` assertions are tests — `seller_git.rs:952`, `seller_git.rs:1049`, both inside the `#[cfg(test)]` module that opens at `seller_git.rs:804`. | — |

## Verification checks and rejection

The checks declaration and environment-provisioning path is wired into contribution jobs. Running
the declared checks and producing or verifying their attestation remain absent from production.

| Spec | v1 says | Code today | Issue |
|---|---|---|---|
| §9.1 | A reader reads `.maxplayer/checks.toml` from the pinned base and validates it. | The seller's contribution-workdir path inspects the pinned base commit. If the declaration blob is absent, it returns `Ok(())` and silently skips provisioning. If present, `capture_job_checks` calls `parse_declaration` and `validate_against_base`; the path then calls `env_lock_ref` and records the declaration and resolved reference — `seller_node/run.rs:1739`. | — |
| §9.2 | A checked delivery carries an attestation the buyer verifies. | No production path calls `render_attestation` or `parse_attestation`. | — |
| §9.1 | The runner resolves an environment backend and runs declared commands with the required network posture. | The seller calls `resolve_backend` and `provision`: Nix provisioning warms the dev shell with `true`; container provisioning pulls and verifies the pinned digest; and `argv_prefix` produces the checks-posture prefix. Provisioning failures become `execution_failed` feedback with detail `env_unprovisionable` through `refusal_feedback` — `seller_node/run.rs:1751`, `seller_node/run.rs:4767`. No production path runs the declaration's `prepare` or `commands` arrays. | — |
| §10 | A buyer publishes `REJECT` on a deterministic verification failure. | No production path publishes it. `reject_draft` exists (`gateway.rs:725`) and its only caller is its own unit test (`gateway.rs:986`). | — |

The execution sentinel in §8.2 is a separate mechanism, and it **is** enforced. A buyer refuses to
spend on a delivery that carries no sentinel bound to that job (`authorize_pay.rs:441`).

## Documentation

| Item | State | Issue |
|---|---|---|
| Section citations in Rust comments | 55 comments across `crates/` cite section numbers from an earlier layout of `protocol-v1.md`, such as `§19` for the execution sentinel. The restructure changed that numbering. Updating the comments touches `.rs` files. | — |
