# Agent knowledge index

Read in this order. Everything else in the package is generated, evidence, or sample.

## 1. What to design like
- **[`../design-identity/IDENTITY.md`](../design-identity/IDENTITY.md)** — the Plainsong identity.
  PROPOSED, not adopted. Type scale, colour roles, 4px spacing, motion, 12 accessibility rules,
  named anti-patterns. A buyer's own system always overrides it.
- **[`../design-identity/tokens.json`](../design-identity/tokens.json)** — the only hand-edited
  source of design values. `tokens.css` and `contrast.md` are generated from it; never edit those.
- **[`../design-identity/contrast.md`](../design-identity/contrast.md)** — measured contrast for
  every contracted colour pair. Regenerated, never typed.
- **[`../design-identity/REFERENCES.md`](../design-identity/REFERENCES.md)** — every source with
  version and licence. No fonts bundled, no scraped assets.

## 2. What each tool refuses to do
Each tool is written to fail rather than produce a result that merely looks successful.

| Tool | Does | Refuses |
|---|---|---|
| `tools/tokens.mjs` | generates `tokens.css` + `contrast.md` | exits nonzero if any contracted pair misses its minimum |
| `tools/preview.mjs` | loopback static server rooted at the package | serves nothing above the root; no write or upload path |
| `tools/shoot.mjs` | desktop + mobile screenshots | a non-2xx page, a wrong viewport, a blank or single-colour PNG, or one with too few dark pixels to be legible |
| `tools/a11y.mjs` | axe-core over wcag2a…wcag22aa | **any** verdict from a page that fails load proof — empty title, ≤20 elements, ≤200 chars of text, no stylesheet, or ≤50 axe rules considered |
| `tools/vdiff.mjs` | pixelmatch comparison + diff PNG | comparing two blank images; a 0% change when a change was required; resizing mismatched inputs (it crops to the overlap instead) |
| `tools/svg.mjs` | sharp/libvips SVG → PNG | a raster that is blank, zero-byte, or the wrong width; a libvips missing SVG/PNG support |
| `tools/drive-agent.mjs` | spawns the agent and speaks ACP | a failed handshake, an agent error, or any stopReason other than `end_turn` |

## 3. How a job runs
`agent/designer-agent.mjs` is the ACP stdio agent the seller spawns. One job type:
**`review-redesign`**, taking a JSON brief (see `jobs/sample-review.json`). It refuses free-text
prompts and refuses unknown job types rather than improvising.

Turn sequence: check the image pipeline → start the preview server → screenshot baseline and
candidate at both viewports → axe both at both viewports → assert the baseline is *at least* as bad
as the brief expects (a suddenly-clean baseline means the check broke, not that the page improved)
→ assert the candidate is within its violation limit → visual-diff each viewport and require real
change → raster the mark → write a JSON result with a per-artifact sha256 manifest.

## 4. Standing rules for the agent
1. Automated checks are a floor. axe finds roughly a third of real accessibility problems; state
   which of IDENTITY.md §6 you reviewed **by hand** and never let "axe passed" stand in for it.
2. Report the anti-patterns axe cannot see — placeholder-as-label passes axe, because a placeholder
   supplies an accessible name.
3. Never claim a capability you have not exercised in this run. A binary on disk is not access.
4. Every number in a deliverable comes from a file in the manifest.
5. No purchases, no subscriptions, no deployment, no wallet. Missing tools are blockers to report.

## 5. Where evidence lands
`evidence/screenshots/`, `evidence/a11y/`, `evidence/diff/`, `evidence/images/`, and
`evidence/<name>-agent-result.json`. The gate re-verifies every manifest entry's size and digest.
