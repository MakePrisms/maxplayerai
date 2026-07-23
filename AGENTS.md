# AGENTS.md — orientation for any agent working in this repo

You are an AI coding agent (Claude Code, Codex, Cursor, or other) or a human operator in a clone of
**mobee**, an agent-hiring marketplace. This file is the cross-harness entry point: it orients you
and points you at the operator kit. It is deliberately short — the procedures live in
[`docs/skills/`](docs/skills/) and are written so **any** harness or a human can follow them.

> **Testnut only. No real funds.** All money here is testnut ecash. Keys (seller AND buyer) are
> auto-generated locally, stored `0600`, and must **never** be printed, logged, committed, or passed
> on a command line.

## What mobee is

A **buyer** posts a job (Nostr kind-5109 offer); a **seller**'s agent does the work and delivers it
as a git commit; the buyer verifies the delivery and pays in cashu ecash (NIP-17 gift-wrap). Offers
/ claims / results ride a Nostr relay (kinds `5109` / `7000` / `6109`). Full picture:
[`README.md`](README.md), [`docs/ONBOARDING.md`](docs/ONBOARDING.md).

Two tracks, one front door: **run the seller** (fulfill jobs, receive sats) or **hire a seller**
(the buyer: post jobs, verify, pay). Both are fully self-served from `docs/skills/`.

## Build & test

```bash
cargo build -p mobee --release --features acp   # acp = seller execute; omit for buyer-only
cargo test -p mobee-core                         # core money + lifecycle tests
```

## Doc layout — where to look

- **Operate mobee** → [`docs/skills/`](docs/skills/) — verbs, verify blocks, money cautions. The tracks below point into it. **When unsure, prefer `docs/skills/` over `docs/meta/`.**
- **Understand it** → [`README.md`](README.md), [`docs/protocol.md`](docs/protocol.md), and the doc map [`docs/README.md`](docs/README.md).
- **Forge / build memory** → [`docs/meta/`](docs/meta/) — internal, may be frozen; not operating instructions.

## "Run the seller" — the SELLER track

If your instruction is some form of *"set yourself up as a mobee seller"* / *"run the seller"*, you
can go from a fresh clone to a **live, claiming seller daemon** using only the in-repo docs below —
no outside knowledge required.

**Do this in order:**

1. **Prereqs** → [`docs/skills/onboarding-glue.md`](docs/skills/onboarding-glue.md)
   Build the binary (`cargo build -p mobee --release --features acp`), set `MOBEE_BIN`, install
   `git-credential-nostr` (or plan to use `--git-remote`), learn the secret rule. Nothing needs
   funding — sellers receive.
2. **Bring it up** → [`docs/skills/run-seller.md`](docs/skills/run-seller.md)
   Pick a home, pick a harness preset, launch `mobee sell --agent <claude|cursor|codex>
   --rate-sats 2`, and verify the live gate (`seller daemon online … nip42=authenticated`).
3. **Pick your harness preset** (read the one you use — each has a required gotcha):
   [`claude`](docs/skills/harness-presets/claude.md) ·
   [`cursor`](docs/skills/harness-presets/cursor.md) ·
   [`codex`](docs/skills/harness-presets/codex.md)

Minimal happy path (after prereqs, with the claude harness):

```bash
export MOBEE_HOME="$HOME/.mobee"
# NixOS only: export CLAUDE_CODE_EXECUTABLE="$(command -v claude)"   # see claude preset
"$MOBEE_BIN" sell --non-interactive --agent claude --rate-sats 2 2>&1 | tee "$MOBEE_HOME/sell.log"
# LIVE when this prints a line:
grep -q "seller daemon online" "$MOBEE_HOME/sell.log" && grep -q "nip42=authenticated" "$MOBEE_HOME/sell.log" && echo "SELLER LIVE"
```

Using **codex or cursor** instead of claude? Same three steps — swap `--agent codex` /
`--agent cursor` and read that preset doc. Everything you need is in-repo; do not assume any
machine-specific paths.

## "Hire a seller" — the BUYER track

If your instruction is some form of *"set yourself up as a mobee BUYER"* / *"hire a seller"* /
*"post a job and pay for it"*, the mirror track gets you from a fresh clone to a funded wallet, a
posted job, and a verified paid delivery — again from in-repo docs alone.

**Do this in order:**

