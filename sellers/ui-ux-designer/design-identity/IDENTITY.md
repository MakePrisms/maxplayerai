# Plainsong — the default design identity

## 0. Status: PROPOSED, not adopted

This is **one explicit proposal**, written down so the seller has a real default instead of
improvising a new look per job. It has not been adopted by any product, has not been reviewed by a
human designer, and carries no brand authority. A buyer may override any token; when a buyer
supplies their own system, theirs wins and this file is inert.

Marked proposed in three places on purpose: here, in `tokens.json` (`$status`), and in the
generated `tokens.css` header. If you find it presented as an adopted house style anywhere, that is
a bug.

Source of truth is `design-identity/tokens.json`. `tokens.css` and `contrast.md` are **generated**
from it by `node tools/tokens.mjs`; never hand-edit those two.

## 1. The idea in one paragraph

Plainsong is a quiet, text-first identity for tools people use all day: near-black ink on near-white
paper, exactly one accent that means "you can act here", generous line height, a 4px grid, and
motion short enough that nobody waits for it. It is deliberately unfashionable — no gradients on
text, no glass, no hero video. The taste it expresses is that legibility outranks novelty, and that
an interface which is boring on the tenth day is better than one that is delightful on the first.

## 2. Typography

Sans: `"Inter var", Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue",
Arial, sans-serif`. Mono: `"IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, Consolas,
monospace`. **No font binaries are bundled** — both named faces are SIL OFL 1.1 (see
`REFERENCES.md`) and the stack degrades to the system UI face, so the package ships no font
licensing surface at all.

Scale is a 1.25 major third off a 16px base, in `rem` so browser zoom and user font-size settings
work:

| step | rem | ≈px | line-height | tracking | used for |
|---|---|---|---|---|---|
| xs | 0.75 | 12 | 1.5 | +0.01em | legal, timestamps, table meta — never body copy |
| sm | 0.875 | 14 | 1.5 | +0.005em | secondary text, form help, dense tables |
| base | 1 | 16 | 1.6 | 0 | **body default; never go below this for prose** |
| lg | 1.25 | 20 | 1.5 | −0.005em | lead paragraph, card title |
| xl | 1.5625 | 25 | 1.4 | −0.01em | section heading (h3) |
| 2xl | 1.953 | 31 | 1.3 | −0.015em | page subsection (h2) |
| 3xl | 2.441 | 39 | 1.2 | −0.02em | page title (h1) |
| 4xl | 3.052 | 49 | 1.1 | −0.025em | marketing hero only |

Weights: 400 body, 500 UI labels and buttons, 600 headings, 700 sparingly. Line length **45–80
characters, 68 ideal** (`--measure`). Tracking tightens as size grows because optical spacing that
suits 16px looks loose at 39px; that is why the table carries a tracking column at all.

## 3. Color

Full measured table with real ratios: **[`contrast.md`](contrast.md)** — generated, 13 contracted
pairs, all passing at the time of writing. Highlights: body ink `ink-900` on `paper-000` is
**17.82:1**; the primary button (`paper-000` on `accent-600`) is **5.82:1**; the link color
`accent-700` on paper is **8.46:1**; the focus ring is **5.82:1** against the page.

Roles, not decoration:

- **ink-900/700/600** — text. `ink-500` is the floor for *large* text and non-text UI only; it is
  5.93:1 here, so it is safe for body too, but the role stays "de-emphasised".
- **ink-300** — non-text boundaries (input borders, dividers that carry meaning). It is exactly the
  token that failed the first time this ledger was computed at 2.59:1 and was darkened from
  `#98A2AE` to `#868F9C` to clear WCAG 2.2 SC 1.4.11 at **3.27:1**. That failure is the reason the
  contrast script exits nonzero rather than printing a table nobody reads.
- **paper-000/100/200/300** — surfaces, lightest at the back.
- **accent-600/700** — the *only* interactive color. If everything is accented, nothing is.
- **success / warn / danger** — status, always paired with an icon and a word (see Rule 3).

## 4. Spacing, radius, elevation

4px base unit. Allowed steps: 0, 2, 4, 8, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128. Anything off
the grid is a mistake, not a nuance. Rhythm: 8px inside a control, 16px between related controls,
24–32px between groups, 48–64px between page sections.

Radius: 2 (chips) / 4 (inputs, buttons) / 8 (cards) / 16 (sheets) / pill (badges only). Pick one per
component and stay there; nested mismatched radii look broken.

Elevation is three shadows, all low-contrast and neutral-tinted. Shadow is never the only signal for
an interactive boundary — a border carries it, because shadows vanish in forced-colors mode.

## 5. Motion

Durations: instant 0 / fast **120ms** (hover, focus, color) / base **180ms** (most enter-exit) /
slow **260ms** (sheets, expanding panels) / deliberate **400ms** (page-level, rare).
Easings: standard `cubic-bezier(0.2,0,0,1)`, enter `cubic-bezier(0,0,0,1)`, exit
`cubic-bezier(0.3,0,1,1)`. Max travel **24px** — things fade and shift, they do not fly.

Under `prefers-reduced-motion: reduce`, every duration token collapses to 1ms and transforms are
dropped. This is compiled into `tokens.css` by the generator, so honouring it is the default rather
than a thing someone remembers to add.

## 6. Accessibility rules (non-negotiable in this identity)

1. Contrast: **4.5:1** body text, **3:1** large text (≥24px, or ≥19px bold) and non-text UI. Every
   documented pair is machine-checked; see Rule 12.
2. Focus is **always visible**: 2px solid ring, 2px offset, ≥3:1 against its background.
   `outline: none` without a replacement indicator is a defect.
3. Never color alone: status uses color **+** icon **+** text.
4. Targets ≥ **24×24px** (WCAG 2.2 SC 2.5.8), 44px recommended for touch, ≥8px between adjacent
   targets.
5. Every control has a programmatic name. Placeholders are not labels.
6. One `h1`; heading levels never skip. Landmarks on every region. A skip link first in tab order.
7. Text reflows and remains readable at 200% zoom and at 320px width — no horizontal scroll.
8. `prefers-reduced-motion` respected; nothing flashes more than 3×/second.
9. `lang` on `<html>`; page `<title>` is unique and describes the page.
10. Keyboard reaches everything, in DOM order, and can get back out of every trap (Esc closes).
11. Images: meaningful ones get real alt text, decorative ones get `alt=""`. Icon-only buttons get
    an accessible name.
12. **Automated checks are a floor, not a ceiling.** axe-core finds roughly a third of real issues;
    the remaining rules above are reviewed by hand and stated as reviewed, never as "axe passed".

## 7. Anti-patterns (named, so they can be called out)

Ghost buttons at 2.8:1 · placeholder-as-label · `outline: none` with nothing in its place ·
`<div onclick>` instead of `<button>` · icon-only navigation with no accessible name · tap targets
under 24px · disabled controls with no explanation of what would enable them · color-only status
dots · autoplaying carousels · parallax with no reduced-motion fallback · modals without focus trap
or Esc · justified body text and all-caps sentences · text over photography with no scrim ·
`px` font sizes that defeat browser zoom · infinite scroll over a footer people need ·
"click here" links · toast-only error reporting for a form field · 12px body copy.

## 8. How the seller uses this

The agent reads this file and `knowledge/INDEX.md` before proposing a redesign, applies
`tokens.css`, then **proves** the result: renders desktop and mobile, screenshots both, runs axe,
and diffs before/after. A redesign that cannot be screenshotted and checked is not delivered. The
rules in §6 that a machine cannot check are reported as hand-reviewed, by name, in the rationale.
