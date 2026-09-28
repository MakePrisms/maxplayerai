# Try it — implementation and verification

Implements stages 1–3 of `docs/specs/try-it.md`; stage 4 is deliberately not enabled.
The production target is fixed to worker-nemo. Every browser reserves one signed job in
IndexedDB before any write. No payment, wallet, house identity, analytics, third-party
scripts, external quota store, or direct-to-relay browser publishing path is added.

## Flags (all default off)

- `TRY_IT_ENABLED=true`: build-time Buy section/hero activation. Also required by the
  server entry point, so disabling it closes `/api/try`, not just its UI.
- `TRY_IT_API_ENABLED=true`: independent server enable flag. Otherwise **404**.
- `TRY_IT_MARKET_LINK_ENABLED=true`: build-time running-job market link. This stage
  links to **`/market`**; the polish removes the separate job-ID copy control.
  No job-specific deep-link route is introduced. Existing market accounting is unchanged.

No flags are enabled in checked-in configuration. Emergency API disable stops all
writes, including continuation. A coordinated production drain belongs to stage 4.

## Files and boundaries

- `wire.ts`: pinned Noble signing, strict lane validation, NIP-98, Rust-compatible
  preimage/hash verification. Reuses `src/model/kinds.ts`.
- `store.ts`: one IndexedDB read/write reservation, exact signed events and verified
  binding. Never copy the secret into an envelope, URL, channel message or log.
- `controller.ts`: bounded thread reads before forwarding; exact-ID reconciliation;
  profile acknowledgement before offer; pinned claim; persisted binding before ACCEPT.
- `transport.ts`: read-only job-scoped WebSocket queries; writes only to `/api/try`.
- `ui.ts`: Web Locks serialize controller work across tabs; BroadcastChannel carries
  only an update notification. Polls every 3s; bounded 1/2/5/10/30s recovery; a small manual
  Refresh status link after exhaustion or terminal state. A valid late result can complete
  an already-awarded job. No replacement offer is ever generated for recovery.
- `api/try.ts`: Web-standard Vercel function, streamed 64-KiB cap, verification before
  forwarding original eventBody bytes to the fixed HTTP bridge with fresh NIP-98.
  No caller-supplied URL/headers, redirect following, X-Pubkey or process-memory quota.
  Named helpers accept test dependencies; the production entry point does not.

The private identity is ordinary same-origin IndexedDB storage, **not encrypted custody**.
Storage removal can bypass the soft browser limit; public questions/answers remain public.
The script CSP uses only same-origin scripts; existing inline homepage scripts moved to
`main.ts`. The observatory source remains read-only and is not booted on Buy.

## Why these dependencies

