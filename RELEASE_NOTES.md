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
