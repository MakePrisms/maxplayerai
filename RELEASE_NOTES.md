## v0.5.5

Two changes worth reading before you upgrade: a seller can now specialize with an operator-authored
context file, and delivery push tokens carry the ref they were minted for.

### A seller's `MEMORY.md` reaches the job prompt (#828, #932)

An operator-written `MEMORY.md` is loaded into the prompt of every job that seat runs. This is the
first way a seller differentiates itself — brand guidelines, house style, domain notes, anything you
want in front of the agent on every job.

Write it at `$MAXPLAYER_HOME/memory/MEMORY.md`.

**It is inert until you write one.** A seat with no index, or a blank one, composes a prompt that is
byte-identical to the previous release. `memory_enabled` defaults to `true`, so this read path is on
for every seat — but nothing is injected until an operator puts a file there. Creating memory stays
an operator act: the read path only ever reads, and never creates the memory directory.

**It never blocks a job.** An index over the 64 KiB injection bound is refused and the job runs
without memory. An unreadable file is the same. This is context, not a gate, and a job that would
otherwise have been delivered and paid must not die over it.

⚠ **Under `[sandbox] mode = "docker"`, put the content in `MEMORY.md` itself.** Only the index's own
content is inlined into the prompt. The topic files it links live outside the job's mount namespace
and the agent cannot open them, so detail moved into linked files is unreachable for a containerized
seat. The bound is 64 KiB precisely so the index can carry that detail directly.

The retro half of #828 — a model turn after a paid job that writes back to memory — is **not wired**
in this release. `retro_enabled` exists and defaults to `true`, but nothing calls it, so no memory is
written automatically by anything.

### Delivery push tokens carry a ref scope (#930)

The NIP-98 token minted for a delivery push now carries a `["ref", "refs/heads/<branch>"]` tag naming
the single ref it is for. The push refspec and the token's scope are derived from one function, so a
later edit cannot silently split them.

**This is preparation, not yet protection.** Enforcement lives on the relay — it has to read that tag
and refuse a push aimed at any other ref — and that relay change is not deployed. Against the relay
you are talking to today the tag is inert, and a stolen delivery token is no more restricted than it
was in v0.5.4. The scoping becomes real only once the relay side ships; this release is the half that
has to land first.

### Also

Front-page copy now matches what the daemon actually does. The old text said a buyer awards the
runner they want and pays on acceptance; in reality posting awards the first claim meeting your
terms, and settlement follows delivery. Website only — no change to the shipped binary.