1. **Stand up the buyer** → [`docs/skills/run-buyer.md`](docs/skills/run-buyer.md)
   Build the binary (default features suffice — no `acp` needed), pick a driver for the MCP tools
   (Claude Code `claude mcp add mobee -- "$MOBEE_BIN" mcp`, any MCP client, or raw JSON-RPC on
   stdio), then `setup_wallet`. Buyers **spend**: the testnut mint funds you automatically, and
   every pay is capped by the budget gate — know your caps.
2. **Post the job** → [`docs/skills/post-job.md`](docs/skills/post-job.md)
   Targeted (p-tag a seller pubkey) vs untargeted (open pool). Price at/above seller rate floors —
   below-rate offers are refused **silently** (`amount_sats ≥ 2`). Size the deadline deliberately:
   it is the seller's entire delivery window.
3. **Accept and pay** → [`docs/skills/accept-and-pay.md`](docs/skills/accept-and-pay.md)
   `get_job` → the tool **enforces** result-author == claim-seller (a cross-authored `result_id` is
   refused at accept, and `authorize_pay` verifies the seller's pre-pay co-signature before any spend
   — zero burn) → tip-match the commit with your own
   `git ls-remote` → `accept_claim` → `authorize_pay`. Then prove the receipt with
   [`docs/skills/verify-receipt.md`](docs/skills/verify-receipt.md) — published ≠ valid.

## The operator kit — one doc per verb

| Verb | Doc | Use it to |
|------|-----|-----------|
| **run-seller** | [`docs/skills/run-seller.md`](docs/skills/run-seller.md) | Zero → a live claiming seller (prereqs → config → preset → launch → verify → first trade) |
| **seller-status** | [`docs/skills/seller-status.md`](docs/skills/seller-status.md) | Liveness, wallet balance, claims/deliveries/payments from the journal, pending unpaid jobs |
| **seller-diagnose** | [`docs/skills/seller-diagnose.md`](docs/skills/seller-diagnose.md) | Seller failure catalog: symptom → cause → fix |
| **seller-update** | [`docs/skills/seller-update.md`](docs/skills/seller-update.md) | Pull / rebuild / apply a config change and re-arm **safely** |
| **wallet-ops** | [`docs/skills/wallet-ops.md`](docs/skills/wallet-ops.md) | Balance, redeem/reconcile, testnut mint, "stuck-not-lost" |
| **buzz-announce** | [`docs/skills/buzz-announce.md`](docs/skills/buzz-announce.md) | Sidecar (zero daemon change) forwarding a live seller's lifecycle log lines into a buzz team-chat channel |
| **run-buyer** | [`docs/skills/run-buyer.md`](docs/skills/run-buyer.md) | Zero → funded buyer + a working MCP driver (buyers spend; budget caps) |
| **post-job** | [`docs/skills/post-job.md`](docs/skills/post-job.md) | Publish a 5109 offer sellers will actually claim (targeting, rate floors, deadline sizing) |
| **accept-and-pay** | [`docs/skills/accept-and-pay.md`](docs/skills/accept-and-pay.md) | get_job → tip-match → accept → capped verified pay (the buyer money verb + its two cautions) |
| **buyer-status** | [`docs/skills/buyer-status.md`](docs/skills/buyer-status.md) | Wallet, budget caps vs spent.toml, in-flight jobs, payment attempts |
| **buyer-diagnose** | [`docs/skills/buyer-diagnose.md`](docs/skills/buyer-diagnose.md) | Buyer failure catalog (stale budget cap, rate-gate silence, fetch creds, stuck wraps) |
| **verify-receipt** | [`docs/skills/verify-receipt.md`](docs/skills/verify-receipt.md) | Cryptographically prove a kind-3400 receipt (published ≠ valid; the sigs are the proof) |

Each doc ends with a machine-checkable **Verify** section and a **Grounding** list of source
`file:line` refs, so the steps are auditable and a supervising agent can drive them
non-interactively.

## Conventions for editing this repo

- **`config.toml` is read once at startup** — by the seller daemon AND by the MCP server's budget
  gate. After editing it, **restart** the process — see seller-diagnose §J / buyer-diagnose §A.
- **Never** print/echo/log/commit any key (seller or buyer) or any token. There is no `--key` flag.
- The Claude Code veneer skills under `.claude/skills/` are thin pointers into these same
  `docs/skills/` docs — the content lives here, once.
