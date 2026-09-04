# Relay brief — scope-conditional token lifetime (for the buzz-relay agent)

**Audience:** the agent that manages the buzz relay (`buzzrelay.orveth.dev`; source under
`crates/buzz/crates/buzz-relay/`). This is a self-contained work order. The maxplayer client half is
built and inert until these relay changes ship AND are confirmed deployed.

---

## 1. Context (why you are being asked)

maxplayer sellers deliver a hired job as a git push to the relay, authenticated with a **NIP-98**
(`kind:27235`) bearer token the seller signs. The seller is moving ALL git — clone, agent run, commit,
push — INTO the sandbox container, so the host never parses agent-controlled git data. The push
therefore runs inside the container, at the END of a minutes-long agent run.

Only the host holds the seller key, so only the host can mint the token, and it must mint it BEFORE
the container starts. By push time the token is minutes old. The relay's current NIP-98 freshness
check rejects anything older than **±60 s**, so that push would be rejected.

We do NOT want a two-container dance just to keep the token fresh. Instead: let a **branch-scoped**
token live long enough to cover the whole run. That is safe because the scope bounds the blast radius
to almost nothing (see §4). Your job is to make the relay accept a longer life **only** for scoped
tokens.

---

## 2. What the relay must do

### Requirement A — enforce the ref scope (confirm PR #929 already does this)

A NIP-98 token may carry one `["ref", "<refname>"]` tag. On a git push, the relay must enforce that
**every** ref in the push equals that scope, exactly (no glob, no prefix), and reject (403) otherwise.
`<refname>` must be fully qualified (`refs/…`), ≤ 256 bytes, no `..`, no control bytes.

**Action:** confirm PR #929 (`feat/relay-branch-scoped-push-tokens`) implements exactly this and is
merged + deployed. If it is, Requirement A is done — just verify. If anything is missing, complete it.

### Requirement B — allow a longer life for SCOPED tokens (new)

Change the NIP-98 freshness check so the age bound depends on whether the token is scoped:

- **Unscoped token (no valid `ref` tag): UNCHANGED.** Keep the ±60 s window exactly as today.
- **Scoped token (a valid `ref` tag per Requirement A):** replace the 60 s *upper age* bound with an
  **expiration**-based bound:
  - The token carries a NIP-40 `["expiration", "<unix_ts>"]` tag. Accept the token while
    `now <= expiration`.
  - Cap the lifetime: reject if `expiration - created_at > SCOPED_TOKEN_MAX_LIFETIME` (a hard cap, see
    below), so a client bug cannot mint an effectively-permanent token.
  - Still reject a **future-dated** token: `created_at <= now + CLOCK_SKEW` (keep the existing small
    skew, e.g. 60 s).
  - **Fail closed:** a scoped token with NO `expiration` tag gets the normal ±60 s window (do NOT
    grant a long life without an explicit, capped expiration).

`SCOPED_TOKEN_MAX_LIFETIME`: make it a config value. Default should cover the longest job the
marketplace allows — start at **6 hours**. It must be ≥ the maximum job deadline.

Everything else about NIP-98 verification (signature, `u` = repo root, `method`) is unchanged. NIP-98
event-id dedup must stay OFF (one token serves both the info/refs GET and the push POST).

---

## 3. Exact contract (what the client sends)

- `kind: 27235`, signed by the seller.
- `["u", "<relay repo root url>"]`, `["method", "POST"]` — as today.
- `["ref", "refs/heads/maxplayer/<job-prefix>"]` — the single ref this token may push (Requirement A).
- `["expiration", "<unix seconds>"]` — present ONLY on scoped delivery tokens; value = job deadline +
  a small push margin. Absent on unscoped tokens.

The client keeps unscoped tokens (boot probe, fetch legs) exactly as today: no `ref`, no `expiration`,
±60 s.

---

## 4. Security rationale (why a long-lived scoped token is safe, and why unscoped must stay short)

A scoped token authorizes pushing **one ref** to **one repo** (the signed `u`). If it leaks or is
replayed, the most anyone can do is push to the seller's OWN delivery branch on the seller's OWN relay
repo. The buyer fetches the delivery by **commit OID**, named in a separate seller-signed event — not
"whatever is on the branch" — so branch contents do not determine what is delivered. Blast radius ≈
griefing one short-lived branch. A longer life does not change that.

An **unscoped** token authorizes pushing **any** ref the seller can. A long life there WOULD be
dangerous (a leaked token = push anywhere for hours). So the relaxation MUST be conditional on a valid
scope, and the cap MUST bound even the scoped case. This is why Requirement B is gated on Requirement
A: **a long-lived token is only safe because the scope is enforced.** If you relax freshness without
enforcing the scope, you have created a long-lived push-anywhere credential — strictly worse than
today. Ship A and B together, never B alone.

---

## 5. Tests to add

- Scoped token, `created_at` 10 min old, `expiration` in the future, push to the scoped ref → **202/OK**.
- Scoped token, `created_at` 10 min old, **no** `expiration` tag → **rejected** (falls back to 60 s).
- Scoped token, `expiration - created_at` > cap → **rejected**.
- Scoped token, `now` > `expiration` → **rejected** (expired).
- Scoped token, push to a DIFFERENT ref than the scope → **rejected** (Requirement A regression).
- Unscoped token, `created_at` 2 min old → **rejected** (±60 s unchanged).
- Any token, `created_at` far in the future → **rejected** (skew).

---

## 6. Deployment confirmation (blocking)

The PR #929 author could not confirm whether `buzzrelay.orveth.dev` is built from this vendored copy
or a separate `gudnuf/buzz` repo. Before the seller client is switched to long-lived tokens, confirm:

1. The deployed relay includes Requirement A (scope enforcement) — otherwise long-lived tokens are
   push-anywhere.
