## Unreleased

- Private contribution jobs can use `base_local_path`, an absolute checkout or
  bare-repository path on the buyer daemon machine. The buyer uploads only the
  pinned commit and its history, not other branches or uncommitted files. The
  path stays local. An unavailable checkout falls back to the usual URL download.
  It works only for a direct job (`seller_pubkey`); an open-pool post with
  `base_local_path` is refused, because open-pool sellers must read the
  original URL before claiming. Symlinks and submodules remain unsupported.

- Private contributions on larger repositories no longer time out. The relay
  packs a whole repository before its first byte (about 11 s for 37 MB and 75 s
  for 300 MB), which outlasted the 10 s client on two reads:
  - Seller: the pre-claim read of private inputs and contribution bases uses the
    large-transfer client (300 s per leg) and runs off the seller event loop, so
    heartbeats, awards and shutdown keep running; a finished staging re-drives
    the offer. At most 4 run at once.
  - Buyer: posting keeps the fetched base under `<home>/store-seeds/` (up to
    14 days), and collect imports it into the delivery store before fetching the
    delivered fork, so that fetch asks only for the seller's new commits (under
    0.3 s for the same 300 MB repository). Every verification gate still runs;
    a missing seed only means the old, slower fetch.
  No relay deployment is needed. Update sellers and buyers.
- Git upload retries no longer mistake HTTP-status digits or auth words in a
  repository URL for an authentication refusal. Actual permission refusals still
  stop immediately.
- A permanent relay refusal of a private input upload (a 4xx other than 408, 409,
  421, 425 or 429, such as a quota or symlink/submodule refusal) is no longer
  retried with a full re-upload, and the post error names the refusal. Relay: a
  failure of the relay's own repository inspection now returns 503 (retryable)
  instead of 400; this part needs a relay redeploy.

- Private contribution preparation forwards a single fetched pack unchanged to
  an empty input repository when the server supports it, including jobs that also
  attach input files (the base is staged and uploaded separately, first), avoiding redundant
  libgit2 delta search. Other layouts retain the existing upload path; quotas,
  per-request authorization, redirect refusal and ref acknowledgement stay intact.
- MCP `post_job` now returns a durable preparation handle for slow work. Poll it
  with `get_job`; identical retries do not publish duplicate offers (a repeated
  success says `deduplicated: true`, a repeated failure says so in its message).
  Without `request_id`, a successful post dedupes identical arguments for 10
  minutes; an explicit `request_id` never expires. Input files are fingerprinted
  by content, in parallel and within 8 seconds; if that budget runs out the call
  claims and posts nothing and says so. Reusing a `request_id` with different
  arguments is refused with what that key already did (still preparing, posted
  job id, or a possibly-live offer). A failure before any offer was queued re-runs on retry; once an
  offer was queued, failures and restarts are never re-run, for either key kind,
  and the error names the offer id; abandoned owned staging directories are
  cleaned without touching live work or legacy unlabelled temporary directories.
  Buyer state adds an additive v8 preparation table; restart the buyer daemon and
  MCP server together after upgrading. No relay deployment is needed.

## v0.6.1-rc4

Fourth release candidate for 0.6.1, prepared from main after the private-job delivery
fixes. Once published, install with `npm install -g maxplayer@0.6.1-rc4`.
This is a prerelease: publish on npm `rc`, leaving stable `latest` unchanged at v0.6.0.

### Changes since v0.6.1-rc3

