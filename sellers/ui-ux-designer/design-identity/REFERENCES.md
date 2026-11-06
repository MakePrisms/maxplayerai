# Reference corpus — source, version, licensing

Everything the Plainsong identity and this seller's judgement lean on, with where it came from and
what its licence permits. **Nothing in this package is scraped, and no third-party asset is
redistributed here.** Design guidance below is *consulted*, not copied: the tokens in
`tokens.json` are authored for this package.

## A. Code actually installed in this package

Versions and SPDX identifiers below were read out of `node_modules/<pkg>/package.json` on this
machine, not recalled from memory.

| Package | Version | Licence | Role |
|---|---|---|---|
| `playwright` | 1.62.0 | Apache-2.0 | browser automation, screenshots |
| `playwright-core` | 1.62.0 | Apache-2.0 | pinned by `overrides` to a single copy |
| `axe-core` | 4.11.2 | MPL-2.0 | accessibility rule engine |
| `@axe-core/playwright` | 4.11.2 | MPL-2.0 | axe injection into a Playwright page |
| `pixelmatch` | 6.0.0 | ISC | visual comparison |
| `pngjs` | 7.0.0 | MIT | PNG decode for pixel assertions |
| `sharp` | 0.34.5 | Apache-2.0 (libvips 8.17.3, LGPL-3.0) | SVG → raster, image resizing |

Browser binary: **Google Chrome for Testing 151.0.7922.34**, Playwright revision **1234**, from the
shared `~/Library/Caches/ms-playwright` cache. Chrome for Testing is Google's binary under its own
terms; it is *used*, never redistributed by this package, and nothing here vendors it.

MPL-2.0 (axe-core) is file-level copyleft: we call it as an unmodified dependency, which imposes no
obligation on this package's own source. Nothing here is statically linked into axe.

## B. Normative accessibility sources

| Source | Version / date | Licence | How used |
|---|---|---|---|
| WCAG 2.2 (W3C Recommendation) | 5 Oct 2023, latest errata 12 Dec 2024 | W3C Document Licence | the success criteria named in IDENTITY.md §6 (1.4.3, 1.4.11, 2.4.7, 2.5.8, 1.4.10, 1.4.4, 2.3.1) |
| WAI-ARIA Authoring Practices Guide (APG) | 1.2 patterns | W3C Software & Document Notice (BSD-3-Clause for code) | keyboard interaction expectations per widget |
| HTML Living Standard | WHATWG, continuously updated | CC-BY 4.0 | semantic element choice |
| MDN Web Docs | continuously updated | prose CC-BY-SA 2.5, code CC0 | API and CSS behaviour details |

W3C Document Licence permits reference and quotation, **not** derivative redistribution — so this
package cites criteria by number and never reproduces WCAG text wholesale.

## C. Design-system guidance consulted (ideas, not assets)

| Source | Version | Licence | What was taken |
|---|---|---|---|
| GOV.UK Design System | v5.x | code MIT, docs Open Government Licence v3 | plain-language error patterns; one-thing-per-page forms |
| U.S. Web Design System (USWDS) | v3.x | public domain (US Gov work) / CC0 | target-size and focus-visibility discipline |
| Material Design 3 — motion | M3, 2024+ | docs CC-BY 4.0, code Apache-2.0 | the idea of short standard easing curves; **our durations and beziers are our own numbers** |
| Refactoring UI (Wathan & Schoger) | 1st ed. 2019 | commercial book, no reuse | influence only: spacing rhythm, de-emphasis by weight not size. No text or asset reused. |

## D. Typefaces — named, never bundled

| Face | Version | Licence | Status here |
|---|---|---|---|
| Inter | 4.x | SIL Open Font Licence 1.1 | named first in the sans stack; **no font file ships in this package** |
| IBM Plex Mono | 6.x | SIL Open Font Licence 1.1 | named first in the mono stack; not bundled |

Both degrade to the platform UI face, so a machine without them renders correctly with system
fonts. If a buyer wants Inter actually served, they self-host it under OFL 1.1 with its licence
file — that is their deployment decision, not ours, and it is outside this package's scope.

## E. Sample content

`samples/redesign/` is written from scratch for this package: invented product name, invented copy,
no customer data, no third-party logo, no stock photography. Icons are inline SVG authored here.
Public-domain / self-authored throughout, so the before/after screenshots can be shared without any
clearance.

## F. What is deliberately absent

No icon-font, no UI kit, no Figma export, no scraped screenshot corpus, no paid asset library, no
model weights. If a job needs one of those, it is a buyer-supplied input or a blocker to report —
not something this seller acquires. (The fold forbids paid services and new subscriptions.)
