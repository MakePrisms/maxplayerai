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