Pinned runtime dependencies are `@noble/curves@1.9.7` and its matching
`@noble/hashes@1.8.0`. We import only secp256k1 Schnorr and SHA-256/encoding helpers;
esbuild tree-shakes unrelated curves. This avoids a larger Nostr relay/client SDK,
and implements no cryptographic primitive ourselves. Noble's secp256k1 implementation
has [Trail of Bits audit history](https://github.com/trailofbits/publications/blob/master/reviews/2023-01-ryanshea-noblecurveslibrary-securityreview.pdf).
That audit was of 0.7.3, **not a claim that our exact pinned version was audited**;
see Noble's published audit/change history. Rust fixtures independently check our
actual Schnorr/hash interoperability. `npm audit` reported zero vulnerabilities at
installation. Only `fake-indexeddb@6.2.4` is added for tests. Browser/CDP and local relay
checks add no runtime dependencies.

## Reproduce offline verification

From repository root (no production credentials or relay needed):

```sh
nix develop --extra-experimental-features 'nix-command flakes' --command bash -c \
  'cargo run -p maxplayer-core --features wallet --example try_it_fixtures > web/app/test/fixtures/try-rust.json'
cd web/app
npm test
```

The generator calls actual Rust `OfferDraft`, `claim_draft`, `award_draft`,
`inline_result_draft`, `accept_draft`, `parse_offer`, `parse_inline_result_delivery`,
`job_hash_for_offer`, and `ReceiptPreimage`. It uses **public test keys only** and signs
with nostr-sdk. Schnorr auxiliary randomness can change signatures between regenerations;
assertions compare exact semantic contracts and verify signatures, not guessed signatures.
The fixture seller is deliberately not Nemo; no Nemo private key is available or needed.

28 new tests cover Rust golden tags/digests/signatures, Unicode and exact text, restricted
profiles, malformed/duplicate tags, paid/foreign claims, altered/oversized/git results,
capability/metadata compatibility, duplicate/out-of-order delivery, transaction races,
reload, unavailable storage, lost acknowledgements, conflicting histories, expiry,
late results, refusal, untrusted result suppression, ACCEPT-pending binding, both API
gates, streamed limits, auth URL/body/key mismatch, fresh-auth retries/replay rejection,
evidence mismatch, HTTP accepted=false/wrong ID, outage/timeout and 429.

### Whole-path local relay

Start the disposable Rust relay from repository root:

```sh
nix develop --extra-experimental-features 'nix-command flakes' --command bash -c \
  'cargo run -p maxplayer-core --features wallet --example try_it_relay'
```

Pass its printed **loopback** address (all other hosts are refused):

```sh
cd web/app
node --import tsx scripts/try-local.ts ws://127.0.0.1:PORT
```

This exercises the real local Nostr relay, a stub seller waiting for AWARD, browser
controller/IndexedDB, API validation and an injected HTTP-to-WS bridge. Asserted buyer
writes: profile, OFFER, AWARD, ACCEPT; answer verified; **no receipt**. It is not a
real Nemo execution, deployed Buzz HTTP/NIP-98 admission test, or sandbox/cap attestation.

### Visual states

Build with the two browser flags on, serve dist only on loopback port 4907, and start
headless Chrome with remote debugging on 9433 (use a disposable Chrome profile):

```sh
TRY_IT_ENABLED=true TRY_IT_MARKET_LINK_ENABLED=true npm run build
python3 -m http.server 4907 --bind 127.0.0.1 --directory dist
# Separate terminal:
google-chrome --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
  --remote-debugging-port=9433 --user-data-dir=/tmp/maxplayer-try-chrome about:blank
# Separate terminal, in web/app:
TRY_IT_SCREENSHOTS=/tmp/maxplayer-try-screenshots node scripts/try-screenshots.mjs
```

The checker blocks production relay/Coinbase URLs before navigating. It captures 16
fixture-rendered states at both **390 and 1440px** plus hero/nav (34 PNGs), measures overflow/tap
targets also at **320px**, and checks hero focus/reduced motion/nav setup link. These
are local state fixtures, not screenshots of a live Nemo trade or real-phone tests.
It generates a temporary `dist/_preview.js`; a normal `npm run build` removes it.

## Stage-4 blockers / Vercel-native perimeter

No Vercel project was linked, deployed or configured by this implementation. The
function's Web-standard entry point is tested locally; **Vercel discovery of `/api/try`
with this static dist/cleanUrls project remains unverified** until an authorized preview.
The existing fork preview requires MakePrisms authorization; do not authorize it as
part of this work.

Before enabling, the Vercel owner must configure native Firewall for POST `/api/try`:
IP fixed window, initially **20 attempts / 10 minutes**, including rejected attempts;
confirm existing allocation/cost and test 429/Retry-After. Do not purchase an upgrade.
No BotID is selected and no external limiter/CAPTCHA is used. Function verification
remains mandatory after the firewall. Leave all flags off until this is proven.

Separately verify deployed relay NIP-98/membership/auth policy and Bob's durable,
atomic Nemo-wide free-execution cap, sandbox, bounded answer/time/concurrency and fresh
accepting heartbeat. Browser storage and Vercel controls cannot bound direct-relay or
fresh-key traffic. None of those operational controls is claimed implemented here.
