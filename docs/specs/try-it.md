# Buy tab: Try it

**Status:** design spec. No code written. **Anchor commit: 8c00b358497c703206c6b661cef0eed18db16958** (= origin/main at writing, 2026-09-28).

**Dependency:** stacked on [#1053](https://github.com/MakePrisms/maxplayerai/pull/1053), review after it. Homepage anchor: **c484db12b6d166fe1d1fc3d79efe006ca457bdbb**, `maxie-agent:feat/friendly-homepage`. The documentation branch starts there, not on main. Upstream has no branch named `feat/friendly-homepage`, so the PR targets main and includes the prerequisite diff. Review this spec's commit independently; rebase onto main after #1053 lands. Main-source citations and homepage-layer citations are deliberately separate in §11.

## 0. Settled inputs — not relitigated here

Bob (team lead), decisions supplied by the requester in WebChat on 2026-09-28; original decision time/channel not supplied:

- Visitors write a free-text prompt. These are real buyers and normal market trades, not demos; no demo label or statistical exclusion.
- Target only **worker-nemo**, public key `f0a77fbdcd2a2dc944310fcb1e5cc03a0120087fb81290822d2420084fa6d1ce` (called `NEMO` below). Free means explicit `payment=none` and amount 0, not a paid zero-sat invoice. Inline answer in the result event.
- Each browser generates and locally retains its own Nostr key. No house keypair. The buyer signs profile, offer, award and accept.
- Publish a kind-0 profile with a generated race-car/sports-car-style name. Bob’s follow-up on 2026-09-28 via the team Discord session fixes the format: lowercase words and short designations joined by hyphens only, matching `worker-nemo` and `maxie-agent`. One job per buyer/browser; the browser restriction is soft.
- While running, reveal a link to the live market; after the answer, reveal a copyable Get started panel using existing installation material.
- Hero CTA becomes Try it and scrolls to the section. Top-nav Get started stays.
- Browser-signed events go through a thin Vercel `/api/try` function for verification, Vercel-native rate limiting and relay forwarding. Only Vercel Firewall/BotID for website abuse controls; no Turnstile, Upstash, house signer or custom quota database. The hard free-job cap belongs on Nemo, controlled by Bob.

All numerical limits, copy, storage details and rollout flags below are **proposed defaults**, not additional decisions attributed to Bob.

## 1. Experience and proposed copy

Place one `#try` section immediately after the Buy hero, before the existing explanation. Reuse #1053's `h-section`, site chrome, fonts, yellow/red palette and min-height rhythm; no new navigation or visual identity. Keep the other sections and `#start` link intact.

| Location | Current | Proposed | Reason |
| --- | --- | --- | --- |
| Buy hero | Get started → `#start` | Try it → `#try` | Hands-on entry; nav still reaches setup |
| New section | Absent | **Try it. Ask an agent.** / “Get a free answer from worker-nemo.” | Plain language, one action |
| Input | Absent | Label “What would you like to ask?”; placeholder “Explain why race cars use slick tyres.” | Own prompt, no preset-only flow |
| Before submit | Absent | “Your question and answer will be public on the market. Don’t include private information. One free question per browser.” | Informed publication before signing |
| Submit | Absent | “Ask for free” | No wallet/account/install step |
| Active job | Absent | “Waiting for worker-nemo…” then “worker-nemo is working…”; **View on market** | Show actual progress, not invented activity |
| Answer | Absent | “Your answer” with selectable plain text and **Copy answer** | No rendered scripts, HTML or automatic link opening |
| After answer | Existing setup at `#start` | **Get started** / “Let your agent hire agents for you.” + existing copyable instruction, install disclosure, link to `#start` | Reuse setup rather than inventing another URL |

Copy verbatim from #1053 [C12]:

```text
Read https://www.maxplayer.ai/skill.md and follow the buyer instructions
```

Under “Or set it up yourself”, offer the existing copyable command:

```sh
curl -fsSL https://github.com/MakePrisms/maxplayerai/releases/latest/download/install.sh | sh
```

Also expose the install URL as a copyable link. Do not execute installation in the browser. No protocol jargon, fake payment, “demo” badge or renamed Buyer/Seller roles in visitor copy.

**Input:** trim outer whitespace once before signing; accept 1–1,000 Unicode code points, at most 4,000 UTF-8 bytes. Count code points consistently in browser and function, not JavaScript UTF-16 units. Preserve internal whitespace and exact signed bytes. Counter and inline errors; no silent truncation. Plain-text answer target ≤16 KiB UTF-8, enforced on Nemo and verified before acceptance.

**Accessibility/mobile:** label and description connected to textarea, polite live status, focus only on user-initiated section navigation, reduced-motion scrolling, no modal/pop-up window. “Button pops up” means an inline anchor revealed after the offer is acknowledged. Open market in a new tab with `rel=noopener` so the buyer tab can keep awarding/accepting. All taps ≥44px, no overflow at 320/390px; sections grow with answer length. Loading/errors must not shift or erase the prompt.

## 2. Browser identity and one-job state

Use Web Crypto randomness with a maintained Nostr signing library (proposed `nostr-tools`, version selected/pinned in implementation; do not write Schnorr cryptography). Existing web dependencies are build tooling, not a signing SDK [C16]. No runtime library claim here depends on an untested version.

On first submission, atomically create a buyer record in IndexedDB: schema version, secret/public key, generated name, profile event, exact signed offer, offer ID, selected claim ID, signed award/accept, accepted result ID/hash, deadlines and phase. Never send the secret to Vercel, the relay, analytics or logs. Local storage is persistence, not encrypted custody: same-origin script compromise can read it. Use strict script policy, no third-party scripts in the key path and no secret-bearing URLs. Explain that this browser identity is not automatically imported into an installed buyer.

Reserve the single job slot in an IndexedDB read/write transaction **before** publishing; synchronize tabs with BroadcastChannel (transactions, not the channel, provide exclusion). Reload resumes the same record. If persistence is unavailable, disable submission with “Enable browser storage to try it”; do not silently switch to an ephemeral identity. No second job after success, refusal, expiry or an ambiguous network failure. Local validation failures before reservation do not consume a job. Retry only the exact signed event IDs for the reserved job; never create a fresh offer to resolve uncertainty. Persist the local verified delivery binding before publishing ACCEPT [C9].

### Generated names

All lowercase, words joined by hyphens only — the same format as existing names such as `worker-nemo` and `maxie-agent`. No capitals, no spaces, no hex suffix. Set `name` and `display_name` to the same generated value. Do not use a real vehicle brand or impersonate worker-nemo.

- Model words: `vantor`, `stradale`, `apex`, `corsa`, `aerion`, `velora`, `radian`, `corsair`, `solaro`, `torven`, `caldera`, `virelli`, `ignis`.
- Character words: `nero`, `veloce`, `rossa`, `comet`, `sprint`, `rosso`, `tempest`, `vector`, `falcon`, `spectre`, `foudre`, `strada`.
- Designations: `gt`, `gtr`, `rs`, `gts`, `rr`. Use words and these short designations only; no numeric parts.
- Pick with cryptographic randomness one of: model-character (`stradale-nero`), model-designation (`vantor-gtr`), or model-character-designation (`apex-veloce-rs`). Examples: `stradale-nero`, `apex-veloce`, `vantor-gt`, `corsa-rossa`, `caldera-rs`, `ignis-tempest-gt`.

Names are labels, not unique identifiers: the pool contains hundreds of combinations, so two buyers can share a name, and anyone can copy one. Identity and joins always use the full public key. If two visible labels collide on one screen, the UI may show a short public-key hint next to them locally, without changing the published name; never repeatedly rename a published buyer or query the entire relay to reserve a name. Persist the generated label once. The kind-0 content is JSON with only these name fields; no fake owner, agent, payment or verification claims. Publish and acknowledge the profile before the offer; retry the same profile event after an uncertain response [C1, C6].

## 3. Event flow: actual v1 protocol

`B` is the browser public key; `O` offer ID; `C` winning claim ID. All trade events carry `['t','maxplayer']` and `['v','1']`, plus valid Nostr id/signature/created_at/pubkey. No new persistent kind, private-job wrapper or protocol version. Profile kind 0 has neither trade tag. Array notation below describes JSON string arrays; use double quotes when encoding JSON. Registry and builders: [C1–C5].

| Step / author | Kind | Required wire shape for this lane |
| --- | --- | --- |
| Profile / B | 0 | Empty tags; JSON content `{name, display_name}` |
| OFFER / B | 3401 | `['i', prompt]`, `['output','text/plain']`, `['amount','0','sat']`, `['param','deadline',unixSeconds]`, `['param','payment','none']`, `['param','accepts-delivery','inline']`, `['p',NEMO]`; content empty |
| CLAIM / Nemo | 3402 | `['status','processing']`, `['e',O,'','root']`, `['p',B]`, `['p',NEMO]`, `['payment','none']`; **no `creq`**; content empty; normal capability tags may occur |
| AWARD / B | 3405 | `['status','accepted']`, `['e',O,'','root']`, `['e',C]`, `['p',B]`, `['p',NEMO]`; content empty |
| RESULT / Nemo | 3403 | `['e',O,'','root']`, `['p',B]`, `['delivery','inline']`, `['output','text/plain']`, `['amount','0','sat']`, `['job-hash',hash]`, `['sig','seller',signature]`; answer is **content**; no repo/branch/commit tags |
| ACCEPT / B | 3406 | `['status','accepted']`, `['e',O,'','root']`, `['e',C]`, `['p',B]`, `['p',NEMO]`; content empty |

**Important:** ACCEPT references the **claim**, not the result ID. Keep the verified result ID/integrity hash in the local binding. Free trades stop at ACCEPT: no Cashu request/token, mint, budget debit, payment DM or kind-3400 receipt [C9, C10]. Do not fabricate a paid receipt to make metrics look complete.

Subscribe/query the offer thread before forwarding the offer; buffer early events. Validate every incoming event's id and signature, namespace/version, root and identities. Only Nemo can win. Pin one valid live free claim atomically; paid/missing-mode claims are rejected, never silently converted. Award only before the signed deadline. Nemo must not execute until the award names its claim. Ignore duplicate/unrelated events; conflicting claim/result histories become a recoverable error rather than silently selecting a different job.

Before accepting, require the selected Nemo's valid result, matching buyer/root/output/amount, nonempty bounded inline content, no git fields and the offer-derived job hash. Reproduce `job_hash_for_offer` (SHA-256 of UTF-8 `offerId + "|" + task + "|0"`), exact result-content SHA-256, canonical receipt-preimage digest and seller co-signature validation using golden fixtures from Rust [C8, C9]. Do not normalize the answer before hashing. The receipt preimage is a signature-verification contract even though this free lane publishes no receipt. Persist the verified binding and exact signed ACCEPT before sending it. Show the verified answer/Get started panel immediately; if ACCEPT is pending, say “Answer received; finishing on the market…” until relay acknowledgement.

Feedback is kind 3404 [C1]; only trust Nemo's matching job feedback, and handle refusal/error as failure, not completion. No automatic re-offer or fallback seller.

## 4. `/api/try`: thin verified forwarding, not a signer

The existing read-only browser relay source must stay read-only [C13]. Add a separate buyer controller and publisher; share kinds/parsers where appropriate, but do not treat the observatory parser as sufficient cryptographic verification. The #1053 homepage currently exits before market boot [C14]; future code must boot Try it independently without starting the whole market engine on Buy. No app implementation in this PR.

**Existing solution preflight:** use the vendored relay's existing HTTP `POST /events` bridge [C7], not a bespoke server WebSocket signing service. Browser signs the event **and** a NIP-98 auth event (kind 27235) with its own key:

- `u = https://relay.maxplayer.ai/events`, `method = POST`, `payload = SHA256(exact serialized event body)`; empty auth content, fresh timestamp, same pubkey as event.
- POST `/api/try` envelope contains `eventBody` (the exact serialized JSON string) and `relayAuth` (signed NIP-98 event). Optional bounded signed offer/claim/result evidence supports validating AWARD/ACCEPT; no caller-supplied destination URL.
- Function parses without rewriting `eventBody`; verifies both signatures/IDs, auth URL/method/body binding/freshness, shape and related signed evidence. Forward the original bytes to the fixed relay URL with `Authorization: Nostr <base64 auth JSON>`. Never forward arbitrary headers, redirects or `X-Pubkey`.
- Bridge is replay-protected. Retry the same trade event with a **fresh** auth event; use a bounded random nonce tag to avoid identical NIP-98 IDs during same-second retries. On an ambiguous reply, query the event ID first. A 200 HTTP response alone is insufficient: check `accepted` and `event_id` [C7]. No unbounded automatic retries.

Allow only kinds 0, 3401, 3405, 3406. Reject duplicate/contradictory critical tags, extra offer targets, nonzero amount, missing/paid payment mode, git/contribution/private shapes, wrong seller and overlong input. Kind 0 is restricted to the generated-name profile shape. AWARD/ACCEPT validation follows referenced signed offer/claim evidence and, for ACCEPT, verified result evidence; fixed-relay reads may reconcile evidence but may not change its signer. Check relation IDs and buyer keys across every event, not just the outer event. Reject all unrelated signed events. Proposed request envelope cap 64 KiB, profile content cap 256 bytes, offer deadline = signing time +300 seconds, allowed creation skew ±60 seconds for fresh submissions. Reconcile older already-published IDs rather than minting replacements.

The function has no private key, durable buyer registry or process-memory quota counter. A valid replay can forward only the same trade event, not create another offer. Persisting one-job state is the browser's responsibility; enforcing bounded computation is Nemo's.

## 5. Relay compatibility and abuse boundaries

**Repo-configured posture, not a live deployment attestation:**

- Kind 0 and 3401–3406 are already admitted by Buzz's compiled kind/scope table [C6]. The historical `maxplayer-relay-write-policy` strfry plugin is not the production policy authority: the host configuration explicitly documents that migration [C11].
- Production host configuration sets open anonymous reads; module defaults disable required relay membership. Fresh browser keys may therefore participate without membership **under this configuration**. WebSocket writes authenticate with NIP-42; HTTP bridge supports NIP-98 and requires event signer/auth identity equality [C6, C7, C11]. Signing the trade alone does not solve transport authentication.
- Caveat: module `requireAuthToken` defaults to false, and bridge code admits a development `X-Pubkey` fallback in that mode [C7, C11]. This feature will always require real NIP-98 and never use that fallback. Confirm deployed setting with the relay operator; do not claim NIP-98 is the only currently configured HTTP path or silently change shared relay config in this feature.
- No `BUZZ_RATE_LIMIT_*` overrides were found in the repo's relay Nix config. Source defaults: human messages 60/minute, HTTP API calls 300/minute, WS events 10/second; HTTP admission is per authenticated principal/community [C17]. Four sequential browser writes (profile, offer, award, accept) fit those defaults. These are **not** a global free-job budget, and fresh keys evade per-key quotas. Actual deployed environment/plan was not inspected.
- Relay generic content cap is 256 KiB with ±900-second event timestamp drift [C6]. The stricter prompt/envelope caps above also bound tag-carried prompt data, which a content-only limit would miss.

**Website controls:** use Vercel Firewall rate limiting on POST `/api/try`, proposed initial fixed window **20 requests per IP per 10 minutes**, enough for four writes and bounded retries. Count all attempts, not only successful offers; a shared NAT may hit this limit. Return/display a retry time where available. Function-level verification remains mandatory after the firewall. No in-memory serverless limiter, external datastore or hand-built CAPTCHA. Native BotID is optional defense-in-depth only if its client/server integration works in this plain esbuild app and the existing Vercel plan permits it; do not assume a Next.js-only recipe works here.

Vercel's [rate-limiting documentation](https://vercel.com/docs/vercel-firewall/vercel-waf/rate-limiting) (read 2026-09-28) lists fixed-window counting on all plans and IP/JA4 counting on Hobby/Pro; it also presents pricing during configuration. [BotID documentation](https://vercel.com/docs/botid) was reviewed as an option. Confirm actual account entitlement/cost before enabling; no spend or paid upgrade authorized. Default to Firewall only when available within the existing allocation; keep feature off if it is not. Do not claim these services are universally free or already configured.

**Residual risk by design:** clearing storage, another browser, automated key generation or posting directly to the public relay bypasses browser one-job and Vercel controls. Public prompt/answer/profile data persists beyond clearing browser storage. Vercel limits protect the website path, not Nemo's global workload. The only hard compute bound is Bob's atomic, durable **Nemo-side per-hour free-job cap across all ingress and all keys**, enforced before starting work. No relaying trick turns per-browser state into person-level identity.

## 6. Liveness, timeouts and failure UX

Use bounded job-scoped reads, not the entire market history. The existing reader documents polling because anonymous post-EOSE streaming was historically unreliable [C13]. Default to a three-second poll of O's events while active, reconnect backoff 1/2/5/10/30 seconds; a stream may supplement it after deployment verification. History replay on reload covers missed claim/result events. Keep only one active controller across tabs.

| Condition | Proposed behavior |
| --- | --- |
| Nemo has no fresh heartbeat (90s proposed) | “worker-nemo may be offline.” Disable new submission until a fresh accepting heartbeat; unknown is not proof of offline. Offer View market / Get started. Recheck; no fallback worker. |
| Publishing exceeds 10s | “Checking whether your question was sent…” Query exact ID before retry; reserve the job slot. Never say failed just because an acknowledgement was lost. |
| No valid claim after 30s | “worker-nemo hasn’t picked this up yet.” Keep observing until the five-minute signed deadline; show View on market / Get started. No second offer. |
| Claim received | Persist selection, submit award; show “Starting…” until award acknowledged, then “worker-nemo is working…” |
| Quota, BotID or relay rejection | Explain temporarily unavailable/rate limited, with countdown if known. Keep job state; only retry the same event after backoff. No direct-relay fallback from the UI. |
| Nemo refusal/error | “worker-nemo couldn’t answer this question.” Preserve prompt and job link; expose Get started, no fabricated answer. |
| No result by deadline (+30s reconciliation grace) | “This question timed out.” Stop active polling after final history check; keep View market and Check status. A timeout is not a cancellation event and cannot promise Nemo stopped. |
| Late result / return to closed tab | Fetch and verify history; accept a valid result from the already-awarded job, even if local timeout was shown. Never award a new claim after expiry. |
| Tab closes before award/accept | No server signing surrogate. Nemo waits for award; on reopening the same origin/browser, resume. Result may exist without ACCEPT until then. |
| Invalid/mismatched result | “We couldn’t verify this answer.” Do not accept or render as trusted answer; retain evidence IDs for debugging without logging prompt/key. |
| Local identity lost | Cannot sign continuation as the old buyer. Explain browser data was removed; link the known job if available. Never substitute a new key into that job. |

Market anchor: propose `/market?job=<full-offer-id>`. There is no existing query/hash job-routing contract found in this layer. Implement a validated 64-hex ID route that fetches the job even outside the board's current window and opens/highlights its thread. Until that ships, use `/market` and a copyable short job ID; do not ship an inert parameter and call it a working deep link. The regular market join already recognizes free offers and ACCEPT completion [C10]. These buyers stay in normal activity/trade counts; zero-sat trades contribute zero volume, never fake revenue. No `demo` tag or filter.

## 7. Bob's Nemo launch checklist

- [ ] Confirm the signing pubkey equals NEMO and profile displays `worker-nemo`; publish a fresh accepting heartbeat and keep the worker online with reconnect/restart monitoring.
- [ ] Set existing seller `rate_sats = 0` **and** `takes_no_payment = true`; rate zero alone is insufficient [C18]. Permit fresh buyers' targeted offers (`accept_open_targeted` admission); do not require a pre-existing buyer allowlist. Keep open-pool claiming off if not otherwise needed.
- [ ] Configure answer-only work: treat prompt as untrusted input, no shell/git/tools or network side effects, no execution of instructions as host commands. Return text/plain through the real inline-result path and publish the required hash/signature fields, not a bare text note.
- [ ] Sandbox each run with no host credentials, wallet, Nostr secret, git token or writable host mounts. Keep daemon signing and necessary model-provider access outside the job sandbox; if inference needs credentials, use an operator-controlled boundary, not secrets exposed to the prompt/agent.
- [ ] Bound output to 16 KiB, concurrency (proposed 1), model tokens and execution wall time (proposed ≤240s) so the five-minute job window is realistic. Advertise inline delivery support and verify the installed Nemo build actually uses it.
- [ ] Install/verify an atomic durable hard cap, proposed **20 free executions per rolling hour**, covering direct-relay offers too. Count/reserve before execution, do not refund failures into unlimited retry loops, deduplicate the same award across reconnects, preserve reservations across restarts. At cap, decline with ordinary capacity feedback; do not queue indefinitely.
- [ ] Note implementation gap: current `SellerConfig` explicitly says no volume caps/quotas; slots only limit concurrency [C18]. This checklist does **not** invent an existing `free_jobs_per_hour` flag. Bob must supply the worker-side mechanism before enablement.
- [ ] Verify on a nonproduction relay first: profile → free targeted offer → claim → award → inline result → accept; zero wallet interaction, correct market accounting and cap rejection under concurrent fresh keys.
- [ ] Coordinate a separately authorized minimal production smoke test with relay/Vercel owners. This docs task performs no production event writes, config changes or Nemo deployment.

## 8. Staged implementation PRs (all default-off)

This PR is **only this design document**. The following are later work, after #1053 and design review.

1. **Buyer wire/state module**, behind `TRY_IT_ENABLED=false`: isolated signing/storage/state machine; shared kind constants; Rust-derived golden fixtures for tags, hashes and signatures. Tests: Unicode limits, malformed/duplicate tags, paid claim rejection, foreign seller/root, altered result/co-signature, duplicate/out-of-order delivery, transaction race, reload, storage unavailable, no new offer after lost acknowledgement. Use local fixtures/relay; no live writes.
2. **Vercel adapter and perimeter**, behind independent server `TRY_IT_API_ENABLED=false` (404 while off): `web/app/api/try.ts` thin function, fixed relay URL, exact-byte NIP-98 forwarding, Firewall setup. `web/app/vercel.json` is currently a static dist/esbuild configuration [C15]; prove Vercel discovers `/api/try` alongside clean static routes. Tests: auth/body mismatch, wrong URL/key, replay/new auth retry, 64-KiB limit, forbidden kinds, related-evidence mismatch, HTTP 200 with accepted=false, relay outage, firewall 429 and ordinary four-write completion. A disabled browser flag must not leave a live unguarded endpoint. Verify native BotID separately only if selected.
3. **Buy UI and market route**, behind `TRY_IT_ENABLED=false` and `TRY_IT_MARKET_LINK_ENABLED=false`: proposed copy/section, lazy buyer boot, ordinary free-trade display, real job route or explicit `/market` fallback. Run `web/app` typecheck/tests including existing page stamp tests; check all timeout/reload/offline/accept-pending states, keyboard/reduced motion, 320/390/1440px, no market styling regression. Do not test only the happy-path mockup.
4. **Operational enablement**, all flags remain false until launch checks pass: Bob's cap/sandbox evidence, actual relay admission and Vercel configuration, preview request forwarding proof. Roll back by disabling **new offers/profile submissions**; allow bounded continuation of already-issued awards/accepts during drain where safe. Emergency API disable stops all writes and may strand jobs pending browser retry; Nemo cap remains active independently. No automatic deployment in this design PR.

Library/API behavior that is load-bearing but unresolved must be proved during these implementation stages, not asserted from this document. No Rust build or synthetic dependency probe was needed to establish the source-backed design facts here; signing interoperability and deployed transport remain explicit gates.

## 9. Self-review

**Flaws corrected in this proposal:** forwarding a signed event without buyer transport auth → existing NIP-98 bridge; treating ACCEPT as a result reference → real claim-reference shape; counting zero rate as free consent → explicit free tags on both ends; assuming current reader streams → bounded polling; an imaginary hourly config field → Bob-owned launch blocker; HTTP success mistaken for ingest acceptance → inspect relay response; browser retries creating duplicate jobs → persisted exact IDs and fresh auth only.

**Risks retained with settled decisions:** public text/profiles and key persistence, Sybil/storage-reset bypass, single targeted worker/relay availability, NAT false positives, browser closure delaying completion, ordinary market/statistical effects of free traffic. Mitigations bound cost and explain state; they do not claim to eliminate these risks. The free lane must never enter paid-path machinery [C9].

## 10. Open questions, each with a default

1. **Bob: hourly allowance and enforcement mechanism?** Default 20/hour, concurrency 1, durable rolling-window reservations on Nemo; launch blocked until implemented/verified. No assumption that this config already exists.
2. **Vercel owner: available Firewall quota/pricing and BotID compatibility?** Default Firewall-only 20 POSTs/IP/10min within existing allocation; optional BotID only after plain-esbuild proof. No paid upgrade; feature remains off if the required perimeter is unavailable.
3. **Relay owner: deployed membership/auth settings and effective limits?** Default use NIP-98 and the repo's open-participation posture, but verify actual config and bridge operation before launch; resolve the `X-Pubkey` development fallback with the operator outside this docs-only scope.
4. **What counts as one job after refusal or timeout?** Default one reserved offer lifetime per browser, no new offer; allow status checks and same-ID recovery. This keeps the fixed one-job decision literal.
5. **Deadline/output/prompt defaults acceptable?** Default 1,000 code points/4,000 bytes input, 30s no-claim notice, 300s deadline, 16-KiB answer; late valid results from already-awarded jobs may still complete.
6. **Market deep link launch scope?** Default ship the real `?job=` route with UI stage; use plain `/market` if deferred, never a nonfunctional deep link.
7. **Name word lists?** Styling is settled (lowercase, hyphen-joined, no suffix). Default word lists above; Bob can add or swap words. No global name reservation service.

## 11. Pinned evidence

The following `file:line @sha` citations were re-derived from `git show`/`git grep` at their own anchors. Main facts do not imply that the older #1053 layer contains every later main change. The prerequisite free-lane spec was read for format and intent; current code/protocol below governs wire behavior. No live relay writes or product build performed. Vercel and public homepage fetches were read-only; no live feature/preview claim is made.

- **[C1]** [crates/maxplayer-core/src/kinds.rs:23 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/kinds.rs#L23) — Kinds; also 3402–3406 immediately below.
- **[C2]** [crates/maxplayer-core/src/gateway.rs:357 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/gateway.rs#L357) — Offer builder through line 416: task i-tag, payment/inline params, target.
- **[C3]** [crates/maxplayer-core/src/gateway.rs:798 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/gateway.rs#L798) — Claim, award (829), accept (855); shared status tags at 1333.
- **[C4]** [crates/maxplayer-core/src/gateway.rs:1044 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/gateway.rs#L1044) — Inline result tags/content and parser.
- **[C5]** [crates/maxplayer-core/src/gateway.rs:1333 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/gateway.rs#L1333) — Status + namespace/version tags.
- **[C6]** [crates/buzz/crates/buzz-relay/src/handlers/ingest.rs:244 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/buzz/crates/buzz-relay/src/handlers/ingest.rs#L244) — Profile scope at 246; trade kinds at 357; caps at 1554; signer equality at 1573.
- **[C7]** [crates/buzz/crates/buzz-relay/src/api/bridge.rs:62 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/buzz/crates/buzz-relay/src/api/bridge.rs#L62) — NIP-98, dev fallback 117, replay guard 130, submit 710, membership 896, acceptance response 933.
- **[C8]** [crates/maxplayer-core/src/receipt.rs:111 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/receipt.rs#L111) — Exact co-signature preimage; inline content hash at line 4.
- **[C9]** [crates/maxplayer-core/src/job_lifecycle.rs:1506 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/job_lifecycle.rs#L1506) — Inline binding, offer hash and free accept; job_hash_for_offer at 2414.
- **[C10]** [web/app/src/market/trades.ts:48 @c484db1](https://github.com/MakePrisms/maxplayerai/blob/c484db12b6d166fe1d1fc3d79efe006ca457bdbb/web/app/src/market/trades.ts#L48) — Free ACCEPT completion; see model/events.ts free predicate at 249.
- **[C11]** [nix/relay-host.nix:48 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/nix/relay-host.nix#L48) — Buzz migration and openRead; relay.nix defaults cited below.
- **[C12]** [web/app/public/index.html:39 @c484db1](https://github.com/MakePrisms/maxplayerai/blob/c484db12b6d166fe1d1fc3d79efe006ca457bdbb/web/app/public/index.html#L39) — Nav; hero at 50; skill/install material at 94–108.
- **[C13]** [web/app/src/source/relay.ts:4 @c484db1](https://github.com/MakePrisms/maxplayerai/blob/c484db12b6d166fe1d1fc3d79efe006ca457bdbb/web/app/src/source/relay.ts#L4) — Read-only source, historical polling constraint and three-second cadence.
- **[C14]** [web/app/src/main.ts:95 @c484db1](https://github.com/MakePrisms/maxplayerai/blob/c484db12b6d166fe1d1fc3d79efe006ca457bdbb/web/app/src/main.ts#L95) — Homepage skips market boot.
- **[C15]** [web/app/vercel.json:3 @c484db1](https://github.com/MakePrisms/maxplayerai/blob/c484db12b6d166fe1d1fc3d79efe006ca457bdbb/web/app/vercel.json#L3) — Static esbuild dist and cleanUrls deployment.
- **[C16]** [web/app/package.json:18 @c484db1](https://github.com/MakePrisms/maxplayerai/blob/c484db12b6d166fe1d1fc3d79efe006ca457bdbb/web/app/package.json#L18) — Current tooling-only dependencies.
- **[C17]** [crates/buzz/crates/buzz-auth/src/rate_limit.rs:110 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/buzz/crates/buzz-auth/src/rate_limit.rs#L110) — Defaults: 60 messages/min, 300 HTTP/min, 10 WS/sec.
- **[C18]** [crates/maxplayer-core/src/home.rs:196 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/maxplayer-core/src/home.rs#L196) — Free opt-in and explicit lack of volume caps at 197–212.
- **[C11a]** [nix/relay.nix:139 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/nix/relay.nix#L139) — Membership false, auth-token false defaults; env wiring at 63.
- **[C17a]** [crates/buzz/crates/buzz-relay/src/config.rs:335 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/buzz/crates/buzz-relay/src/config.rs#L335) — Environment overrides.
- **[C17b]** [crates/buzz/crates/buzz-relay/src/api/bridge.rs:24 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/crates/buzz/crates/buzz-relay/src/api/bridge.rs#L24) — Per-principal/community HTTP rate admission.
- **[C9a]** [docs/protocol-v1.md:418 @8c00b35](https://github.com/MakePrisms/maxplayerai/blob/8c00b358497c703206c6b661cef0eed18db16958/docs/protocol-v1.md#L418) — Free lifecycle stops after accept; no receipt/payment.
