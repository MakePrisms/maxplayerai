## v0.5.1-rc1

A seat now advertises what it can actually do, a buyer can filter awards on it, and the runner sheet
shows it. This is a release candidate.

### What a seat operator gets

A seat publishes five capability fields in two classes. The class decides where a field appears and
what a buyer may do with it. The line between them is provenance: a field is filterable only if the
seat measured it. `docs/protocol-v1.md` §4.5 defines the split.

**Filterable** — `harness_family`, `harness_model`, `capabilities`. A buyer's award filter reads
these. They appear on both the kind-30340 announcement and the kind-3402 claim, spelled identically
on each.

- `harness_family` — read from live roster state on every beat.
- `harness_model` — the model id a harness reported when it was last read.
- `capabilities` — toolchain tokens proven by running a probe at seat start.

**Display** — `harness_variant` and `hardware`. You declare these yourself, in a new top-level
`[seat]` config section. They appear on the announcement **only**: a seller must not put them on a
claim, and a reader must not filter on them. No probe can answer them, and that is the reason for
the rule. A fork name and a machine description are facts about the operator, not measurements, and
a buyer commits satoshis at award against a value nothing could contradict.

**Restart a seat after you change its toolchain.** `capabilities` is measured once, at seat start,
and that snapshot is republished for the life of the process. Both directions of drift are real and
only one is safe. If you add a toolchain to a running seat, the seat under-advertises and loses work
it could have won. If you remove one, the seat keeps advertising it and can be awarded work it can
no longer do. Nothing on the wire catches the second case; no event carries a capability back, so no
buyer message can contradict the advertisement. Issue #891 tracks a bounded re-probe.

### What a buyer gets

`harness_family` is exact-or-nothing at dispatch.

`harness_model` is a self-report of what was last observed, and it is not a promise. Nothing
selects or pins a model. The seat states what its harness reported when it was last read, and an
arriving job opens its own session. Do not read a filter match as a guarantee about execution.
Issue #785 carries the model-selection work that would make it a commitment.

The advertised model id and the `["model", name]` on a job result come from the same source, so
they are directly comparable. They are separate reads and can differ without anyone lying.

An absent field satisfies no named requirement. A seat that declares nothing matches nothing.

**Award filtering is present and inert in this release.** The machinery that selects a claim on
advertised capability ships here, but no offer carries a capability request yet: both production
award sites pass an empty request, so every job selects the same claim it would have selected
before. Award behaviour is unchanged. Nothing you run today starts filtering, and no seat becomes
invisible because of this release.

### What the runner sheet shows

The Profile section lists the five fields, and each row carries a mark naming what its value is
worth: `enforced at dispatch` for the harness family, `last observed` for the model, `as of seat
start` for capabilities, and `operator-declared` for the variant and the hardware. The marks come
from `docs/protocol-v1.md` §4.5.3 and §4.5.4. They are on the rows because the five are not equal,
and a sheet that displayed them alike would invite exactly the reading the spec forbids.

### Freshness

Of the five fields, only `capabilities` is bounded by seat uptime. `harness_family` is read live on
each beat, and `harness_model` refreshes from every completed job, including when a harness stops
reporting one. A recent beat does not mean the capabilities on it were measured recently.

### Known limitation: Docker-contained Cursor

Docker-contained Cursor seats are not supported in v0.5.1-rc1. Such a seat cannot complete its
pre-advertise probe, so the seller refuses to advertise and the seat accepts no work. That refusal
is deliberate and fail-closed: a seat that cannot prove its harness does not take jobs.

Three causes are identified:

- The credential proxy's upstream leg cannot negotiate HTTP/2. Cursor's agent endpoint serves
  HTTP/2 only and returns no HTTP/1 response at all, so the upstream connection is never
  established.
- The proxy buffers a request body before forwarding it. An agent stream does not close its body,
  so the request is never forwarded.
- The proxy forwards every request to the single upstream a credential names, while Cursor's
  control plane and agent leg use different hosts.

None of the three is fixed in this release.

Uncontained Cursor seats and other harnesses are outside the scope of this limitation; it describes
Docker-contained Cursor only.
