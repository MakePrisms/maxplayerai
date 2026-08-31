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