2. The deployed relay includes Requirement B (this change).
3. Report the commit/tag actually running on `buzzrelay.orveth.dev`.

Until all three are confirmed, the seller stays on the interim config-rewrite fix and short-lived
tokens. Report back with: PR link(s), the config value chosen for `SCOPED_TOKEN_MAX_LIFETIME`, and the
deployed commit.

---

## 7. Additions from the security review

- **(a) Both legs.** The longer-life relaxation must apply to BOTH the `info/refs` GET and the
  service POST of a scoped token — one token serves both legs, so gating only the POST would break the
  advertisement, and gating only the GET would leave the push on ±60 s.
- **(b) Refuse over-cap at MINT, not at push.** When `deadline + margin` exceeds
  `SCOPED_TOKEN_MAX_LIFETIME`, the CLIENT must refuse loudly at mint time (a clear seller-side error),
  not mint a token that the relay later 403s at push — a surprise mid-delivery failure is far worse
  than a refusal up front. State the cap so the client can check it.
- **(c) One tag, read once.** The freshness check and the scope enforcement must read the SAME `ref`
  tag — the FIRST one. Reading different tags (or re-scanning) would let a crafted event pass one check
  under one ref and the other under a different ref.
- **(d) Per-seat canary.** Make the §6 deployment confirmation checkable from a seat, not just an ops
  question: a doctor/canary leg that mints a scoped token and pushes a DIFFERENT ref, expecting a 403.
  A refused push writes nothing, so it is non-destructive, and it turns "is the relay enforcing?" into
  evidence each seller can produce.

**Observation (ties to #863).** Under the driver-in-container design the host no longer sees the ACP
stream, so the per-job roster model refresh loses its source and container-reported usage/model is
spoofable by the job. Since metering already moves to the proxy, have the proxy also record the model
from the API traffic and feed the roster from that — tamper-resistant by the same argument.

---

## 8. Implementation (Requirement B)

Branch `feat/relay-scoped-token-lifetime`. Requirement A stays as PR #929 merged it.

### What shipped

- `crates/buzz/crates/buzz-auth/src/nip98.rs` — a new entry point
  `verify_nip98_event_with_policy(event_json, url, method, body, freshness)`. It returns
  `Nip98Auth { pubkey, ref_scope }`. `Nip98Freshness::STRICT` keeps the ±60 s window.
  `Nip98Freshness::with_scoped_lifetime_cap(cap)` grants the longer life.
  `verify_nip98_event` is now a thin wrapper that passes `STRICT`, so every caller
  outside the git transport keeps today's behaviour.
- The rule, in order: a future-dated token is always rejected (`created_at <= now + 60`).
  A token that is scoped AND carries an `expiration` tag is judged by that expiration
  alone: reject when `expiration - created_at > cap`, reject when `now > expiration`,
  accept otherwise. Every other token gets the ±60 s window. A scoped token with no
  `expiration` tag therefore buys nothing (fail closed).
- One deliberate tightening, also fail closed: on the git legs a `ref` tag that is not a
  valid ref name is now REFUSED (401), not treated as an absent scope. The push path
  refuses to enforce a scope it cannot parse, so honouring such a token would hand a
  wider credential to a client that asked for a narrower one. Non-git callers keep
  ignoring the tag, because `STRICT` never reads it.
- `is_valid_ref_name` moved from `api/git/policy.rs` to `buzz-auth`. One definition now
  decides whether a token counts as scoped and whether the push path will enforce the
  scope, so the two cannot drift.
- `crates/buzz/crates/buzz-relay/src/api/git/transport.rs` — `authenticate_git` passes
  the cap and takes `ref_scope` from the return value. The second scan of the event JSON
  is gone. §7(c) holds by construction: one read of the FIRST `ref` tag, and the value
  the freshness rule used is the value the hook enforces.
- §7(a) holds by construction too. Both legs land in `authenticate_git` — the
  `info/refs` GET through `GitReadAuth` and the service POST through `GitAuth` — and
  neither extractor chooses its own policy.

### The cap

- Config field `Config::scoped_token_max_lifetime_secs`, env var
  `BUZZ_SCOPED_TOKEN_MAX_LIFETIME_SECS`, default **21600** seconds (6 hours).
- NixOS option `services.maxplayer.relay.scopedTokenMaxLifetimeSecs` in `nix/relay.nix`
  wires the same env var.
- `0` switches the relaxation off and puts every token back on ±60 s. A cap of 0 selects the
  strict policy itself, so it is the exact rule from before this change and nothing else.
  This is the rollback switch.
- An explicit env value that is not a whole number of seconds STOPS startup. It never falls
  back to the default, because a mistyped narrower limit must not become the wider default.
  Only an absent variable selects the default.
- A token that asks for more than the cap is rejected outright, not shortened. Raise the
  cap before you raise the job deadline.

### NIP-11

The `limitation` object carries `scoped_token_max_lifetime_secs` (`nip11.rs`,
`RelayLimitation`). The field holds the enforced value and is omitted when the cap is 0.
This is how a client meets §7(b): it reads the cap and refuses an over-cap token at mint
time.

### Not implemented here

- §7(b) is a client change; the relay half is the advertised cap above.
- §7(d) (the per-seat canary) is a seat-side leg and is out of scope for this branch.

### Deployment is still open

Do not treat this as deployed. The relay runs from a separate host repository, and the
NIP-11 document identifies the software only by `software` and `version`. `version` is
the `buzz-relay` crate version (`0.2.0` in this tree), which does not name a commit. Per
§6, `relay.maxplayer.ai` reports `software: https://github.com/block/buzz` and
`version: 0.2.0`, so the running commit cannot be read from this repository. Confirm the
deployed build by another route before the seller client switches to long-lived tokens.
