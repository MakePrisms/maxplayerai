# UI/UX designer seller

A **runnable** maxplayer seller package for UI/UX design work. It is not a persona file: a real
model harness, driven through maxplayer's own local driver, is given a natural-language brief and
**authors the design itself** — then renders it, inspects its own screenshots, and audits its own
accessibility. Around that sit a pinned browser harness, accessibility checks, visual comparison,
image handling, and one smoke gate that proves the whole path works — or fails.

## Read this first: what this does NOT do

- **It has never been run against a live maxplayer relay.** No wallet, no sats, no seat, no mint.
  The seller *registration* path — publishing a profile, claiming a job, delivering, getting paid —
  is entirely unexercised here. Everything below concerns the agent and its tools.
- **The generative path costs real model tokens** on this host's own account, takes minutes, and
  produces **different output every run**. The gate is deliberately not deterministic.
- **It rests on one model harness.** `codex-acp` is what answers here; `claude-agent-acp` is
  installed but refused by a standing org spend limit. If the working account hits its limit the
  generative check fails loudly — it never falls back to faking the designer.
- **No human designer has judged the output.** Zero automated accessibility violations is a floor,
  not taste.
- **It has not been run inside the docker sandbox** (`[sandbox] mode = "docker"`).
- Automated accessibility checking finds roughly a third of real problems. See
  [`design-identity/IDENTITY.md`](design-identity/IDENTITY.md) §6 rule 12.
- The default design identity is **proposed, not adopted**.

## Prerequisites

- **Node ≥ 20** (developed and verified on v26.5.0).
- **A `maxplayer` binary built with the `acp` feature**, for the generative check:
  ```bash
  cargo build -p maxplayer --features acp --release
  ```
  A stock release build refuses the path with `maxplayer run requires rebuilding with the acp
  feature`. The gate locates `target/release/maxplayer` (or `target/debug`), verifies it really has
  the feature, and fails with that message rather than substituting anything.
- **An authorized ACP harness on PATH** — `codex-acp` is what this was verified against. Override
  with `--agent-command <path>`.
- A Playwright-managed Chromium. The lockfile pins `playwright@1.62.0`, whose Chromium is
  **revision 1234 / Google Chrome for Testing 151.0.7922.34**. If that revision is already in your
  `~/Library/Caches/ms-playwright` (or `PLAYWRIGHT_BROWSERS_PATH`), nothing downloads. Otherwise:
  `npx playwright install chromium`.
- No paid service, API key, or subscription is required or accepted.

## Setup and run

```bash
cd sellers/ui-ux-designer
npm ci
bash scripts/smoke.sh          # the gate — exit 0 means everything below is true
```

**Expect this to take several minutes**: the gate spends a real model turn letting the agent design
something. Last verified run: **9 checks run, 0 failed**.

The gate is the documentation that cannot go stale. It launches Chromium and prints its version
(rather than checking a binary exists), regenerates the contrast ledger, checks libvips, **hands a
natural-language brief with no candidate supplied to a real model through the maxplayer local
driver and requires it to author, render and audit its own design**, then drives the deterministic
review pipeline as tooling, re-verifies every produced artifact from outside the agent, and finally
runs four negative controls that must each fail. Any failure exits nonzero.

Run the generative path directly:

```bash
node agent/design-run.mjs --brief briefs/nova-status-brief.md --out runs/mine
node agent/design-run.mjs --brief briefs/nova-status-brief.md --out runs/mine --verify-only
```

`--verify-only` re-checks a finished run's evidence without spending another model turn.

Run the review tooling directly:

```bash
node tools/drive-agent.mjs --brief jobs/sample-review.json
```

## What is in here

| Path | What it is |
|---|---|
| `agent/designer-agent.mjs` | the ACP stdio agent a seller spawns; implements `review-redesign` |
| `tools/drive-agent.mjs` | minimal ACP client — spawns the agent and speaks the handshake |
| `tools/tokens.mjs` | generates `tokens.css` + `contrast.md`; nonzero exit on a contrast miss |
| `tools/preview.mjs` | loopback-only static server rooted at this package |
| `tools/shoot.mjs` | desktop + mobile screenshots that must prove they contain a rendered page |
| `tools/a11y.mjs` | axe-core run guarded by load proof |
| `tools/vdiff.mjs` | pixelmatch comparison with required-change / max-change thresholds |
| `tools/svg.mjs` | sharp SVG rasterising and pipeline check |
| `tools/verify-manifest.mjs` | re-reads and re-digests every artifact after the run |
| `design-identity/` | the Plainsong identity, tokens, measured contrast, licensed corpus |
| `knowledge/INDEX.md` | what the agent reads before working, and what each tool refuses to do |
| `samples/redesign/` | before/after sample plus `RATIONALE.md` |
| `evidence/` | screenshots, axe JSON, diffs, and the verified `MANIFEST.md` |
| `config/seller.config.toml.example` | separate seller home, least privilege, no wallet |

## Pointing a real seller at it

```bash
mkdir -p ~/.maxplayer-ui-ux-designer
cp config/seller.config.toml.example ~/.maxplayer-ui-ux-designer/config.toml
# edit: set the ABSOLUTE path to agent/designer-agent.mjs
MAXPLAYER_HOME=~/.maxplayer-ui-ux-designer "$MAXPLAYER_BIN" seller
```

`MAXPLAYER_HOME` gives this seat its own home so its key and state never sit beside another seat's.
The config file carries **no** mint, wallet, key or rate — those are human decisions, and this
package was built under a hold on real sats.

Note on discovery: `ui-ux-designer` is a *custom* preset, so it publishes **no wire harness
family** — the closed vocabulary is `claude-code | codex | cursor | goose`
(`crates/maxplayer-core/src/agent_presets.rs`). Buyers filtering by family will not match this
seat. They reach it by naming the agent.

## Job brief format

```json
{
  "job": "review-redesign",
  "name": "meridian-invoices",
  "baseline": "/samples/redesign/before/",
  "candidate": "/samples/redesign/after/",
  "expectBaselineViolations": 4,
  "maxCandidateViolations": 0,
  "minChangeRatio": 0.02
}
```

`expectBaselineViolations` is the important one: it asserts the baseline really is as bad as
expected. A baseline that suddenly reports zero problems almost always means the check broke, not
that the page improved — so that case fails the run.

Unknown job types and free-text prompts are refused rather than guessed at.

## Reports

Full write-up, including everything verified first-hand and every limitation:
[`reports/ui-ux-designer-seller-report.md`](reports/ui-ux-designer-seller-report.md).
