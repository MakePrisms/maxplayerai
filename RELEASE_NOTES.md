## v0.5.3

A Claude seat reports the model that actually ran the turn, instead of the picker preference it was
configured with. An operator can reclaim the containment holders a retired seat left behind.

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

**Display** — `harness_variant` and `hardware`. You declare these yourself, in a top-level `[seat]`
config section. They appear on the announcement **only**: a seller must not put them on a claim, and
a reader must not filter on them. No probe can answer them, and that is the reason for the rule. A
fork name and a machine description are facts about the operator, not measurements, and a buyer
commits satoshis at award against a value nothing could contradict.

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

`harness_model` is a self-report of what was last observed, and it is not a promise. Nothing selects
or pins a model. The seat states what its harness reported when it was last read, and an arriving job
opens its own session. Do not read a filter match as a guarantee about execution. Issue #785 carries
the model-selection work that would make it a commitment.

The advertised model id and the `["model", name]` on a job result come from the same source, so they
are directly comparable. They are separate reads and can differ without anyone lying.

An absent field satisfies no named requirement. A seat that declares nothing matches nothing.

**Award filtering is live.** A job can carry a capability request, and both production award sites —
the automatic award and the manual one — judge every claim against it through one shared constructor,
so a request honoured on one path cannot be dropped on the other. A job that names nothing filters
nothing: omit the request and every claim competes exactly as it did before.

### A Claude seat reports the model that ran the turn

`harness_model` on a Claude seat used to carry the model picker's own value — `default`, `sonnet`,
`haiku`, `opus[1m]`. None of those is a model identity. Each is a preference that resolves to a
different concrete model on a different account, so two seats both advertising `sonnet` can be
running different models, and a buyer filtering on it is awarding against ad copy.

A Claude seat now takes its model id from the harness's own session-init frame, which names the
concrete model — `claude-opus-5[1m]` — before any agent output. That id outranks the configured
picker value. When no frame arrives the field is absent, and absence is absence: no placeholder and
no preference stands in for a measurement that was not made. A run that explicitly picked the one
concrete picker row loses attribution rather than reporting an id nobody observed.

This is scoped to the Claude adapter. Codex reports byte-for-byte what it reported before, and no
other harness changes.

### What the runner sheet shows

The Profile section lists the five fields. Harness family, harness model and capabilities render as
bare label and value pairs. Harness variant and hardware carry an `operator-declared` mark, because
they are declarations rather than measurements.

The three filterable rows no longer carry provenance marks. That is a deliberate simplification and
it costs something worth naming: the sheet no longer shows that `capabilities` is bounded by the
seat's uptime, so a reader cannot tell from this surface alone that a stale toolchain claim is
possible. `docs/protocol-v1.md` §4.5.3 and §4.5.4 remain the authority on what the five fields are
worth, and the three are still not three grades of one proof.

### Freshness

Of the five fields, only `capabilities` is bounded by seat uptime. `harness_family` is read live on
each beat, and `harness_model` refreshes from every completed job, including when a harness stops
reporting one. A recent beat does not mean the capabilities on it were measured recently.

### Reclaiming what a retired seat left

The boot reaper removes only the holders labelled with the booting seat's own key, because on a host
running several seller daemons ownership is the one thing that makes a removal safe. A seat that
never boots again therefore never reaps, and its holders survive indefinitely — one container and one
namespace each.

No local query can close that gap. "This seat is retired" and "this seat is slow to start" produce
identical evidence. The operator holds the missing fact, so the operator supplies it:

```
maxplayer sandbox-reap --seat <64-hex> [--dry-run]
```

The seat id is evidence, not a convenience: there is no default, no fallback to the local identity,
and the host-wide flags a person would reach for — `--all`, `--all-seats`, `--every-seat` — are
refused by name rather than left unrecognised.

The command also reports what it could not remove. A failed `docker rm` used to be dropped, so a host
where one holder resisted removal still answered "no reapable containment holders", exit 0 — a false
statement about the host, on the one line that decides whether the leak is gone. Removed and failed
are now counted separately.

### doctor names a seat that never linked an account

`maxplayer doctor` checks each harness's credential directory. It treated "missing" as acceptable per
PATH, so a cursor seat with no credential directory at any known location passed silently: both
candidates were individually excused. The bound belongs to the harness, not the path. One absent
candidate says nothing, because only the operator's build decides which of cursor's two locations
exists — but a harness with none of its locations present is a seat that never linked an account.

The check groups candidates by harness and reports a group that is entirely absent. A real metadata
error still reads `could not read`; only NotFound folds into the per-harness verdict. The PASS line
names the paths it actually inspected, rather than claiming a property for a directory that does not
exist.

### Running Cursor in a container

A Docker-contained Cursor seat needs the two-leg configuration, because Cursor's control plane and
its agent traffic go to separate hosts. Use the browser-login session through `file_credentials`,
which keeps the real value on the host and carries a per-job placeholder into the container. Do not
forward `CURSOR_API_KEY`: it is a real reusable key, and forwarding it puts it inside the container
for a stranger's job to read. `docs/DOCKER.md` carries the configuration, including the macOS step
for a session that lives in the Keychain.
