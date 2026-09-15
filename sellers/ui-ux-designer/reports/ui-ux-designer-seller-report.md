# UI/UX designer seller — build report

- **Worker:** `w-ui-ux-designer-seller` (forge worker, channel `#ui-ux-designer-seller`)
- **Ordering seat:** maxie, on Damian's authorization. Folded by hearth.
- **Worktree:** `~/forge/v2/wt/w-ui-ux-designer-seller`
- **Branch:** `w/ui-ux-designer-seller`
- **Base:** `d7b94db2dbb7aeeefdcbb087edd0c90df56a8bdb` ("release: cut v0.5.8 (#984)"), resolved **by
  URL** from `https://github.com/MakePrisms/maxplayerai.git`. On the Studio checkout
  (`/Users/forge/forge/work/maxplayerai`) that URL is the remote labelled `upstream`; `origin`
  there is the fork `maxy-player/maxplayerai`, whose local `main` was stale at `91e5946`. I fetched
  and confirmed the live head rather than trusting the label.
- **Package:** `sellers/ui-ux-designer/`
- **Date:** 9 September 2026 (UTC)

## 1. The gate

```bash
cd sellers/ui-ux-designer
npm ci
bash scripts/smoke.sh
```

The generative check spends a **real model turn** and takes several minutes. That is deliberate:
a gate that only checks a deterministic rewrite is exactly the defect this package was corrected
for. It also needs a `maxplayer` binary built with the `acp` feature (see §2.1).

**Verified both directions, first-hand:**

- full gate — `exit 0`, **"9 checks run, 0 failed"**, `SMOKE GATE PASSED`
- sabotaged job (baseline and candidate pointed at the same page) — `exit 1`, "7 checks run,
  2 failed", failing with:
  `baseline reported only 0 violations, expected at least 4 — the accessibility check is probably
  not exercising the page`

A gate that cannot fail proves nothing, so the sabotage run is part of the evidence, not an aside.

What the gate does, in order:

1. **Environment.** Node floor check, then it *launches* Chromium and prints the version the
   running browser reports. Not `which`, not a file-exists test.
2. **Contrast.** Regenerates `design-identity/contrast.md` from `tokens.json` with the WCAG
   relative-luminance formula. 13 contracted pairs, all passing; a miss exits nonzero.
3. **Image pipeline.** Loads sharp and asserts libvips has SVG and PNG support compiled in.
4. **The generative path — the heart of the gate.** Hands `briefs/nova-status-brief.md`, a
   natural-language brief with **no candidate implementation supplied**, to a real model harness
   through the **actual maxplayer local driver**. The model must author the page itself, render it,
   read its own screenshots and audit output, and iterate to zero accessibility violations at both
   viewports. See §2.1.
5. **The review pipeline, as tooling only.** Spawns `agent/designer-agent.mjs` over ACP stdio —
   `initialize` → `session/new` → `session/prompt`, requiring `stopReason: "end_turn"`. It is
   retained and explicitly demoted: it inspects a supplied candidate and is never allowed to stand
   in for a designer.
6. **External verification.** `tools/verify-manifest.mjs` re-reads all 11 artifacts from disk and
   recomputes each sha256. The agent's own digests are not trusted; a manifest self-certified by
   the process that wrote the files is a receipt, not evidence.
7. **Four negative controls, each of which must exit nonzero** (and did):
   - an empty page that axe scores as **0 violations** is rejected by the load-proof guard;
   - a 404 URL yields no screenshot;
   - two identical images fail a required-change diff;
   - a generative run whose `status.html` has been deleted is **not** reported as a pass — proving
     the generative check reads artifacts off disk rather than trusting its own summary.

## 2. Harness access — verified, not assumed

### 2.1 The model harness and the maxplayer driver

The correction required integrating a supported coding/model harness using the **existing
authorized runtime only**, with a concrete BLOCKED if none was available. Presence on `PATH` is not
availability, so each adapter was *driven* over ACP stdio and judged on whether a model answered:

| Adapter | Handshake | Session | Model answered | Verdict |
|---|---|---|---|---|
| `claude-agent-acp` | protocol 1 OK | OK | **no** | refused: `errorKind: rate_limit`, org monthly spend limit — reproduced twice, an hour apart |
| `codex-acp` | protocol 1 OK | OK | **yes** | **used** — `gpt-5.6-sol`, `stopReason: end_turn`, non-zero output tokens against a real quota |
| `claude-code-acp`, `cursor-agent`, `cursor-agent-acp`, `goose` | — | — | — | absent from `PATH` |

So no BLOCKED: an authorized harness is available, and the gate depends on `codex-acp`, not on the
spend-limited one. Full detail in `evidence/RUNTIME.md`.

The harness is not spoken to directly. It is driven through the **actual maxplayer local driver**,
built from this branch — the stock `/opt/homebrew/bin/maxplayer` (0.1.0-rc.3) refuses this path
outright with `maxplayer run requires rebuilding with the acp feature`:

```bash
cargo build -p maxplayer --features acp --release
# -> maxplayer 0.5.8 (e72e9bd7114fe86412b62ff68a08c28c5602d396)
```

