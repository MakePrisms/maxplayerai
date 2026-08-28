## v0.5.4

The three seller admission controls become additive and independent. **This changes what an existing
seller config means**, so read the upgrade note below before you upgrade a seat.

### ⚠ Upgrading: a seller with no allowlist stops claiming targeted work

Before this release, an empty `accept_offers_only_from` meant accept-all on the targeted surface.
After it, that same config admits no one.

Admission on the targeted surface is now the union of the two controls that govern it — a buyer you
named, or `accept_open_targeted` for one you did not. With no buyer named and `accept_open_targeted`
left at its default of `false`, neither clause admits, and the seat claims nothing.

**Nothing about that state is an error.** The config still parses, the node still boots, the relay
subscription is still live, and the seat still advertises. It simply never claims again. That is the
part worth naming: the failure is silence, and silence is also what a quiet market looks like.

Three ways back in, any one of which is enough:

- name a buyer in `accept_offers_only_from`
- set `accept_open_targeted = true` to take targeted offers from buyers you never named
- set `claim_open_pool = true` to claim untargeted offers

**The node tells you.** A seat whose config can claim nothing prints a warning at boot naming all
three knobs and which of them is off. It is emitted on every boot rather than once, because the
condition is a standing state of the config and not an event — a seat can be restarted long after the
upgrade that closed it. The warning also fires for an allowlist whose every entry is unusable, which
is a list that fences everyone out while looking configured.

**The other direction, if you keep an allowlist.** A seat running a populated
`accept_offers_only_from` together with `accept_open_targeted = true` used to refuse buyers it had not
named, because the allowlist silently cancelled the flag. It now accepts them, which is what that
config always said. If you keep an allowlist and do not want strangers on the targeted surface, check
that `accept_open_targeted` is `false`.

### The three admission controls are additive

`accept_offers_only_from`, `accept_open_targeted` and `claim_open_pool` are now independent. Each
admits on its own and none cancels another.

- **`accept_offers_only_from` always admits.** A listed buyer gets in for targeted work whatever the
  flags say.
- **`accept_open_targeted` additionally admits** targeted offers from buyers you never named.
- **`claim_open_pool` owns the untargeted surface** by itself, in the rate gate.

Previously one fence ran ahead of all of it and applied to both surfaces, so a populated allowlist
cancelled the other two controls outright. The config file said one thing and the seat did another,
with no error to say so.

A refused targeted offer now distinguishes the two cases rather than reporting one string for both.
An operator who wrote a list is sent to the list; an operator who wrote none is sent to the flag,
rather than hunting for a list that does not exist.

The containment readiness gate reads the same rule. "This seat serves strangers" is now exactly
"either open surface is set", so a seat that has opted in to strangers without a working sandbox
fails its readiness check rather than passing on a technicality.

### An undispatchable job is labelled `capability_missing`

A job that reached execution with no serving harness for the harness its buyer asked for was reported
as `execution_failed`. Nothing had run — the dispatch arm sits above execution and returns before it.
`execution_failed` reads as *tried and broke*, which attributes a fault to a run that never happened
and points the buyer at a retry, when the only move that can succeed is finding a seat that serves
the harness.

The arm is post-award, so the reason code decides money rather than only wording. `capability_missing`
is a releasable failure, so a buyer's funds are not stranded behind a job no seat could have run.

### The contribution gate declares the Node suites

`.maxplayer/checks.toml` is the set a seller's attestation runs from the pinned base with the network
denied, and the set a delivery is **paid** against. It declared five cargo rows and no Node rows, so
every test in `web/app` and `web/network` ran zero times under it. A delivery could regress the market
terminal and attest green. Pull requests to `main` caught that; the gate that pays did not.

Both suites are now declared. The default devshell gains Node, because a declared row that cannot
execute is worse than no row at all.

The guard that keeps those rows in place asserts only what a text reader can actually decide — that
each declared form appears exactly once — and says so in the assertion. Whether an arbitrary argv runs
a test suite is not decidable by inspecting its tokens, and two earlier attempts to decide it each
admitted a new false green.

### Self-probes stop leaving diagnostics directories

Every self-probe stamped the wall clock into its own job id, so each probe minted a fresh diagnostics
directory, and nothing pruned them. Probes now use remove-only cleanup: the container is still removed
— an abandoned probe container leaks exactly as an abandoned job container does — and only the
diagnostics capture is dropped.

A capture that was never attempted is now recorded distinctly from one that failed. With those two
collapsed, the signal meaning evidence was destroyed fired on every probe and was buried under the
most frequent event on the node.

**This stops new directories; it does not reclaim old ones.** A seat that has been probing for weeks
keeps whatever it has already accumulated until someone removes it.

### A partially failing advertising probe no longer narrows the roster in silence

The advertising boot check emitted its failure lines after the gate that acts on them, so a probe
where some harnesses failed could quietly reduce the advertised roster with nothing in the log saying
which ones dropped or why. The failure lines now come first. The refusal to advertise a harness
nothing proved is unchanged.
