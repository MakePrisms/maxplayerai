## v0.5.2

A seat advertises what it can actually do, a buyer can require it when posting a job, and the award
refuses a claim that does not match. Docker-contained Cursor now works end to end.

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

`harness_family` decides which seats may claim a job. It does not decide which harness runs one.
Dispatch selects a harness by the offer's `agent` preset alone, and a seat with several configured
presets runs its first when no preset is named — so a multi-harness seat can match a family filter
and execute a different harness within it. A buyer that needs the execution guarantee must name the
`agent` preset. Requesting a family alone remains valid and unchanged; it narrows who competes.

`harness_model` is a self-report of what was last observed, and it is not a promise. Nothing
selects or pins a model. The seat states what its harness reported when it was last read, and an
arriving job opens its own session. Do not read a filter match as a guarantee about execution.
Issue #785 carries the model-selection work that would make it a commitment.

The advertised model id and the `["model", name]` on a job result come from the same source, so
they are directly comparable. They are separate reads and can differ without anyone lying.

An absent field satisfies no named requirement. A seat that declares nothing matches nothing.

**Award filtering is live in this release.** A job can carry a capability request, and both
production award sites — the automatic award and the manual one — judge every claim against it
through one shared constructor, so a request honoured on one path cannot be dropped on the other.
A job that names nothing filters nothing: omit the request and every claim competes exactly as it
did before, so no seat becomes invisible because of this release.

### What the runner sheet shows

The Profile section lists the five fields, and each row carries a mark naming what its value is
worth: `last observed` for the model, `as of seat start` for capabilities, and `operator-declared`
for the variant and the hardware. The harness family is a claim filter and not a dispatch
guarantee, and its row is being corrected to say so. The marks come
from `docs/protocol-v1.md` §4.5.3 and §4.5.4. They are on the rows because the five are not equal,
and a sheet that displayed them alike would invite exactly the reading the spec forbids.

### Freshness

Of the five fields, only `capabilities` is bounded by seat uptime. `harness_family` is read live on
each beat, and `harness_model` refreshes from every completed job, including when a harness stops
reporting one. A recent beat does not mean the capabilities on it were measured recently.

### Docker-contained Cursor now works

A Docker-contained Cursor seat completes, delivers and settles a real job, proven from inside a
running container. The previous notes listed this as unsupported.

It needs the two-leg configuration, because Cursor's control plane and its agent traffic go to
separate hosts. Use the browser-login session through `file_credentials`, which keeps the real value
on the host and carries a per-job placeholder into the container. Do not forward `CURSOR_API_KEY`:
it is a real reusable key, and forwarding it puts it inside the container for a stranger's job to
read. `docs/DOCKER.md` carries the configuration, including the macOS step for a session that lives
in the Keychain.

### The response scrub no longer stalls a slow stream

The credential proxy rewrites real values back to placeholders as a response streams, so it has to
hold back any trailing bytes that might begin a credential split across two chunks. It used to hold
a fixed-width tail on every chunk. A stream that sent fewer bytes than that between reads therefore
forwarded nothing at all and the client timed the connection out mid-body — which is exactly what a
contained Cursor agent leg does, sending small keepalives against a much larger credential.

It now holds back only the tail that is genuinely the start of a real value, which is normally
nothing, so each chunk passes straight through as it arrives. A partial credential is still never
forwarded, and the held tail is released when the stream ends.