`maxplayer run` is the local driver only: no relay, no wallet, no sats, no deployment.

**What the model actually produced**, from the passing gate run (`runs/generative-smoke/summary.json`,
verdict `PASS`, `failures: []`):

| Evidence | Result |
|---|---|
| Driver | exit 0, `job.execution_changed → completed` |
| Deliverable | `status.html` authored by the model, ~14 KB, not byte-identical to anything shipped here |
| Agent-invoked tools | `shoot.mjs` **true**, `a11y.mjs` **true** — read from the driver's own session updates |
| Accessibility, desktop | **0 violations**, 15 passes, 63 rules considered, axe-core 4.11.4 |
| Accessibility, mobile | **0 violations**, 15 passes, 63 rules considered, axe-core 4.11.4 |
| Load proof | HTTP 200, title "Nova service status", 225 elements |
| Screenshots | desktop 122,207 B and mobile 291,881 B, both content-asserted |
| Design notes | `DESIGN-NOTES.md` written by the model |

**A detector bug worth recording.** The first generative run passed on the merits yet the gate
failed it, claiming the agent "never rendered its design". The driver writes *two* streams and they
carry different things: `events.jsonl` nests its type under `.payload` and holds only
`driver.ready` / `job.execution_changed` / `agent.message`, while the ACP tool calls and the
`turn_ended` stop reason arrive on driver **stdout**. Reading only the first stream produced a
confident false accusation against work that was in fact correct. The gate now reads both, and
`--verify-only` re-checks finished evidence without spending another model turn.

The load-bearing line in the order was "verify actual harness access, not installed-binary
presence". Concretely:

| Capability | How it was proven | Result |
|---|---|---|
| Chromium drivable from the package | `chromium.launch()`, then `browser.version()` | **151.0.7922.34** (Playwright revision 1234) |
| Screenshot readback | captured, decoded with pngjs, pixel statistics asserted | desktop 1280×900, 93,810 B, 1744 distinct colours, 3.83% ink |
| Node/frontend preview | loopback server started, page fetched, HTTP status checked | ephemeral port, 200 |
| axe accessibility | run at both viewports with load proof recorded | baseline 4 violations / 37 nodes; candidate 0 with 29–30 passes |
| Visual comparison | pixelmatch over real captures | desktop 8.51%, mobile 8.25% changed |
| SVG / image handling | sharp rasterised an authored SVG, dimensions and content asserted | 480×192, 4,475 B, 246 colours |

Pinned: `playwright@1.62.0`, `axe-core@4.11.4`, `@axe-core/playwright@4.11.2`, `pixelmatch@6.0.0`,
`pngjs@7.0.0`, `sharp@0.34.5`, with `npm ci` from a committed lockfile. Playwright 1.62.0 was
chosen *because* its Chromium revision (1234) is the one already in this host's cache, so a fresh
`npm ci` downloads no browser.

**Two real pinning defects found and fixed during the build**, both invisible without checking:

1. `@axe-core/playwright`'s loose `playwright-core: ">= 1.0.0"` peer caused npm to hoist a **second
   playwright-core at 1.63.0** beside the pinned 1.62.0. An `overrides` entry now forces one copy.
2. Its `axe-core: "~4.11.3"` dependency installed a **nested axe-core 4.11.4** next to my declared
   4.11.2 — so the engine that actually ran was not the one I had pinned. Both the dependency and
   the override are now 4.11.4, and there is a single copy. This was only visible because the tool
   prints the engine version it used.

## 3. Deliverables

| Item ordered | Where |
|---|---|
| Runnable seller package | `sellers/ui-ux-designer/` |
| One explicit proposed design identity | `design-identity/IDENTITY.md` — "Plainsong", marked PROPOSED in the doc, the token file and the generated CSS |
| Concrete typography / colour / spacing / motion / a11y rules / anti-patterns | IDENTITY.md §§2–7; 12 numbered a11y rules; 18 named anti-patterns |
| Reference corpus with source, version, licensing | `design-identity/REFERENCES.md` — dependency licences read off disk from `node_modules`; WCAG 2.2, APG, GOV.UK, USWDS, M3, Inter/IBM Plex OFL. No fonts bundled, no scraped assets |
| Reproducible pinned tooling | `package.json` + `package-lock.json`, `npm ci` verified |
| Setup / run docs | `README.md`, limitations stated first |
| Concise agent knowledge index | `knowledge/INDEX.md`, including a table of what each tool *refuses* to do |
| Sample redesign source | `samples/redesign/before/index.html`, `samples/redesign/after/index.html` |
| Before/after desktop and mobile screenshots | `evidence/screenshots/` — four captures, all in the manifest |
| Rationale | `samples/redesign/RATIONALE.md` |
| Local smoke command | `scripts/smoke.sh` |
| Evidence manifest | `evidence/MANIFEST.md`, regenerated and externally verified by the gate |

## 4. Design identity summary

