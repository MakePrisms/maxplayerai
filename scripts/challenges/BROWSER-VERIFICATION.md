# Direct-browser relay verification — 2026-10-16

Code verified: `64b0c91ccca6660e3594c4505112d657b1c4c04c`, based on
`origin/feat/community-challenges` at `49d12e44`.

## Checks

- `npm test --prefix web/app`: **370 passed**, zero failures (includes shared model and transport).
- `node --test scripts/challenges/*.test.mjs`: **48 passed**, zero failures.
- `npm run build --prefix web/app`: passed; restored default relay/buyer configuration after E2E.
- Full `bash scripts/challenges/run-e2e.sh`: exit 0, **65 PASS lines** including the final summary.
  Used the idle warm app target at
  `/home/openclaw/.openclaw/workspace/repos/maxplayer-challenges-e2e/target/challenges-e2e`,
  `CARGO_BUILD_JOBS=4`, `CARGO_PROFILE_DEV_DEBUG=0`; Buzz built in this checkout's isolated target.

Selected E2E output:

```text
PASS relay initial snapshot received
PASS Rust CLI and shared model agree on eligibility
PASS buyer auto-award committed without manual override
PASS relay streaming running job received
PASS minutes exactly match award/result timestamps
PASS relay streaming leave received
PASS join once eligible on day 1 and day 2 without rejoining or reaccepting
PASS swarm-solved zero payment
PASS swarm solved: completed + result visible
PASS swarm-exhausted zero payment
PASS swarm exhausted: completed + unsolved update visible
PASS ALL community challenge E2E
```

## Browser evidence

The OpenClaw browser tool rejected `http://127.0.0.1:51007/challenges.html` with
`browser navigation blocked by policy`. Browser-tool navigation is therefore **unverified**.
The documented local headless-Chrome/CDP fallback exercised the actual built page against
that E2E's disposable Buzz relay, not a fake feed or demo mode:

- 90 one-second DOM samples; every sample displayed the fixture's buyer.
- Only WebSocket destination: the fixture's `ws://127.0.0.1:56337`.
- One ephemeral AUTH public key, distinct from the buyer; outbound frames only AUTH, REQ, CLOSE.
- **64 EVENT frames on live subscriptions after their EOSE**.
- Without navigation/reload: seller row **Ready → Working now → Ready → Left**;
  subsequent challenge changes and two-seller runner activity also rendered.
- Zero requests to `/api/challenges`; no community server existed.
- No horizontal overflow at 1440×900 or 390×844 (mobile scroll width exactly 390).

Local private/ignored artifacts: `.scratch/browser-proof/evidence.json`, `mobile.json`,
`desktop-8.png`, `desktop-65.png`, `mobile.png`; E2E artifacts under `.scratch/ce-3mmoqy32`.
All disposable services and verification Chrome were stopped; fixture ports are closed.

AUTH refusal/poll fallback, signature/scope rejection, pagination boundaries, reconnects,
shared model behavior and history-round starvation are covered by regression tests.
Production relay behavior, deployment and paid settlement were not exercised. No real relay
writes, money operations, PR, merge, or deployment; no shared test environment or ct-* units changed.

## Split challenge/trade reader follow-up

Implementation: `31692c51`; failure-URL display follow-up: `d9b8d7f4`.

- **374 app tests, 49 challenge-script tests passed**. Final display follow-up passed
  TypeScript checking and the default production build.
- Full local E2E (`bash scripts/challenges/run-e2e.sh --skip-build`, using previously
  built binaries): **65 PASS lines**, exit 0, ending `PASS ALL community challenge E2E`.
  Both solved and exhausted runner scenarios passed, including zero payment and no
  posting after completion. Local artifacts: `.scratch/ce-ng5vv4hj`.
- Headless Chrome exercised two logical reader connections (challenge/trade) to that
  same disposable relay: 90 DOM samples, fixture buyer matched throughout, **59
  post-EOSE live events**, live Ready/Working/Left changes without navigation, no
  challenge API requests, only AUTH/REQ/CLOSE outbound frames, one shared ephemeral
  AUTH identity distinct from the buyer. No overflow at 1440px or 390px.
  Evidence: `.scratch/browser-split-proof/{evidence.json,mobile.json,desktop-8.png,mobile.png}`.
  Chrome and disposable E2E services exited. Browser-tool navigation was not repeated;
  the prior localhost policy restriction remains an unverified tool-specific path.
- Mock-socket regressions separately verify distinct relay destinations/kind routing,
  cross-relay ID deduplication/latest replacement, failed-public-relay tolerance, member
  heartbeat routing to trade only, anonymous public streaming, and shared ephemeral AUTH.
- Public-relay interoperability and physically separate challenge/trade relay E2E remain
  unverified. `E2E_CHALLENGE_RELAYS` configures reads; it does not reroute the existing CLI
  publisher. The Rust sidecar/publisher migration belongs to the other branch.
- No crates, publication invocation, protected environment, deployment, or real-money
  behavior changed. Only the requested browser branch is pushed.
