# Evidence manifest — GENERATED

Written by `tools/verify-manifest.mjs`, which re-reads every file from disk after the agent
run finished. Sizes and digests below were recomputed here, not copied from the agent's own
report.

- agent: `ui-ux-designer` · identity Plainsong (PROPOSED — not adopted. See design-identity/IDENTITY.md §0.)
- job: `review-redesign` (meridian-invoices)
- run: 2026-09-09T18:02:01.202Z → 2026-09-09T18:02:07.230Z
- node v26.5.0 · Chromium 151.0.7922.34 · axe-core 4.11.4 · sharp 0.34.5 / libvips 8.17.3

## Results

- baseline violations: {"desktop":4,"mobile":4}
- candidate violations: {"desktop":0,"mobile":0}
- baseline findings: button-name (1), color-contrast (34), html-has-lang (1), image-alt (1)
- visual change: {"desktop":"8.51%","mobile":"8.25%"}

## Artifacts

```
status  bytes      sha256                                                            file
OK          60634  5d40fc37979617c2872825b9526af59aee9d065f55d14cb9505ca3f9b7f31a73  evidence/screenshots/meridian-invoices-baseline-desktop.png
OK          48998  8d5d3acf13bcce74b051d07a4c2e92c89e2190a32682438a84b5deef1d17f5e1  evidence/screenshots/meridian-invoices-baseline-mobile.png
OK          93810  93d705a22651530527357a995039199518819fd487e634a3d06f0cc87be9b8b1  evidence/screenshots/meridian-invoices-candidate-desktop.png
OK         125764  a15b9d35add1c328d21a042ef4e27397bf3614f6ef4cc6f841322e13241e47ad  evidence/screenshots/meridian-invoices-candidate-mobile.png
OK           2239  fee78452372c394cd6a5d2d6a7366af846636b79cdf5297d1b09b3c832cabe73  evidence/a11y/meridian-invoices-baseline-desktop.json
OK           2238  3f2fb5cad655b8028cbaca2cfa5769e1ce57cf4bfd74533f754f505f92466ae8  evidence/a11y/meridian-invoices-baseline-mobile.json
OK            728  64d168666ff484b803a050ddce0c7bab0430b446587f5fb6465a457fffafb9cc  evidence/a11y/meridian-invoices-candidate-desktop.json
OK            751  d383cac8d43860e12db9669877563ce599425570b14adf72c110660652ba4fbb  evidence/a11y/meridian-invoices-candidate-mobile.json
OK          94671  96fd4dffb38b9a2ef8c4800be1d97de9dd9bcb36a2ce0de7479d3403c4b9eb89  evidence/diff/meridian-invoices-desktop.png
OK          95776  c69b99570211b4efca5ce0d05c437288d9799f6dc57ecca674fd0a1c0524b6fd  evidence/diff/meridian-invoices-mobile.png
OK           4475  dfd2cf14ca7231b4bfede0457fddfe5d8ffcdf1ecf60410903b616ae64d3d72c  evidence/images/meridian-invoices-mark.png
```

All 11 artifacts verified on disk.