**Plainsong** — proposed, not adopted, and inert the moment a buyer supplies their own system.
Near-black ink on near-white paper, one accent, 1.25 type scale off a 16px base, 4px spacing grid,
motion capped at 260ms with 24px maximum travel, `prefers-reduced-motion` compiled into the
generated token block rather than left to memory.

The contrast ledger is generated, never typed. It earned its place immediately: the first border
ink, `#98A2AE`, measured **2.59:1** against paper — under the 3:1 floor for non-text UI in WCAG 2.2
SC 1.4.11. It was darkened to `#868F9C` = **3.27:1**. That failure is documented in IDENTITY.md §3
rather than quietly corrected.

## 5. Sample redesign result

axe-core 4.11.4, tags `wcag2a wcag2aa wcag21a wcag21aa wcag22aa`, both viewports:

- **before:** 4 violations across 37 nodes — `color-contrast` (34 nodes), `html-has-lang`,
  `image-alt`, `button-name`
- **after:** 0 violations, 29 passes desktop / 30 mobile, load proof 103 elements and 1,045
  characters of rendered text

`RATIONALE.md` additionally lists **ten defects axe did not and could not find**, each explained —
most importantly placeholder-as-label, which axe *passes* because a placeholder does supply an
accessible name, and `<div onclick>` controls, which axe cannot flag because a div with no
interactive role is just a div. Those are recorded as hand-reviewed, never folded into the machine
result.

## 6. Evidence manifest

`sellers/ui-ux-designer/evidence/MANIFEST.md` — 11 artifacts, all verified on disk after the run:
4 screenshots, 4 axe JSON reports, 2 diff images, 1 raster. Run recorded as node v26.5.0 /
Chromium 151.0.7922.34 / axe-core 4.11.4 / sharp 0.34.5 with libvips 8.17.3.

`evidence/` also holds artifacts from the manual tool verification I did while building
(`before-*`, `after-*`, `desktop-before-after.png`, `mark-480.png`). Those are genuine outputs but
they are **not** covered by the manifest, which describes only the agent's own run.

## 7. Limitations — stated plainly

1. **The maxplayer binary IS now run, but only its local driver path.** `maxplayer run` is built
   from this branch with `--features acp` and drives the model harness for real. No live relay was
   touched: no wallet, no sats, no mint, no seat, no key. The seller registration, job-claim,
   delivery and payment path remains **entirely unexercised**. This package proves the *agent and
   its tools*; it does not prove the seat earns. That is a real gap, not a rounding error, and it
   follows directly from the hold on real sats in my fold.
1b. **The gate depends on a binary you must build.** `cargo build -p maxplayer --features acp
   --release` is a prerequisite, and the release build is not cheap. The gate fails with a clear
   message rather than silently substituting a fake driver.
2. **Docker sandbox mode unexercised.** Under `[sandbox] mode = "docker"` argv[0] resolves against
   the image's PATH, and the image would need node, this package, and a drivable Chromium. Not
   tested.
3. **The designer is a real model; the deterministic pipeline is now only tooling.** The
   generative path calls `codex-acp` (`gpt-5.6-sol`) through the maxplayer driver and the model
   authors the deliverable. Consequences a buyer should know: output **varies between runs**, the
   gate is not deterministic, each run costs real model tokens on the host's own account, and the
   run takes minutes rather than seconds. `agent/designer-agent.mjs` still only accepts a
   `review-redesign` JSON brief — it is a tool, not the designer.
3b. **The harness is single-supplier in practice.** `claude-agent-acp` is present but refused by a
   standing org spend limit, and cursor/goose adapters are absent, so the gate currently rests on
   `codex-acp` alone. If that account hits its own limit, the generative check cannot run — and it
   will fail loudly rather than fall back to deterministic execution.
3c. **One generative brief, and no human design review of its output.** The model's status page
   passed axe at both viewports and its screenshots were content-asserted, but no human has judged
   whether it is *good*. Zero automated violations is a floor, not taste.
4. **sharp's install script is blocked** by this host's npm allow-scripts policy. The library loads
   and works (libvips 8.17.3, verified), but on a host that needs the build step, `npm ci` alone
   may not be enough.
5. **Automated accessibility checking is a floor**, roughly a third of real issues. Everything else
   in IDENTITY.md §6 is hand review, and is reported as such.
6. **Plainsong is proposed**, unreviewed by any human designer, with no brand authority.
7. **The sample is static HTML** — no framework, no build step, no data layer. A real product
   integration would need work this package does not contain.
8. **Two paths, each narrow**: one generative brief (`briefs/nova-status-brief.md`) and one
   deterministic job type (`review-redesign`).
9. The two negative-control fixtures (`tools/fixtures/blank.html`) and the sabotage job exist to
   test the gate. The sabotage job was run from `/tmp` and its artifacts were removed; only the
   real run's evidence is committed.

## 8. Holds observed

No wallet touched, no sats, no paid service, no new subscription, no production deployment, no
merge, no tag, no push to any branch. Delivery is a local branch in an isolated worktree. Git
identity was set per-command (`git -c user.name=… -c user.email=…`); `git config` was never run, so
no other lane's committer was disturbed.
