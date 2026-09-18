# Nova status page — design notes

## What I designed

The page is a single, offline `status.html` with its CSS and tiny bar-generation script inline. It has no dependencies, build step, framework, font download, or external network request.

The reading order follows the urgency of a status visit:

1. A plain-language current verdict: “Everything is working normally.”
2. Four component cards with explicit status text, uptime percentage, and a compact 90-day strip.
3. The two latest resolved incidents, with title, summary, start time, and duration.
4. A final refreshed timestamp.

The wide layout uses two columns for component comparison and a single incident list. At 390 CSS pixels it becomes one column, preserves comfortable side margins, and places incident timing beneath the summary. The final mobile capture shows no horizontal overflow or clipped content.

## Token use

Only values from `design-identity/tokens.css` are used for the visual system.

- `paper-100` is the page ground; `paper-000` is the high-clarity card surface; `paper-300` and `ink-100` provide separators and boundaries.
- `ink-900` is primary copy. `ink-600` is supporting text, using the palette’s measured AA-safe body-text pairing on white.
- `success-600` / `success-700` reinforce “Operational” and “Resolved.”
- `warn-600` and `danger-600` mark the exceptional uptime bars that correspond to the incident history.
- `accent-100` supplies the restrained Nova ring motif, and `accent-700` marks the refresh symbol.
- The supplied type scale, spacing scale, radii, monospace family, and `shadow-sm` establish hierarchy without introducing parallel values or effects.

## Accessibility decisions

Status never depends on color. Every live component includes a check symbol and the word “Operational”; the top-level state includes a check and the full phrase “All systems operational”; incidents are explicitly labeled “Resolved.” Uptime history is also stated as a numeric percentage and summarized in screen-reader-only prose. Red and amber bars are supplementary history cues, not the only carriers of meaning.

The source order matches the visual order. Semantic `header`, `main`, `section`, `article`, headings, lists, and `time` elements make the page navigable and understandable with a screen reader. Decorative branding, the ring motif, and visual bar strips are hidden from the accessibility tree. Each uptime strip instead has a concise spoken summary, avoiding 120 repetitive bar announcements.

All foreground/background pairings follow the supplied contrast ledger. Axe’s one inconclusive `color-contrast` check was manually inspected: it targets only the `aria-hidden` check glyph inside the status icon. That glyph uses the ledger’s `paper-000` on `success-600` pair, measured at 6.02:1. It is also redundant with visible and accessible status text.

## Final verification

Final screenshot run:

- Desktop: **1280 × 1351 px**, 1,471 distinct colors, 4.68% ink.
- Mobile: **780 × 3886 px** (390 CSS px at 2×), 1,928 distinct colors, 7.59% ink.

Final axe-core 4.11.4 audits:

- Desktop: **0 violations**, 0 flagged nodes (63 rules considered).
- Mobile: **0 violations**, 0 flagged nodes (63 rules considered).

Both runs passed the audit tool’s document-title, DOM-size, text-length, stylesheet, HTTP-status, and rules-considered load proof.