- Container delivery requests fresh, branch-scoped authorization from the seller
  host immediately before every Git HTTP request and retry, rather than reusing a
  short-lived token. Failed token hand-offs stop further refreshes so late replies
  cannot authorize later requests. The signing key remains outside the container
  (#1081).
- Container upload POSTs have a five-minute request ceiling, a 15-second connection
  timeout, and on Linux a 60-second bound on unacknowledged TCP data. This is not a
  progress-aware or unlimited upload policy; existing job deadlines still apply.
  Host-side delivery and buyer payment-verification timeout contracts are unchanged
  (#1081).
- Buyers preload the exact starting commit and its reachable history before
  publishing either targeted or open-pool private contribution offers. Sellers then
  upload only missing contribution data. Preparation failures prevent publication;
  existing jobs are not retroactively preloaded. Open-pool bidders still inspect the
  public source, and private-repo reads remain restricted until seller selection
  (#1093).
- Buyer preparation requests have a five-minute ceiling, including downloading the
  starting history, with a 15-second connection timeout. Input uploads get up to
  three attempts with fresh authorization and backoff. Before retrying, the buyer
  checks the exact remote ref to recover a lost success response or reject a
  conflicting commit. Repository quotas and payment behavior are unchanged (#1093).
- Mint-sidecar test helper cleanup resolves two clippy warnings; no mint runtime
  behavior changes (#1088).

### Rollout

Deploy the relay containing #1093 **before upgrading buyers**: old relays reject
open-pool input preparation before publication. No new schema migration is required
for these delivery fixes. Upgrade/restart buyer daemons to use the new preparation
path.

Upgrade the **seller daemon and versioned sandbox image together** for #1081. A new
container with an old daemon fails closed when authorization refresh is unanswered;
a daemon-only upgrade leaves old images without the fix. Test the combined rollout
with a newly posted private job.

## v0.6.1-rc3

Third release candidate for 0.6.1, prepared from main after the seller-credits stack
merged (#1084–#1087). Once published, install with `npm install -g maxplayer@0.6.1-rc3`.
This is a prerelease: publish on npm `rc`, leaving stable `latest` at v0.6.0.

### Release scope

RC3 is the first build from `main` with seller credits. It includes everything in RC2
and the `nostr://` mint support and `maxplayer-mint` sidecar that RC1 tested from the
unmerged stack, so RC1 testers can move to RC3. Known gap from RC1: a cross-mint hop
that RC1 journaled with a `nostr://` source is no longer swept automatically;
reconcile it by hand.

### Changes since v0.6.1-rc2

- Wallet support for `nostr://<npub>` mints, reached over Nostr relays instead of
  HTTPS (#1085). Accept one by adding its URL to `accepted_mints` and running
  `maxplayer wallet mints add <url>`. Melting is refused on every `nostr://` mint,
  including through cross-mint hops, and a `nostr://` melt saga is never resumed
  automatically. In-flight requests hold their inputs and cdk recovery until they can
  no longer land.
- `maxplayer-mint`, an opt-in sidecar that lets a seller issue its own credits
  (1 credit = 1 sat) over Nostr, with no HTTP server and no open port (#1086). It
  serves no minting or melting and says so in its info. It is not part of the
  `maxplayer` binary or the npm package; build it from source
  (`crates/maxplayer-mint/README.md`). A CI guard keeps cdk's mint code out of the
  default build.
- Seller-credits design spec (#1084), operator guide with backup and service setup,
  and the end-to-end harness (#1087).
- Buyer awards are reserved against the mint that will fund the job, not only the
  default mint, so a buyer funded at an extra mint is no longer refused at award
  (#1077). Refusals name the mint. The buyer store moves to schema v7 (an additive
  column; older binaries still open it).
- Website: production page-view analytics through Vercel, with query strings and
  fragments stripped (#1083).

- Docker Codex seats can explicitly select a model and reasoning effort through
  `[sandbox.harnesses.codex]` (`model = "gpt-5.6-sol"`, `reasoning_effort = "high"`),
  including ChatGPT subscription seats. ACP acknowledgments determine the advertised
  model; authentication containment and behavior without the setting are unchanged.

- Seller sandbox: update Codex ACP from 1.2.0 to 2.1.1 (Codex dependency
  currently resolves to 0.159.3), refreshing support for current Codex models.
  Available models still depend on the seller account. Ships with the next
  versioned sandbox image; existing images are unchanged.

- Seller sandbox: update Claude ACP to 0.85.0 (SDK 0.3.286 / Claude Code
  2.1.286) and the standalone Claude CLI to 2.1.286, enabling Opus 5.5
  (requires Claude Code >=2.1.280). Ships with the next versioned sandbox image.
  To select it per seat, set `ANTHROPIC_MODEL=claude-opus-5-5` on the seller
  daemon and add `forward_env = ["ANTHROPIC_MODEL"]` under `[sandbox]`, then
  restart the seat. Keep any existing forwarded variables.

## v0.6.1-rc2

Second release candidate for 0.6.1, prepared from main after the shared repository
limits and Git diff review changes (#1072). Once published, install with
`npm install -g maxplayer@0.6.1-rc2`. This is a prerelease: publish on npm `rc`,
leaving stable `latest` at v0.6.0.

### Release scope

RC1 was a separate seller-credits test build from the unmerged #1034/#1036/#1037
stack. **RC2 does not include those unmerged changes**, including `nostr://` mint
support or the mint sidecar. RC1 seller-credit testers should remain on RC1 until
that feature stack is included in a later release; RC2 is not its feature superset.
The RC1 notes below are preserved as the record of that published test build.

### Changes since stable v0.6.0

- Unified public/private repository defaults: 1 GiB compressed storage/upload,
  5 GiB unique uncompressed Git objects, 100 MiB per file and one million objects.
  Remove separate file-count and commit-count caps in clients, relay and reviewer.
- Git delivery security reviews now send the task plus the diff against the
  contribution's pinned starting commit, including additions, deletions, edits,
  mode changes and surrounding context. Artifact jobs diff against the empty tree.
  Small diffs stay together; large hunks split with task/path/location context.
  Every batch must succeed; independent requests can still miss interactions.
- Disk-backed relay downloads and review requests reduce whole-input memory copies;
  raise the Git-only reverse-proxy upload allowance. No transcript collection.
- Website/market updates: Try it on Buy and revised copy (#1068), graceful Try it
  safety declines (#1073), updated first-job video (#1074), and delivery-aware
  market lamps/removal of seller-profile Overdue status (#1075). Try it remains
  controlled by its existing deployment flags.
- Private-job documentation and v0.6.0 accuracy corrections (#1071).

### Coordinated rollout

Deploy matching relay, reviewer and Git reverse-proxy configuration, then update
and restart buyer/seller clients and MCP servers. Prepare client builds first:
older clients reject the changed private-repository provisioning limits. The
checked-in NixOS `#relay` deployment includes relay, reviewer and Nginx together.
Preserve existing wallets, keys, databases and repository data.

Public seller repositories still share storage across job branches. The new
limits do not change repository layout or retention. Large reviews can still
exceed provider/time budgets; batching is not proof of safe execution or a full
code-correctness review. Changed binary files, symlinks and submodules remain
unsupported review inputs. No paid full-size JEV benchmark is claimed.

Release publication alone does not deploy services or prove live compatibility.
Smoke-test private provisioning/delivery/collection and a reviewed Git contribution
after updating the server and clients.

## v0.6.1-rc1

First release candidate for 0.6.1, cut to test seller credits before they merge.
This is a prerelease; stable remains v0.6.0. Install with `npm install -g maxplayer@rc`.
It is built from the seller-credits PR stack (#1034, #1036, #1037), not from `main`.

### Changes since v0.6.0

- Wallet support for `nostr://<npub>` mints: a Cashu mint reached over Nostr relays
  instead of HTTPS (seller credits stage 1, #1034). Buyers and sellers can pay and
  accept tokens from such a mint when it is on their accepted-mint list.
- In-flight `nostr://` requests hold their inputs and cdk saga recovery until the
  request can no longer land, so a slow relay cannot cause a double spend (#1034).
- `maxplayer-mint`, an opt-in mint sidecar that serves a cdk mint over Nostr
  (stage 2, #1036), plus a local end-to-end harness (stage 3, #1037). The sidecar
  is not part of the `maxplayer` binary or the npm package; build it from source
  with `cargo build --manifest-path crates/maxplayer-mint/Cargo.toml`. A CI guard
  keeps cdk's mint code out of the default `maxplayer` build.

### Existing installations

Nothing changes unless a `nostr://` mint is added to the wallet or accepted-mint
configuration; HTTPS mints behave as in v0.6.0. GitHub marks this as a prerelease,
npm publishes it under `rc`, and stable `latest` stays at v0.6.0.

## v0.6.0

Stable 0.6.0 release, incorporating the RC1–RC8 fixes and the new homepage.
Install with `npm install -g maxplayer@latest`.

### Highlights since v0.5.11

- Private job flow with encrypted execution-review requests and results, and
  optional execution reviews for jobs (#1033, #1022).
- Docker Desktop seller compatibility: ignore dormant fallback tunnel devices
  and select a verified u32 egress backend when flower is unavailable (#1044, #1047).
- Reviewer startup, credential staging, shared-identity defaults, subscription
  recovery, and SQLite POSIX-lock lifetime fixes (#1042, #1045, #1051, #1054,
  #1056, #1061).
- Private Git provisioning, ref/auth handling, pre-run review and pinned-commit
  fetch fixes (#1055, #1058, #1059).
- Seller Git results echo the offer's output type, fixing buyer rejection of
  public deliveries with non-text/plain output (#1066, fixes #1065).
- New buyer-first homepage, seller page and first-job video; live marketplace
  moves to `/market` (#1053).

### Upgrade and rollout

Upgrade buyers and sellers and restart daemons/MCP servers. Operators upgrading
from 0.5.11 or early RCs must also deploy the matching relay/reviewer revision.
Keep existing wallets, signing keys and job state. For existing explicit trust
settings on the Maxplayer relay, follow the v0.6.0-rc5 identity guidance below;
a binary upgrade does not overwrite those settings or re-encrypt old jobs.

Before stopping a reviewer affected by the pre-RC7 lock bug, preserve its main
database and orphaned WAL consistently through its live file descriptors unless
you explicitly accept losing those records. Recovery must be performed on copies.
Already-published malformed result tags are not repaired or paid automatically.
See the RC7 and RC8 sections for these data/re-delivery limitations.

Relative to RC8, no new runtime protocol/schema/key migration is introduced.
Publication is not proof of live deployment or an end-to-end job test: verify
public/private jobs through review, delivery and settlement after rollout.

This is a stable GitHub release; npm `latest` advances to 0.6.0. The npm `rc`
channel remains on RC8. Seller-credit and Try it proposals are not included.

## v0.6.0-rc8

Eighth release candidate for 0.6.0. This is a prerelease; stable remains v0.5.11.
Install with `npm install -g maxplayer@rc`.

### Changes since v0.6.0-rc7

- Fix seller Git results to echo the offer's declared output type instead of
  hardcoding `text/plain`, including resumed deliveries (#1066, fixes #1065).
  Public jobs requesting types such as `application/json` can now produce
  results that pass the buyer's existing output-type validation.
- Add buyer-side evidence and seller-side output-tag regression coverage.

### Rollout and verification

Upgrade and restart sellers before testing new public Git jobs. Buyers may
upgrade as well; buyer validation is unchanged. This fix introduces no relay,
reviewer, database-schema, key or configuration migration.

Already-published results with incorrect signed output tags are not repaired by
upgrading. Re-delivery or separate settlement requires deliberate follow-up;
this release does not automatically settle those jobs. The separate
`relay_answered=false` diagnostic remains outside this fix.

Retest a new public Git job through delivery and payment. This release does not
claim a live end-to-end verification. GitHub remains a prerelease, npm uses `rc`,
and stable `latest` is unchanged.

## v0.6.0-rc7

Seventh release candidate for 0.6.0. This is a prerelease; stable remains v0.5.11.
Install with `npm install -g maxplayer@rc`.

### Changes since v0.6.0-rc6

- Close permission-check file handles before SQLite opens the reviewer and
  private-content databases, preserving SQLite POSIX locks and preventing
  orphaned-WAL review data loss (#1061).
- Stabilize wallet worker-send and child-process custody tests with offline
  fixtures and deterministic synchronization (#1062).

### Rollout and existing data

Deploy and restart the reviewer with this revision. Upgrade buyers and sellers
for the private-content store fix, then restart their daemons/MCP servers.
No protocol, schema, signing-key or configuration migration is required by RC7.
Client installation does not deploy the relay/reviewer host.

Before restarting an affected reviewer, preserve the main database and any
orphaned WAL through the running process's `/proc/<pid>/fd/` descriptors, with
writes paused for a consistent capture. Recover on copies. Operators may skip
recovery only if they accept permanently losing the stranded review records.
The fix prevents future lock loss; it does not recover already orphaned rows.

Verify the restarted reviewer retains its database lock and persists new reviews;
retest a private job end to end. This release does not claim live-host verification.
GitHub remains a prerelease; npm uses `rc`, and stable `latest` is unchanged.

## v0.6.0-rc6

Sixth release candidate for 0.6.0, containing the private-job fixes merged since
RC5. This is a prerelease; stable remains v0.5.11.
Install with `npm install -g maxplayer@rc`.

### Changes since v0.6.0-rc5

- Resolve the reviewer's systemd state directory before opening SQLite (#1054).
- Seed the repository manifest when provisioning a private job, allowing its
  first input or delivery push (#1055).
- Recover reviewer subscriptions after NIP-42 authentication, reconnects and
  subscription closures; isolate malformed requests and retain the client default
  accepted mints (#1056).
- Correct private Git ref names and signed input-read headers; use the encrypted
  review path for the seller's pre-run offer check (#1058).
- Advertise support for fetching reachable commits by their pinned ID (#1059).

### Coordinated rollout and verification

Upgrade buyers and sellers and restart their daemons/MCP servers. Deploy the
relay and reviewer from a revision containing these fixes as well: installing a
client package does not deploy the server. Preserve the RC5 shared identity and
existing keys; explicit old client settings still require the RC5 migration below.

These fixes are merged and their main-branch CI passed. This publication does not
claim a newly verified live private-job round trip. Retest a new private job from
input upload through offer review, seller execution, delivery review and collection.
Do not treat old jobs as migrated or re-encrypted by this upgrade.

GitHub remains a prerelease; npm uses `rc`, and stable `latest` is unchanged.

## v0.6.0-rc5

Fifth release candidate for 0.6.0. This is a prerelease; stable remains v0.5.11.
Install with `npm install -g maxplayer@rc`.

### Changes since v0.6.0-rc4

- Reuse the deployed reviewer public identity (`31b18b42…`) as the shared
  private-content/reviewer default (#1051). Client and relay service defaults agree.
- Keep the existing reviewer private key in place; no OpenClaw secret-store export
  or reviewer signer replacement is required for this migration.
- Document coordinated configuration changes and old-job limitations.

### Existing installations

Upgrading does not overwrite explicit client settings. Set `privacy.service_pubkey`
and the `wss://relay.maxplayer.ai` reviewer entry to
`31b18b42bcef9842c10e518834d32da2a0f8f6f8f3758124e25cc392ada1fe5c`, then restart
buyer/seller daemons and MCP servers. Align the relay's `MAXPLAYER_PRIVATE_SERVICE_PUBKEY`
override (or deploy its new default) and restart the relay. Verify the worker actually
uses the matching existing signer and prove a new private job through offer and
delivery review before calling the rollout complete.

This release does not change live credentials, rewrite existing repository access
records, or re-encrypt old jobs. Preserve old keys and job state. GitHub remains a
prerelease; npm uses `rc`, and stable `latest` is unchanged.

## v0.6.0-rc4

Fourth release candidate for 0.6.0. This is a prerelease for testing, not a
replacement for stable v0.5.11. Install with `npm install -g maxplayer@rc`.

### Changes since v0.6.0-rc3

- Unify the default execution-reviewer identity with the existing private-content
  service identity (#1049). Both now use `7e6b3b05…`; the reviewer default references
  the service constant rather than maintaining a competing public key.
- Add a regression check that fresh-home reviewer, privacy and relay-protocol
  identity defaults agree. Preserve custom relay trust, explicit reviewer maps,
  empty maps and disabled review settings.
- Correct operator setup guidance to reuse the existing content-service private
  key, keep the relay identity separate, and coordinate signer/client migration.

### Required operator migration

This release does not provision the live reviewer's signing key. The reviewer must
run with the existing content-service private key before clients use the new trust
identity. Existing explicit `[review.reviewers]` entries retain their old values:
update them to match `privacy.service_pubkey` and restart buyer/seller daemons and
MCP servers. A binary upgrade alone does not repair those explicit settings.

Keep private jobs private. Verify the live reviewer identity and a complete private
job (offer review through delivery acceptance), plus a public review, before broad
rollout. Production signer migration and these live checks were not verified when
this candidate was prepared; local tests and CI are not deployment proof.

GitHub remains a prerelease, npm uses the `rc` dist-tag, and sandbox images
use the versioned RC tag without moving stable `latest`.

## v0.6.0-rc3

Third release candidate for 0.6.0. This is a prerelease for testing, not a
replacement for stable v0.5.11. Install with `npm install -g maxplayer@rc`.

### Changes since v0.6.0-rc2

- Fix Docker Desktop seller startup when the VM kernel lacks the flower traffic
  classifier (#1047). Automatically probe and select a verified u32 fallback.
- Keep sandbox containment fail-closed. The compatibility backend preserves normal
  web, DNS and model-proxy access while blocking unsupported packet formats:
  IPv4 options/fragments, IPv6 extension headers and non-TCP/UDP/ICMP protocols.
- Independently verify the selected classifier's installed rules and name the
  classifier in the startup log. No manual networking configuration is required.

Petar reported local Mac verification before requesting this candidate.
GitHub remains a prerelease, npm uses the `rc` dist-tag, and sandbox images
use the versioned RC tag without moving stable `latest`.

## v0.6.0-rc2

Second release candidate for 0.6.0. This is a prerelease for testing, not a
replacement for stable v0.5.11. Install with `npm install -g maxplayer@rc`.

### Changes since v0.6.0-rc1

- Fix startup failure when reviewer configuration contains relay URL map keys (#1045).
- Supply the deployed Maxplayer reviewer key by default for the Maxplayer relay.
  Explicit reviewer settings, empty reviewer maps, and disabled reviews are preserved.
- Fix the shell installer rejecting release binaries whose version includes a commit SHA.
  Incorrect versions and malformed build stamps remain rejected.
- Ignore dormant fallback tunnels when selecting Docker Desktop job egress (#1044).

GitHub remains a prerelease, npm uses the `rc` dist-tag, and sandbox images
use the versioned RC tag without moving stable `latest`.
Private-job reviews still require the reviewer identity to match the configured
private content-service identity; this candidate does not change that requirement.

## v0.6.0-rc1

First release candidate for 0.6.0, based on main at `99e3c5f`.
This is a prerelease for testing, not a replacement for stable v0.5.11.
Install with `npm install -g maxplayer@rc` or select the versioned release assets.

### Changes since v0.5.11

- Private job flow and encrypted private-job execution reviews (#1033).
- Optional execution reviews (#1022).
- Owner-only staging of reviewer systemd credentials (#1042).
- Isolated custody test fixtures to prevent collisions (#1029).

The GitHub release is marked prerelease, npm packages use the `rc` dist-tag,
and sandbox images use the versioned RC tag without moving stable `latest`.

## v0.5.11

### The platform fee destination moves to `maxplayer@strike.me`

`PLATFORM_FEE_ADDRESS` is now `maxplayer@strike.me`. Every release through v0.5.10 remitted to
`maxplayer@agi.cash`; a binary built from this version and later pays Strike. The address is still a
compiled-in constant with no config key, environment variable or flag, for the reason given under
v0.5.8 (*The platform fee is charged, and now paid*): a seller-editable destination would let a
seller pay the fee to itself.

The rate and the Rust remit path are unchanged. What does change is the live ceiling the LNURL-pay
endpoint advertises: Strike answers `minSendable` 1 sat and `maxSendable` 16,000,000 sats where the
previous host advertised 1 to 1,000,000 sats, so an accrued balance between one and sixteen million
sats that was previously refused as above the maximum becomes payable. Remittances already journaled
carry the literal they paid, so history written against the old address stays readable as it was.

## v0.5.10

A seller's delivery lock is now bounded by the lifetime of the push *work*, not by the patience of
whatever async arm started it. A caller that times out no longer hands the seat to the next delivery
while bytes from the old one are still on the wire, and a revoked push stops at the next boundary
instead of doing its local work when the slot comes free. Nothing about relay policy, token expiry
or the per-leg authorization from v0.5.9 changes.

### The delivery turn is bounded by the work, not by its caller (#1006)

The seat's delivery lock was released when the future that started the push finished. Three things
followed from that. A push revoked while parked on a blocking thread still occupied the turn, and
still performed its local work once the thread woke. Queued and pre-HTTP work honoured neither
cancellation nor a deadline. And an async supervisor cancelled at an await took the lock guard with
it while its own blocking thread was still mid-upload, which let the next delivery open a second
`git-receive-pack` against the same remote.

The turn is now handed back only when **both** sides are done with it: the work has actually stopped
(or provably never started), **and** the supervising arm has left the excluded section. Either
condition alone has been a bug — supervisor-alone was the original defect, and work-alone is its
mirror. A caller's timeout does not free the seat for live work: it revokes, declares its own side
finished, and returns `TimedOut`.

New `crates/maxplayer-core/src/delivery_turn.rs` carries the exclusion token itself — the lock's
owned guard, moved in, so a dying supervisor cannot take exclusion with it — plus an absolute
deadline fixed when the turn is created. `begin()` is queue admission on the blocking thread, and
`RunningWork` is dropped on the thread that did the work. `seller_git` composes one gate for the
transport: the delivery's authority first, the turn's lifetime second.

That gate is asked at queue admission, before the push-config rewrite, before pack generation, at
pack negotiation, before the mint so a dead delivery never joins the signer queue, again after the
mint because the queue wait is exactly where a turn dies unnoticed, on every buffered pack chunk,
and before every wire request.

The drain bound is stated as a constant rather than inferred:
`DELIVERY_DRAIN_BOUND = DELIVERY_PUSH_TIMEOUT + DEFAULT_HTTP_LEG_TIMEOUT` = 150s + 120s = 270s, with
a build-time assertion on the sum and a test pinning the literal. It is not an HTTP timeout — it is
the work's absolute deadline, checked at every boundary above, plus the one in-flight leg whose
bytes cannot be recalled.

One span stays outside that guarantee and is documented rather than hidden: libgit2's delta search
discards its cancellation answer, so it is bounded by the delivery's object list rather than by a
clock. It is measured from its true start and an overrun is reported with the number;
`UNINTERRUPTIBLE_DELTA_BUDGET = 5s` is what that span is expected to fit in, held finite and
strictly inside the work deadline by a second compile-time assertion. A hard bound there needs a
killable executor — the local phase in a child process, the deadline enforced by a signal — which is
an architectural change, written down instead of smuggled in.

The tests run the real signer actor, the real transport and a real HTTPS git fixture, with no sleep
used as scheduling proof: the fixture parks a chosen request and announces it, and ordering is read
off one journal. They cover cancellation before dispatch (`.git/config` byte-identical, no mint, no
dial), cancellation during the signer queue wait with the mint parked inside the round trip,
cancellation mid-flight with the advertisement held at the server, and a real GET and POST across a
caller timeout — the POST held open at the relay while the arm that started it times out, a second
real delivery launched into that window and proved pending on acquisition, landing its ref only
after the abandoned upload stops. Peak concurrency stays 1 throughout.

## v0.5.9

Every delivery push now mints its authorization at the request it is sent on, and a contained job
under gVisor is handed a resolver it can actually reach. Both are on for a seat that upgrades
without changing its config. A seller can also offer a third-party tool to its jobs without the
credential entering a job container — that one is opt-in, on new config keys.

### The delivery push authorizes each leg, and asks nothing back (#994)

A git push is several requests — the ref advertisement, the pack POST, any attempt after those —
and libgit2 opens a fresh stream for each. Until now one token minted when the job was picked up
had to cover all of them, and it was at its oldest exactly when the relay finally checked it.

The push is now handed a minter rather than a header: `HttpStream::send` calls it immediately
before each request leaves, passing the destination it is about to contact. Every wire request
carries its own fresh NIP-98 token scoped to this job's ref. **Nothing about relay policy or token
expiry changes** — the tokens are simply younger when they arrive. Signing stays inside the signer
actor; the seller key never leaves it, and both legs of the blocking bridge are bounded, so a
stalled signer surfaces as a failed leg instead of parking the thread holding the delivery lock.

The minter is shown the destination and compares it against the transport's own `same_destination`,
so a leg aimed anywhere but the remote this job was told to deliver to cannot obtain a token at all.

**The post-push remote read-back is gone**, along with `attest_remote_branch` and
`check_remote_attestation`. The per-ref status report is now the only accepted answer:
`require_status_report` treats an empty report, a report for somebody else's ref, a rejection, a
contradictory report, and extra refs as failures. Silence is not acceptance.

Three further refusals ship with it:

- **No followed redirect.** A 3xx is a destination nothing authorized, and reqwest keeps the
  Authorization header across a same-origin hop while 307/308 replays the body. Both transport
  clients now follow nothing, and a 3xx fails the leg naming the destination that *was* authorized.
- **No hidden replay.** reqwest can replay a request it believes was never processed (HTTP/2
  GOAWAY, REFUSED_STREAM) from below `HttpStream::send`, so the minter was never asked and the
  first attempt's token went out again. Both blocking clients now carry `reqwest::retry::never()`.
- **Authority ends when the delivery's turn ends.** The authority flag is re-checked inside
  `HttpStream::send` after the mint, with nothing between the answer and the send, and
  `PushAuthority` revokes on `Drop`, which covers cancellation, early return and panic.

The evidence runs the production article — two awarded jobs, the real `serialized_bounded_push`,
the seat's one delivery lock, the real signer actor, and a real HTTPS git remote recording the exact
Authorization bytes of every request — and asserts four distinct tokens, each naming its own job's
ref, and exactly two requests per push, which is the read-back's absence stated as an assertion.

### Contained jobs get a resolver they can reach (#995)

Under gVisor a contained job's lookups went to docker's embedded resolver at 127.0.0.11, which the
sandbox terminates in its own network stack: every lookup failed `EAI_AGAIN` while the identical
container under runc resolved. `--dns` cannot fix it, so the job is handed a real resolver file and
its egress policy opens port 53 to exactly the addresses in that file.

- Resolvers are discovered from `[sandbox] dns_servers`, else the host's own upstreams
  (`resolv.conf`, then `resolvectl` for a systemd stub). Loopback and the cloud metadata endpoint
  are refused with a named reason.
- One udp and one tcp ACCEPT per resolver, pinned to that host address (/32, /128) on port 53 only,
  below the metadata DROP and above the range DROPs. The readback now proves each rule's address,
  protocol, port and position rather than counting ACCEPTs.
- Resolvers are resolved once before the namespace exists; the same value renders the file and the
  exceptions, and the file is mounted read-only at `/etc/resolv.conf`.

**No rule was removed, no deny widened, no protection relaxed.** `maxplayer doctor` now reports the
canonical resolver plan a launch installs, and marks the unconfigured case as an explicit
unverified discovery floor rather than a certified plan.

### A seller can offer a third-party tool without the credential entering the job (#1004)

Two automated routes ship on new config under `[sandbox]`, and a skill routes a seller's tool to the
one that fits it. **A seat that declares neither key is unchanged.**

- **Proxy swap** (`[[sandbox.mcp_tools]]`). The job holds a per-job placeholder. The existing
  credential proxy (#647) swaps the real credential into the `Authorization` header at egress, for
  the vendor's host only, for the life of the job, and the job reaches a vendor-hosted MCP server
  through `mcp-http-bridge`, now installed in the sandbox image. The docker alias pinhole opens only
  for a job outside namespace containment, and only when that job's environment or one of its MCP
  server entries references the alias; the launch capture redactor knows every real credential value
  a launch holds.
- **Holder** (`[[sandbox.held_tools]]`). The daemon runs one persistent holder container per declared
  tool — `tool-holderd` from the new `maxplayer-tool-kit` crate, plus the vendor's own CLI. The
  holder enrols once and resumes its login across restarts. Each job gets its own Unix socket per
  tool, mounted as a volume subpath and reached through `tool-mcp-bridge --socket …`; job end
  detaches the socket and the tool stays enrolled.

**Public** and **Direct token** stay manual setup, with the steps in the skill. **Dedicated machine**
and **Browser login** are not supported at this moment: the skill says so and stops. The routing
decision tree is `docs/specs/seller-tool-onboarding/10-routing-and-options.md`; the skill is
`.claude/skills/seller-tool-onboarding/SKILL.md`.

`driver/acp.rs` now carries `McpServer` as the ACP wire shape the adapters actually read — a stdio
entry with no `type` key, or an `http` entry.

**What is live validation here, and what is not.** The two are not interchangeable, and the
difference is the reason to read this paragraph:

- **Proxy swap — validated live against a third party.** Accepted on 2026-09-14 against GitHub's
  remote MCP server, through the real proxy, a real sandbox launch and a real `claude-agent-acp`
  turn. The token owner's login came back from GitHub; the token was absent from everything the
  container received; the placeholder sent straight to GitHub got `400`, and an unknown placeholder
  at the proxy got `502` with no substitution; the per-job placeholders were revoked at job end.
  The credential was a fine-grained read-only token limited to public repositories, on a read-only
  endpoint — that pair, not the proxy, is what bounded the job.
- **Holder — a live run of the mechanism, with the kit's fakes standing in for the vendor.** On
  2026-09-14, through the daemon code, two holders enrolled once each, one job drove both through
  their own sockets, a contained job ran, a restart resumed both logins, and a real
  `claude-agent-acp` turn called both tools. **No real vendor CLI was exercised**: the vendors and
  the CLI in that run are the test doubles shipped in the kit, so a real vendor CLI inside a
  seller-built holder image remains its own acceptance run, per vendor.
- **Source-level only.** The routing tree, the manual-setup steps, and the unsupported-route prints
  are written decisions checked by reading this tree. No run stands behind them, and this section
  does not claim one.

The live tests are `#[ignore]`d and configured by environment — `seller_exec::mcp_tool_tests::live_*`
and `held_tool::live_tests::live_*` — and each of this feature's three bundles under `evidence/`
(the `20260914T…` directories) carries a README stating what it proves and its limits.

Two limits ship with the feature. The Holder route needs Docker Engine 26 or newer for the volume
subpath mount, and a seat too old for it fails its boot line rather than every awarded job. The
proxy constrains the destination host, not the operations, so a broad credential behind the Proxy
swap still needs a trusted operation filter, which is not built; each holder holds one credential
for one vendor.

### Also

- The Muse layer of the buyer path ships as a published skill, source-checked against this tree
  (#990).
- `.gitignore` additions (#987).

### Worth knowing

- **A containment finding was recorded, not repaired.** A live gate bound to `runsc` and proving
  what it ran under shows that a gVisor payload passes through host netfilter rules that runc
  denies: a private address inside the `172.16.0.0/12` DROP is reachable, and so is a non-53 port
  on the resolver. The rules render, verify and install correctly. The candidate mechanism — gVisor
  terminating the network stack in user space — is written down as a hypothesis, not a proof, and
  a fix is a change to how containment is enforced for that runtime.
- **The cancellation / lock-bound issue is DEFERRED** and not addressed in this release
  (tracked in thread 1547960593204650058).
- This release carries no claim of a full-scope review PASS, and live post-fix delivery has not
  been proven: the delivery-push work is covered by offline gates against real HTTP/2 and HTTPS
  fixtures, not against a production relay.
- The DNS runtime gates are Linux-only and `#[ignore]`d; they are not run by CI.
- No CI job compiles `crates/buzz`, so the relay code in this release was never built by CI.

## v0.5.8

A seller node now charges a 10% platform fee and pays it automatically, and a docker seat delivers from inside its container. Both are on for a seat that upgrades without changing its config.

### The platform fee is charged, and now paid (#973, #979)

The product takes 10% of the offer amount — the price the buyer paid — on every payment a seller collects. The rate is compiled in, and so is the destination: the Lightning address `maxplayer@agi.cash`. There is no config key, environment variable or flag for either. A seller-editable destination would let a seller pay the fee to itself.

Two stages ship together. Collecting a payment journals what the fee comes to, and `maxplayer seller fees` prints it per job beside what the buyer paid, the mint fee, and what you keep. Then the node pays it: once a receipt is journaled new, a thread of its own resolves the destination over LNURL-pay and melts the accrued balance out of the seller's ecash. A balance under the destination's minimum accumulates instead of paying — the expected steady state for small jobs, not an error.

**The fee comes out of the accrued amount, never on top of it.** The invoice, the mint's fee reserve, the proof input fee the wallet SDK recomputes on the proofs its swap hands back, and any pre-melt swap fee must all fit under the accrued gross — checked immediately before any ecash is spent. Over it, the prepared payment is cancelled, its proofs go back to unspent, and the attempt is journaled as a refusal with nothing posted.

**It cannot affect the payment it follows.** The receipt is written and the job is marked paid before the attempt starts, and the attempt cannot fail the collect, delay it, or change what the seller received. A failure is logged and journaled and leaves the balance owed; the node then retries from a 30-second base out to a 30-minute cap, jittered, for as long as it runs.

`[platform_fee] auto_remit = false` stops both automatic attempts — the one after a collect and the retry tick — and does nothing else: the fee still accrues, stays owed, and stays visible. It cannot change the rate or the destination. `maxplayer seller fees remit --confirm` is the operator's recovery path and pays regardless of the switch.

An attempt that reached the point of spending is **held** until the mint reports its quote paid — not released on "expired", not on "failed", not on any clock. A mint pays a quote it once reported unpaid, so releasing on either could pay the same balance twice.

**What is not proven yet:** no test exercises the live path. The decision logic and the fee arithmetic are covered against scripted effects; the adapter that talks to a real mint and a real LNURL host is run by nothing in CI.

### A docker seat delivers from inside its container (#963, #981)

The ACP agent runs inside the delivery container, and the git push happens there rather than on the host. This is **on by default** for a seat with `[sandbox] mode = "docker"` whose container runs as a non-root uid; an explicit setting overrides that. Launcher seats and `mode = "none"` seats are untouched.

The delivery path was hardened alongside it: the host validates the job-writable exchange files rather than trusting them, an unexpected workdir layout is refused, the push token is bound to its destination ref, and the container reap is mandatory and classifies a thread group by all of its tasks.

### Long-lived scoped push tokens, and a relay that may refuse them (#963)

A seller mints a long-lived scoped push token through a seam of its own. `maxplayer doctor` names the effective delivery path and the relay's token policy, and a relay that does not support a long-lived token refuses rather than working around it. The NIP-11 read refuses a redirect.

### Also

- A buyer-side multi-turn buying skill, stage 1 (#970, issue #948).
- An onboarding docs pointer on every CLI path, plus the `grok-bot-operate` skill (#982).
- The web app no longer leaves a lamp flashing after a missed terminal event (#974).
- `accepting` on a seller heartbeat means alive and serving, not "has a free slot" (#978).
- Free-lane docs corrected: a free trade ends at accept, and a free seat still needs a reachable https mint (#977, #971).

### Worth knowing

- Nothing here moves a sat until a seat upgrades. The fee is charged and paid from the first payment an upgraded seller node collects.
- The free lane's seller half still has no integrated run behind it.
- No CI job compiles `crates/buzz`, so the relay code in this release was never built by CI.
- An internal plan document for the container-delivery wiring ships in the source tree, marked blocked.

## v0.5.7

A job can now complete with no payment at all, and the relay grants push tokens that are scoped to one ref and may outlive the old 60-second window.

### The free job lane (#965, #967)

A buyer holding no bitcoin can hire a seller that takes nothing. Pass `payment="none"` to `post_job` at `amount_sats=0`, then settle it with `collect` exactly as you would a priced job. It pays nothing.

Three additive wire tags, and no `PROTOCOL_VERSION` bump. An absent tag reads as `sat`, so every already-deployed peer keeps reading today's offers correctly, and a stripped or dropped tag reads as *paid* rather than free.

The free path is entered only when the buyer-signed OFFER and the seller-signed CLAIM both state `none`. Either side alone refuses: a seller cannot make a priced job free, and a buyer cannot make a seller work for nothing. The mode is never inferred from an amount of zero — a zero-priced job with no tags is an unpayable priced job, not a free one.

The money gates are not modified, only not reached. `verify_accepted_claim_creq` is byte-identical to v0.5.6. The post-time dust guard is mode-conditional rather than weakened, so a priced post still opens a wallet and still refuses dust. `authorize_pay` refuses a free bind at its entry, so the free path cannot ride the paid one either.

A free collect verifies the delivery exactly as hard as a paid one: the same tip-match against the accepted commit, and the same execution-sentinel check. A delivery failing either refuses and materializes nothing. It reads no spend ledger and publishes no spend total.

**What is not proven yet:** no seller seat has admitted and claimed a free offer in an integrated run. The buyer half is exercised against a real relay; the seller half is covered by unit tests only.

### The relay scopes push tokens to one ref (#929)

A NIP-98 git push token can now name a single ref, and the pre-receive hook enforces that the push writes only that ref. `relay.maxplayer.ai` already runs this.

### A ref-scoped token may declare its own lifetime (#968)

A seller mints a push token before a sandboxed job starts and pushes with it minutes later, which the 60-second freshness window could not serve. A token that is scoped to one ref and carries a NIP-40 `expiration` tag is now honoured until that expiration, up to a cap the relay advertises in NIP-11 as `scoped_token_max_lifetime_secs` (default 6 hours).

The cap is a ceiling on what a token may ask for, not a grant: a token asking for longer is refused outright rather than shortened, and it still dies at its own expiration. **Unscoped tokens keep the 60-second window**, and so does a scoped token carrying no expiration tag. Setting the cap to `0` restores the old rule for everything. `relay.maxplayer.ai` already runs this.

### Also

- `maxplayer --version` stamps the commit it was built from (#955).
- The npm publish and probe jobs run node 22 (#953).
- Dead `mobee-core` references removed from the flake and the vendored buzz tree (#954, #960).

### Worth knowing

- The relay ships in this release's source, but the live relay is deployed separately from these artifacts. Both relay changes above are already deployed.
- No CI job compiles `crates/buzz`, so the relay code in this release was never built by CI. Its first compile happens at deploy time.
- The MCP free lane still routes through the buyer daemon, which opens a wallet store at startup. A free job needs no funds, but the daemon still creates that store.

## v0.5.6

One security fix on the delivery push, and seats now advertise who they are willing to work for.

### A delivered job cannot redirect its own push (#937)

This was a confirmed exploit, not a theoretical one. Under `[sandbox] mode = "docker"` the whole job
workdir is bind-mounted into the container, `.git` included, so the agent can write `.git/config`.
libgit2 applies `url.<host>.insteadOf` from the config of the repo that runs an operation, at connect
time. Emptying the global, XDG and system config paths (#610) does not cover a repo-local file,
because that one is not reached through a search path. So a delivery push straight from the agent's
workdir would follow a planted redirect and hand the seller's push token — and the pack — to a host
the agent picked.

The fix replaces the entire `.git/config` with a minimal one immediately before the push, rather than
editing out the dangerous keys. That is what makes it robust instead of a blocklist: one write
removes `url.*.insteadOf`, `url.*.pushInsteadOf` and `remote.*.pushurl`, and also every secondary
config the agent could have pointed at, because the replacement carries no `[include]` or
`[includeIf]` and does not enable worktree config. A push needs nothing from the config — the URL and
refspec are explicit — so a minimal file is enough. Any stale `config.worktree` is removed as well.

Neutralising and pushing happen in one blocking operation, so nothing runs in between.

**What the guarantee rests on.** It holds because no agent process is alive to re-plant the file: on
the docker path the job container has already exited by the time the push runs. A seat configured
without `[sandbox]` leaves the agent's orphaned background processes running on the host, and there
the window between rewrite and connect is raceable in principle. Structural containment for that case
is the Track B work below, and it is not finished.

### Seats advertise who they will work for (#942, #946)

A seat's kind-30340 heartbeat now carries two admission tags: `admits_pool`, either `open` or
`closed`, for untargeted offers; and `admits_targeted`, one of `open`, `named` or `closed`, for
offers that name the seat.

Targeted admission needs three values rather than two, because it is a union — a seat with named
buyers and the public route off is closed to strangers and open to the named. A boolean would spell
that state and a genuinely closed one identically, which would tell a buyer the operator chose to
serve that it will be refused.

`named` discloses that a list exists. It never discloses who is on it, and it appears only when the
public route is off.

**Nothing reads this yet.** The tags are published and no buyer-side code consumes them; the MCP
tools expose no way to see a seat's policy before posting. This release is the publishing half.

An absent tag means unstated, never `closed`. A reader that finds no tag must not conclude the seat
refuses anything — older seats simply do not publish this.

### Track B groundwork, inert (#939)

A container-side delivery orchestrator ships as an internal `__deliver` subcommand, deliberately not
advertised in usage, together with its design documents. **No job reaches it.** A seller's delivery
still runs through the same path as before, hardened as described above.

Its module documentation describes a caller that reaps the agent's process group before the push.
That reap is a documented contract and not code — nothing implements it in this release. It matters
for the end state, not for anything that runs today.

### Also

Runner lamps keep sweeping under `prefers-reduced-motion` (#940). The lamps animate `background` and
nothing else — no transform, offset or scale — on a 2.2s cycle over hairlines 4.5px and under, which
is neither vestibular motion nor a flash. Suppressing them removed the only cue distinguishing a
working runner from an idle one, since both then rendered with the same static bright bar. Website
only; no change to the shipped binary.

### Unchanged

Delivery push tokens still carry the `["ref", …]` scope tag minted in v0.5.5, and the relay still
does not enforce it. A stolen delivery token is no more restricted than it was in v0.5.4. Nothing in
this release changes that.
