# gVisor named-network DNS + container delivery — run log

Lane: `w-gvisor-dns-delivery-r2` · ordering seat: maxie (requested by Bob)
Brief: `/Users/forge/forge/v2/maxie/runs/gvisor-dns-delivery-brief-20260909.md`
(3,851 B, sha256 `3b0a20feab6ae107e9ddc28cb70b30b1a73656c15fa95c72abc62edd25430812`, verified 2026-09-09)

Six acceptance gates. Nothing below is claimed as passing unless the command
output is recorded here or in `evidence/`.

## Environment reality (2026-09-09, recorded before any test)

| Fact | Value |
| --- | --- |
| Forge host | macOS 26.4.1, arm64 (Apple silicon) |
| Reported failing host (Bob) | Ubuntu 24.04, kernel 6.8.0-124, x86_64, Docker 29.8.0, runsc release-20260831.0 |
| Shared container runtime on this host | colima `default`, aarch64, Docker 29.7.2 client / 29.5.2 server — **off limits**, other lanes use it, no runsc install there |
| Disposable Linux for gates | lima VM `gvisor-repro`, Ubuntu 24.04 (noble release-20260705), aarch64, vz driver, 2 CPU / 4 GiB / 20 GiB |

**Architecture divergence is named, not papered over.** The reproduction host
available to this lane is aarch64; the reported evidence is x86_64. Any gate
result carries the arch it was executed on. If a failure mode proves x86-only,
that is reported as a limitation, not as a pass.

## Source map (read before edits)

- `crates/maxplayer-core/src/sandbox_net.rs` (1126 lines) — renders the egress
  policy. Already documents that loopback must never be denied because docker's
  embedded DNS answers at `127.0.0.11` inside the namespace, and carries a
  load-bearing test for it.
- `crates/maxplayer-core/src/sandbox_netns.rs` (939 lines) — puts the policy in
  force via holder → sidecar → job, all sharing one network namespace. Header
  asserts "name resolution is unaffected: a container joining the namespace
  still gets its own `/etc/resolv.conf` pointing at docker's embedded resolver
  on `127.0.0.11`". That assertion is exactly what the reported failure
  contradicts under runsc, so it is the first thing to test rather than trust.
- `crates/maxplayer-core/tests/sandbox_netns_live.rs` (840 lines) — existing
  live tests.

## Timeline

- 17:39 PDT — brief read, hash verified, worktree + branch created off
  immutable commit `b45f8651dc9cab5b71c962eadd7b84840f5791061dc9cab5b71c962eadd7b84840f579106`.
- 17:41 PDT — disposable lima VM `gvisor-repro` creation started (Ubuntu 24.04
  cloud image, arm64). Shared colima VM deliberately untouched.
- 17:46 PDT — **failure reproduced** (`evidence/gate1-runsc-vs-runc-20260910T0046Z.txt`).
- 17:52 PDT — root cause isolated and a containment-preserving fix validated by
  hand before any code was written.

## Gate 1 result — reproduced (aarch64)

Exact run recorded in `evidence/gate1-runsc-vs-runc-20260910T0046Z.txt`.
Identical image, identical named bridge, identical container security settings;
only `--runtime` differs:

| Probe | Result |
| --- | --- |
| `dns.lookup(relay.maxplayer.ai)` under `--runtime runsc` | `ERR EAI_AGAIN`, exit 1 |
| same under `--runtime runc` (control) | `OK 34.225.223.145`, exit 0 |
| raw UDP datagram to `127.0.0.11:53` under runsc | **TIMEOUT — no answer at all** |

Both containers were handed the *same* `/etc/resolv.conf`
(`nameserver 127.0.0.11`, docker's embedded resolver, `ExtServers:
[host(127.0.0.53)]`). Digests: image
`sha256:1c50e46a35dfe91fcdbbba11876bff312a95567bda98d6dcb7f675c884777412`
(arm64/linux), docker 29.1.3, runsc release-20260817.0, kernel 6.8.0-134,
network subnet 172.18.0.0/16.

**Causal source.** The failure is not resolver policy, not the allowlist, and
not name-specific: a bare UDP packet to `127.0.0.11:53` gets no reply inside the
sandbox. Docker's embedded DNS on a *user-defined* network is a socket bound by
the daemon inside the container's network namespace on `127.0.0.11:<ephemeral>`,
reached through NAT rules installed in that namespace. Under runsc the sandbox
runs its own network stack and terminates loopback inside the sentry, so those
packets never reach the namespace-side rules or the daemon's socket. Under runc
the container shares the host kernel's stack, so they do. That is the whole
delta, and it explains why the shared job namespace fails identically — the
holder's namespace has exactly the same embedded resolver.

**`--dns` does not fix it** (measured): with `--dns 1.1.1.1` on a user-defined
network docker *still* writes `nameserver 127.0.0.11` and merely forwards
upstream from the daemon side, so the container still fails `EAI_AGAIN`. Any
fix that only sets docker DNS flags is theatre.

**Validated fix direction** (measured, same runsc runtime, same named network,
same `--user 65534:65534 --cap-drop ALL --security-opt no-new-privileges`):
supply the sandbox its own `/etc/resolv.conf` naming real upstream resolvers,
read-only, instead of the unreachable embedded one. Result: `lookup: OK
34.225.223.145`, `tls: 200 cert-verified`. Containment is untouched — still the
named bridge, no host networking, no runc, no added capability. The egress
policy must then explicitly permit port 53 to exactly those resolver addresses
and nothing wider.

## Gate status

| Gate | State |
| --- | --- |
| 1 repro + runc control, digests, causal evidence | **done (aarch64), evidence committed** |
| 2 DNS + TLS from real shared job namespace, fresh + recreated | not started |
| 3 doctor/readiness on the real sandbox route + regression tests | not started |
| 4 real container-side Git delivery, remote hash match | not started |
| 5 private/metadata denial + concurrent public success | not started |
| 6 bounded gate script, executed test counts, PR | not started |

## 2026-09-10 — gate 5 turned up a hole bigger than the one I was sent for

Gate 5's cross-job leg failed: from a runsc job in a namespace carrying the full
26-rule plan, a container at `172.31.12.3:8080` — inside `-d 172.16.0.0/12 -j DROP` —
was **REACHED**.

`gate5b-does-the-plan-bind-a-runsc-job.sh` isolates it. Same namespace, same plan
(read back from the netns: the DROP is rule 11/12 and it is there), same probe,
one variable — the job's runtime:

| job runtime | result to a listener inside a DROPped range |
| --- | --- |
| runc  | `timeout` — the DROP is enforced |
| runsc | `REACHED` — the DROP is not |

**The per-job egress plan does not bind the runtime it was written for.** gVisor's
netstack terminates the network inside the sandbox and writes frames to the veth
itself; the host kernel's OUTPUT chain in that netns only sees packets from host
sockets, so it never sees the job's. The chain is installed, correct, verified by
readback — and irrelevant to a gVisor job.

This is not a regression from the DNS work; it predates this branch. The DNS
change opens port 53 to a `/32` in a chain that was already not constraining the
job.

### It also devalues part of my own gate-2 evidence
Gate 2 recorded `metadata: denied (ENETUNREACH)`. I read that as policy. It is
not evidence of policy: nothing listens on `169.254.169.254` in this VM, and
**absence is indistinguishable from enforcement** unless the denied destination
has a live listener. Every denial leg in gate 5 that "passed" against a dead
address proves nothing. Only the neighbour leg — a real listener inside a real
DROP range — was a valid test, and it failed.

Rule for the remaining gates: a denial is only proven against a destination that
answers when it is allowed to.

### Where containment has to live instead
Not in the netns OUTPUT chain. The candidate that gVisor cannot bypass is the
host side of the veth: FORWARD-chain rules in the root netns keyed to the job
namespace's source address, and/or a per-job network rather than one shared
`maxplayer-sbx` bridge (all jobs currently share it, which is why job A could see
job B at all). Both need measuring before either goes in.

## 2026-09-10 — the shared job namespace is SINGLE-USE for gVisor

`gate5c` tried to measure the two candidate enforcement sites and returned
`ENETUNREACH` for everything late in the run — including the **runc control**,
which had worked minutes earlier in that same namespace, and including DNS to
`1.1.1.1`, which no rule under test touched. A control that dies is not a
control, so gate5c's verdicts are **void**.

`gate5d` settles why. One namespace, read with `os.networkInterfaces()`:

| moment | interfaces | dns |
| --- | --- | --- |
| before any gVisor container | `lo=127.0.0.1 eth0=172.31.16.2` | `34.225.223.145` |
| during the runsc job | `lo=127.0.0.1 eth0=172.31.16.2` | `34.225.223.145` |
| after it exits, via runc | `lo=127.0.0.1` | `EAI_AGAIN` |
| after it exits, via a second runsc job | `lo=127.0.0.1` | `EAI_AGAIN` |

**A gVisor container takes the namespace's addresses into its netstack and does
not give them back when it exits.** The namespace is usable exactly once. The
second container to enter it — whatever runtime — finds a namespace with nothing
but loopback.

### What this voids, and what survives
- **VOID**: gate5c, both candidates. Measured against a dead namespace.
- **VOID**: gate 5's git leg (`Could not resolve host: github.com`). It was the
  second runsc container in that namespace, not a DNS bug.
- **STANDS**: gate5b. Its runc control ran FIRST, while the namespace was
  healthy, and was correctly dropped; the runsc probe was the first gVisor
  container in that namespace. The finding holds: the plan binds runc and not runsc.
- **STANDS**: gate 4 and gate 2 — one runsc container per namespace in each.
- **STANDS**: the gate-2 retraction, for the separate dead-address reason.

### Rule for every remaining measurement
One gVisor container per namespace, and a health check of the namespace
immediately before any leg whose result is meant to be evidence. Re-run gate5c
under that rule before either enforcement site is chosen.

## 2026-09-10 — gate5c, re-run soundly: DOCKER-USER does not bind a bridged gVisor job; a per-job network does

Rewritten under the one-gVisor-container-per-namespace rule, with a health check
printed beside every leg. Every leg below ran in a namespace verified HEALTHY
(address present, DNS resolving) immediately before the probe, against a LIVE
listener.

| leg | runc | runsc |
| --- | --- | --- |
| live neighbour, same bridge, no host rule | `timeout` | **`REACHED`** |
| live neighbour, same bridge, `DOCKER-USER -s <ns>/32 -d 172.16.0.0/12 -j DROP` | `timeout` | **`REACHED`** |
| live neighbour, **other bridge** | — | `timeout` |

With that DOCKER-USER rule in place the public route the job must keep is
untouched: `PUBLIC-PASS dns=34.225.223.145 tls=200 verified=true` and
`PUBLIC-PASS git 7fd1a60b…`.

**(a) DOCKER-USER does not bind a gVisor job talking to a peer on its own bridge.**
The mechanism is in the environment line: `br_netfilter=absent`. Without
`bridge-nf-call-iptables`, frames switched between two containers on the same
bridge never enter iptables' FORWARD path at all, so DOCKER-USER cannot see them.
The runc leg's `timeout` in that row is its own netns OUTPUT plan doing the work,
not DOCKER-USER — which is exactly why a control that only ever shows "denied"
proves nothing about the site under test.

**(b) A per-job network does bind it.** Cross-bridge traffic is routed rather than
switched, so it meets DOCKER-ISOLATION in the root netns and is dropped, gVisor
or not.

### The shape of the fix this implies
A per-job network is not merely nicer isolation, it is what makes host-side
enforcement possible at all: once the job's only on-link peer is its gateway,
every other destination is ROUTED, and routed packets from a gVisor sandbox do
traverse the host's FORWARD path where a source-keyed policy can bind them.
Shared bridge + gVisor is the combination with no enforcement point.

Still to measure before any of this is written into product code: with a per-job
network, does a source-keyed DOCKER-USER policy actually deny a runsc job a
ROUTED private destination that docker isolation does not already block, and
does it deny the metadata address? Host-directed traffic (the VM's own
`192.168.5.15`) lands in INPUT, not FORWARD, and needs its own answer.

## 2026-09-10 — gate5e: the enforcement sites, named

Same discipline: fresh holder+plan namespace per probe, one gVisor container in
each, health printed beside every leg, live listeners.

**1. The host itself (`192.168.5.15:49252`, a real listener, reached by route)**

| leg | result |
| --- | --- |
| runc, bare | `timeout` — its netns plan drops 192.168/16 |
| runsc, bare | **`REACHED`** |
| runsc, `DOCKER-USER -s <ns>/32 -d 192.168.0.0/16 -j DROP` | **`REACHED`** |
| runsc, `INPUT -s <ns>/32 -d 192.168.0.0/16 -j DROP` | `timeout` |

A gVisor job can reach the host's own LAN address today, and DOCKER-USER cannot
stop it: host-directed packets are delivered locally, so they land in **INPUT**
and never traverse FORWARD. INPUT binds it.

**2. The metadata address.** Nothing listens on it here, so only the difference
between runs is evidence — and there is one: bare `ECONNREFUSED`, with
`DOCKER-USER -s <ns>/32 -d 169.254.169.254/32 -j DROP` `timeout`. The rule
changes the outcome, so DOCKER-USER does bind the metadata address for a runsc
job. It is reached by route through the gateway, which is why FORWARD sees it
and the same chain was useless for the host and for a same-bridge peer.

**3. Cross-job, each job on its own network:** runsc → job 2's live listener,
`timeout`. Per-job networks are the cross-job answer.

**4. The public route survives all of it:** with both rules installed,
`PUBLIC-PASS dns=34.225.223.145 tls=200 verified=true` and
`PUBLIC-PASS git 7fd1a60b…`.

### The fix, now fully specified by measurement
Three parts, none of which replaces the others:
1. **A per-job network**, not the one shared `maxplayer-sbx` bridge. It contains
   job-to-job traffic and it converts everything else into routed traffic that
   host chains can see at all.
2. **A host-side, source-keyed policy** applied daemon-side for the job
   namespace's address: DOCKER-USER for routed destinations (metadata, private
   ranges off-link) and INPUT for host-directed ones.
3. **The existing netns OUTPUT plan, kept** — it is what binds a runc job, and
   it costs nothing to leave in place as defence in depth.

## Gate 5f — the product's own host policy, measured against the hole it was built for

`scripts/gate5f-product-host-rules-bind-runsc.sh`, evidence in
`evidence/gate5f-evidence.txt`. aarch64, kernel 6.8.0-134, docker 29.1.3,
runsc release-20260817.0, `br_netfilter=absent`, daemon `default-runtime=runc`.

The rules under test are **rendered by the product**, not written by the script:
`cargo run -p maxplayer-core --example render_host_plan -- 172.31.21.10`, from
`HostPolicy`, 17 rules (`evidence/gate5f-host-install-rendered.txt`), plus the
exact inverse for teardown. The probe namespace's address is pinned with
`--ip` and the script **refuses to run** unless the rendered plan carries
`-s 172.31.21.10/32`; a plan keyed to the wrong address would deny some other
container while leaving this job open, and that mistake would otherwise be
invisible. They are installed by piping the plan into the **same applier image
the sidecar uses**, in a `--network host` container — the product's path, so
the image's ability to write the root namespace's chains is itself under test.

### What changed, and what did not
| leg | before | after | what actually binds it |
|---|---|---|---|
| runsc → host `192.168.5.15:49253` (live) | REACHED | timeout | **the new INPUT rules** |
| runc → host `192.168.5.15:49253` (live) | timeout | timeout | the netns OUTPUT plan, already |
| runsc → own-network neighbour (live) | timeout | timeout | the per-job network + DOCKER-ISOLATION |
| runsc → same-bridge neighbour (live) | REACHED | REACHED | **nothing — and that is the point** |

Only the first row is a delta produced by the host policy, and it is the exact
hole gate5e found: a gVisor job reaching the host itself, which DOCKER-USER
cannot stop because host-directed packets never traverse FORWARD. Gate5e
supplies the matching delta for DOCKER-USER (metadata: bare `ECONNREFUSED` vs
ruled `timeout`). Together the two chains of the rendered plan are each caught
working, on evidence, against a runsc job.

The rows that did not change are recorded deliberately. The own-network row was
already denied before the policy was installed, so **this gate does not prove
the host rules deny cross-job traffic** — the per-job network does that, and
claiming otherwise would repeat the gate-2 error of reading an absence as
enforcement. The same-bridge row stays REACHED because switched frames enter no
chain on a host without `br_netfilter`. That is not a defect in the policy; it
is why the product gives every job its own network instead of trying to rule
its way out of a shared one, and it is measured here so that reason stays
evidenced rather than asserted.

### The route survives
With the full policy installed: `PUBLIC-PASS dns=34.225.223.145 tls=200
verified=true` and `PUBLIC-PASS git 7fd1a60b01f91b314f59955a4e4d4e80d8edf11d`.

### Teardown leaves no trace
The rendered teardown plan removed all 17 rules; host readback for the job
address went 9/8 → 0/0, and both chains returned to depth 1, the depth they
had before the run. This is the failure mode `HostRules`' drop guard exists
to prevent: a leaked container gets noticed, a leaked rule in a shared chain
does not, and a recycled address would inherit a dead job's policy.

## Gate 5 — denial holds, and three concurrent jobs still deliver

`scripts/gate5-denial-and-concurrent-success.sh` (rewritten),
evidence `evidence/gate5-denial-and-concurrent-success-PASS-20260910T0311Z.txt`.
The first version's FAIL is kept beside it as
`…-20260910T0245Z.txt`. aarch64, kernel 6.8.0-134, docker 29.1.3, runsc
release-20260817.0, `br_netfilter=absent`, daemon `default-runtime=runc`.
Five namespaces, each on its own network, each with rules rendered by the
product and guarded against a stale plan by source key. **Verdict: PASS,
0 denial legs failing.**

### Denial, before and after the host policy
| destination | before | after | attributable to |
|---|---|---|---|
| host `192.168.5.15:49254`, **live** | REACHED | timeout | **the host policy (INPUT)** |
| `denied-lan.maxplayer.test:49254` → same live listener, **by name** | REACHED | timeout | **the host policy (INPUT)** |
| neighbour job `172.31.34.10:8080`, **live** | timeout | timeout | the per-job network, not the policy |
| `169.254.169.254:80`, nothing listens | ECONNREFUSED | timeout | **the host policy (DOCKER-USER)** — a real difference |

Two legs are new evidence and two are careful non-claims. The by-name leg is
coverage no earlier gate had: a job that reaches a denied address through a
NAME is denied exactly as one that dials the address, which matters because
every real exfiltration attempt is a hostname. It uses a mounted hosts file
rather than a third-party wildcard DNS service so the leg cannot pass or fail
for an unrelated reason.

The metadata leg is evidence only because bare and ruled runs DIFFER
(`ECONNREFUSED` → `timeout`). With nothing listening there, an identical
result in both sections would prove nothing, and this is the same reasoning
that made me retract gate 2's "metadata denied" line rather than defend it.

The neighbour leg was already denied before the policy went in, so the host
policy is **not** credited with it. The per-job network plus docker's own
isolation does that, and gate5c is where it was isolated.

### IPv6 — measured, and left unproven on purpose
The job namespace has no global IPv6 address at all (`NO-V6` under both
runtimes), so there was nothing here to deny and nothing was proved. The
host-side plan deliberately renders no ip6tables rules, because `DOCKER-USER`
is not guaranteed to exist in the v6 table. **Host-side IPv6 denial for a
gVisor job is UNPROVEN and must not be claimed.** The netns plan does cover
v6, and its readback is verified per family at install time, but gate5b showed
the netns plan does not bind a runsc job — so on a host where the job DOES get
a global v6 address, this is an open hole and is listed as such in the
limitations.

### Concurrent delivery
Three jobs at once, each with its own network, holder, netns plan and
host policy installed simultaneously (17 rules each, all reading back on the
host kernel). Every one delivered:

```
c1: OK dns=34.225.223.145 tls=200 verified=true  OK git 7fd1a60b…
c2: OK dns=34.225.223.145 tls=200 verified=true  OK git 7fd1a60b…
c3: OK dns=34.225.223.145 tls=200 verified=true  OK git 7fd1a60b…
```

Denial and delivery are therefore not in tension: the policy that blocks the
host, the metadata address and a denied name by name is the same policy under
which three jobs resolved, verified a certificate and cloned a repository at
the same time.

### No leaks
All four policies torn down by their rendered inverse; the host kernel went
from 17 rules each to 0, and `DOCKER-USER`/`INPUT` returned to depth 1, where
they started.

## Gate 6 — reproducibility, and what this branch does NOT prove

`scripts/run-all-gates.sh` runs the set bounded, writes each gate's output to
its own evidence file, and prints a verdict per gate. Proof run (aarch64,
gate1 + gate5f): `ALL GATES: PASS` in 61s —
`evidence/run-all-gates-proof-summary-20260910T0316Z.txt`. I checked the
per-gate logs rather than the summary line, because 61s looked too fast for a
suite that had taken minutes before; both are complete runs, the speed being
warm images. A full five-gate run through the runner was not executed in one
sitting; gates 1, 2, 4, 5 and 5f each have their own full-run evidence file
from a direct run, and the runner is proved on two of them. **Saying which is
the point of this section.**

### Limitations — each one a thing a reader should not assume
1. **x86_64 is OUTSTANDING.** Every result here is aarch64, kernel 6.8.0-134,
   docker 29.1.3, runsc **release-20260817.0**. The host is arm64 and runsc
   release-20260831.0 ships no aarch64 artifact, so neither a newer runsc nor
   a different architecture was tested. gVisor's netstack behaviour is the
   whole subject of this branch, and it is exactly the kind of thing that can
   differ per platform.
2. **`br_netfilter` is ABSENT on this host**, and that shaped the measurements.
   It is why a same-bridge peer is reachable and unbindable here. Where it is
   enabled, switched frames do enter the chains and that leg may read
   differently. The per-job network makes the product correct either way — with
   no on-link peer, the case does not arise — but the MEASUREMENT is
   host-specific and should not be quoted as universal.
3. **Host-side IPv6 is not rendered, and v6 denial for a gVisor job is
   UNPROVEN.** `HostPolicy` deliberately emits no ip6tables rules because
   `DOCKER-USER` is not guaranteed to exist in the v6 table. The netns plan does
   cover v6 and its readback is verified per family — but gate5b showed the
   netns plan does not bind a runsc job. In this VM the job namespace has no
   global v6 address, so nothing was denied and nothing was proved. **On a host
   whose jobs do get one, this is an open hole.**
4. **The container-side git push uses an unauthenticated disposable remote, by
   design.** A credentialed push would mean putting a secret inside a
   stranger's sandbox. Gate 4 therefore proves the network path for a write,
   not an authenticated push.
5. **The runtime boundary is baseline, not new — but it deserves review.**
   Holder, sidecar and the host-rule applier carry no `--runtime` and inherit
   the daemon default; only the JOB carries the configured runtime. That is
   true of that baseline commit too: `sandbox_netns.rs` there has no `--runtime` in
   `holder_argv`/`sidecar_argv`, and `seller_exec.rs` `run_argv` (lines
   681–687) emits it for the job alone. This branch adds the test that pins it
   (`the_containment_plane_never_carries_the_jobs_runtime`). An operator who
   sets `default-runtime=runsc` gets a runsc holder; measured, that fails
   CLOSED (the job sees `lo` only). None of the three helpers executes any
   seller- or task-controlled input: the holder is `--entrypoint sleep … infinity`,
   the other two read a plan rendered in Rust.
6. **The `--network host` rule applier is the one privileged surface this
   branch adds.** It must be `--network host` because the rules have to land in
   the root namespace's chains, which is the only place a gVisor job's packets
   can be seen. It runs our own image, on a Rust-rendered plan, for
   milliseconds, and it is gone before the job starts; the job never touches
   it. That is my judgement and it should not be only mine — **advisor review
   is requested on this specifically.**

### Retractions kept in the record
Gate 2's "metadata denied (ENETUNREACH)" line is **withdrawn**: nothing listens
there, so absence was read as enforcement. Gate5c's first run is **VOID**
(namespace reuse under gVisor) and its file is kept marked VOID. The first
gate 5 script FAILED and that evidence is kept beside the passing rewrite.

## Gate 5g — the baseline vulnerability, reproduced on that baseline commit

Maxie's ruling: *"Prior-baseline vulnerability claim needs a baseline
reproduction/source citation."* Correct demand — the branch had been asserting
"this predates the fix" from source reading alone. Here is the reproduction.
`scripts/gate5g-baseline-vulnerability-repro.sh`, evidence
`evidence/gate5g-baseline-vulnerability-CONFIRMED-20260910T0330Z.txt`.

Built to be unfair to my own claim:
* **Baseline rules.** Rendered from immutable commit `b45f8651dc9cab5b71c962eadd7b84840f5791061dc9cab5b71c962eadd7b84840f579106` by a throwaway
  example compiled against BASELINE's `NetPolicy` (three fields, no
  `dns_resolvers`) — 24 rules, kept as
  `evidence/gate5g-baseline-plan-from-origin-main.txt`. A renderer built
  against the fix would reproduce the fix, not the bug. The script refuses to
  run if the staged plan carries resolver pinholes, which only the fixed plan has.
* **Baseline arrangement.** ONE shared network, as a single `[sandbox] network`
  produced, and **no host-side rules at all** — that baseline commit has no
  `HostPolicy` (grep: 0 occurrences). The script installs none.
* **Positive controls.** `runc` runs the identical probe. Had the plan failed to
  bind runc too, the finding would be "the plan was never installed".
* **Readback.** Every attacker namespace prints `applier=24/24` and a kernel
  readback of the `172.16.0.0/12` rules, so a REACHED cannot mean "no rules".

### Result on `b45f8651dc9cab5b71c962eadd7b84840f579106`
| leg | runc (control) | runsc | |
|---|---|---|---|
| cross-job → live victim `172.31.40.20:8080` | timeout | **REACHED** | containment failure |
| private egress → live host `192.168.5.15:49255` | timeout | **REACHED** | containment failure |

**BASELINE-VULNERABLE: CONFIRMED, 2 findings.** The netns plan binds a runc job
and not a gVisor one, and baseline has no second line of defence. The branch
does not introduce this; it repairs it.

### A confounded first run, kept
The first attempt read `timeout` on the cross-job leg and looked like baseline
containment working. It was nothing of the kind. The victim carries the
baseline plan too, and its listener was running under **runc** — so the plan
bound the *victim*, and its own OUTPUT rules dropped its replies to a
172.16/12 peer. The probe measured a victim that could not answer, not an
attacker that could not reach.

Fixed by running the victim's listener under **runsc**, which is the real
arrangement on a baseline seat (every job is a gVisor job), and by printing a
`VICTIM-SERVING` liveness check from inside the victim's own namespace before
any conclusion is drawn from a timeout. The confounded run is kept as
`evidence/gate5g-baseline-repro-CONFOUNDED-runc-victim-20260910T0325Z.txt`.

It is the same failure this branch has now hit three times in different
clothes — gate 2's metadata line, gate5c's voided run, and this — and the
lesson is identical each time: **a timeout is only evidence when something was
proved able to answer.**

## Gate 5h — lifecycle: teardown, recycled address, recreation

Maxie: *"Prove selected enforcement path handles runsc, lifecycle
cleanup/recreation and fail-closed setup/readiness."* Gate 5 proved the rules
**deny**. It never proved they **go away**, and that gap is dangerous in both
directions: a rule keyed to job A's address does not stop existing when A does,
and docker hands addresses back — so the next job to get `172.31.55.2` would
inherit a firewall written for a stranger (traffic denied that should be
allowed, or an ACCEPT pinhole that was A's proxy and is now someone else's open
door). A network that fails to delete wedges the next job with the same id.

`scripts/gate5h-lifecycle-and-recycled-address.sh`, evidence
`evidence/gate5h-lifecycle-recycled-address-PASS-20260910T0332Z.txt`, plans
`evidence/gate5h-plans/` (17 install / 17 teardown, rendered by
`render_host_plan`, never transcribed).

**GATE 5h: PASS — 0 failing checks** (aarch64, runsc release-20260817.0):

| check | result |
|---|---|
| A establishes → host rules appear | 0 → **17** rules keyed to `172.31.55.2` |
| A contained while installed | runsc → live host `timeout` |
| A tears down → rules gone | **0** rules survive |
| A tears down → network gone | removed |
| B **recycles A's address** `172.31.55.2` | genuinely recycled |
| B inherits stale firewall? | **0** stale rules |
| B bare (control) → live host | **REACHED** |
| B after its own rules → live host | **ENETUNREACH** |
| same job id twice | both came up, no "already exists" wedge |
| chains returned to start depth | 2 → 2, nothing leaked |

### What each leg is worth
The recycled-address leg is the strong one, and it is the reason the gate
exists: a **bare-vs-ruled difference measured against a live listener**
(`REACHED` → `ENETUNREACH`), on an address that a previous job had owned. It
rules out both "the listener was never reachable" and "the old rules were doing
the work".

Leg 1 proves less and should be read that way: it measures job A only with its
rules installed, with no bare control, so on its own it is consistent with the
host simply being unreachable from that namespace. It is corroboration, not
proof; the bare control in leg 3 and gate5f's `REACHED → timeout` carry the
weight.

Note the two denials read differently — `timeout` for A, `ENETUNREACH` for B.
Both are denials, and the difference is not yet explained; it is most likely
DROP versus an unreachable route at the moment of probe. Recorded as an
observation, not a claim.

## Fail-closed setup — what the product does, and what now holds it

Maxie: *"…and fail-closed setup/readiness."* Read back from
`sandbox_netns.rs::establish()`, the host-side path refuses the job at **four**
distinct points rather than warning and continuing:

1. the holder's address cannot be read → `could not read the job namespace's address`;
2. the address comes back **empty** → refused, because a policy with no source
   key `would deny the range host-wide`;
3. the host-rule applier fails → `host-side containment was not installed`;
4. the applier's count ≠ the rendered count → `host-side containment is
   incomplete … the plan was truncated in transit`.

`HostRules` is adopted **before** the applier's result is examined, so a plan
that failed part-way still has its rules removed on the way out. A namespace
readback (#797 R1) then asks the kernel directly, because everything above it
is the installer's own account of its work.

### The gap that was there
All four guards were held by **reading the source**, not by tests. Gate 5h
measured the teardown once, on one host. Four rendering tests now lock the
invariants on every build (`sandbox_net.rs`):

* `the_host_teardown_exactly_inverts_the_install` — same length, `-I`↔`-D`,
  reverse order, every field otherwise identical. If these two plans drift,
  teardown leaks rules into a shared chain and a recycled address inherits a
  dead job's firewall.
* `every_host_rule_is_keyed_to_the_job_address` — both directions; a rule
  without `-s` is a deny for the whole range on a chain shared with every
  container on the daemon.
* `the_rendered_host_plan_counts_exactly_what_it_renders` — the count guard 4
  relies on; if rendered count and line count could disagree, a truncated plan
  would pass the cross-check.
* `an_empty_job_address_renders_a_source_key_that_is_not_a_host` — asserts the
  *hazard* (bare `/32`), documenting why guard 2 must never be relaxed.

### Test counts, corrected and verified
Earlier notes said "60 sandbox + 55 doctor". The doctor tests are **not** in
`maxplayer-core` — that crate has a `doctor` module with zero `#[test]`. They
live in the `maxplayer` **binary** target. Verified this run:

| suite | command | result |
|---|---|---|
| core lib (all) | `cargo test -p maxplayer-core --lib` | **387 passed, 0 failed** |
| sandbox filter | `cargo test -p maxplayer-core --lib sandbox` | **65 passed, 0 failed** |
| doctor | `cargo test -p maxplayer --bins doctor` | **55 passed, 0 failed** (73 filtered) |

The four new tests were confirmed **by name** in the output, not inferred from
the total moving.

## Gate 5i — fail-closed, and a defect this branch introduced

`scripts/gate5i-fail-closed-live.sh`, evidence
`evidence/gate5i-fail-closed-FAIL-teardown-leak-20260910T0339Z.txt`.
**GATE 5i: FAIL, 2 failing checks.** Kept as a FAIL, unfixed in this commit,
because it found something real.

### What passed
* **A — applier cannot start** (bad image tag): 0 rules installed. `establish()`
  maps this to a hard error.
* **B — applier without `NET_ADMIN`**: `apply-policy: namespace is PARTIALLY
  configured — destroy the holder, do not retry`, **0** rules installed.
* **C — truncated plan**: the code comment claimed a truncated plan "applies
  cleanly and exits 0, so no exit code reveals it". **Confirmed, measured**:
  exit `0`, applier reported `9`, kernel held **9 of 17**. The count
  cross-check is genuinely the only thing that catches it. And the question
  that matters — with 9 of 17 rules, `runsc` → live host `192.168.5.15:49257`
  came back **REACHED**. A partially-installed policy leaves the job
  **uncontained**, so refusing the job is the only safe response.

### What failed — and it is mine
* **D — teardown of a partial install left all 9 rules in place.** Chain depth
  went **2 → 11**. The gate leaked rules into a chain shared with every
  container on the daemon.

Mechanism, confirmed directly against the image rather than inferred:

```
apply-policy: rule 1 failed: iptables -D DOCKER-USER -s 10.99.99.99/32 ...
apply-policy: namespace is PARTIALLY configured — destroy the holder, do not retry
```
…and the *next* rule in that probe plan never ran. The applier aborts on the
first failure, by design.

Teardown is the exact inverse — 17 `-D` in reverse order — so it begins with
rule 17, which a 9-rule partial install never created. That first delete fails,
the applier aborts, and **none of the 9 real rules come out**.

### Why this is a real defect and not a script artefact
`apply-policy`'s exit-3 contract says *destroy the holder*. For the **namespace**
plan that is a complete remedy: the rules live in the holder's netns and die
with it. My **host-side** plan puts rules in the **root netns**, in `DOCKER-USER`
and `INPUT`. Destroying the holder removes none of them. The host path reuses an
applier whose failure contract assumes namespace-scoped rules.

This defeats the intent documented at the adoption site — *"a plan that failed
part-way has already installed rules, and those rules must come out whichever
way this returns."* `HostRules` is adopted correctly; the teardown it runs is
what cannot do the job.

Consequence if shipped: any partial host-rule install strands rules keyed to a
job address in a shared chain, and gate 5h showed those addresses get recycled.
Gate 5h passed only because its install was **complete**, so its teardown
matched rule-for-rule.

**Not fixed in this commit.** The failing gate and its evidence land first.

## Gate 5i, after the fix — PASS

`evidence/gate5i-fail-closed-PASS-after-fix-20260910T0347Z.txt`. The FAIL run
is kept alongside it, not overwritten.

**The fix** (`sandbox_netns.rs`): `HostRules` now carries `complete`, set only
after the applier's count cross-check passes — never at construction. Teardown
branches on it:

* **complete** → the one-shot inverse plan, exactly as before. Every rule is
  present, so the inverse matches rule-for-rule and one invocation is correct
  and cheapest.
* **not complete** → **one applier invocation per rule**. A failure then means
  only "that rule was not there", which on this path is expected rather than an
  error, so a rule that was never created can no longer strand the ones that were.

Adoption still happens **before** the result is examined; that was always right.
What changed is that the teardown it runs can now cope with the state adoption
exists to clean up.

Held by two unit tests: `adopted_host_rules_are_not_complete_until_the_count_check_passes`
(the default must be the rule-by-rule path; the fast path is earned) and
`the_per_rule_teardown_renders_one_valid_delete_per_rule` (each single-rule plan
is one line, a delete, still keyed to this job's `/32` — the applier refuses an
empty plan with exit 4 and a non-iptables binary with exit 5).

**Both strategies now run back to back on the same partial install**, because
the difference between them *is* the fix:

| leg | result |
|---|---|
| C — truncated plan | applier exit `0`, reported `9`, kernel held **9 of 17** |
| C — runsc → live host with 9 of 17 rules | **REACHED** — a partial policy is no policy |
| D1 — one-shot inverse (pre-fix) | **9 of 9 remain**, applier aborts on the first absent rule |
| D2 — per-rule (post-fix) | **removed 9, 0 remain** |
| chain depth | **2 → 2** |

**GATE 5i: PASS, 0 failing checks.**

D1 is kept deliberately as a live regression witness rather than deleted: if
the applier's abort-on-first-failure behaviour ever changes, this gate will say
so instead of quietly preserving a workaround nobody needs any more.

### What this leg does and does not prove
The Rust branch in `HostRules::drop` is covered by the two unit tests above.
What the shell gate proves is narrower and worth stating plainly: that the
per-rule **strategy** clears real rules from real chains where the one-shot
inverse plan cannot. The gate does not drive `establish()` end to end.

## Gate 5j — IPv6: measured, and the code's stated reason does not hold here

Gate 5 left IPv6 as `NO-V6`/UNPROVEN. Maxie asked for "applicable IPv6".
Evidence: `evidence/gate5j-ipv6-finding-20260910T0347Z.txt`.

### What is true by construction
`HostPolicy::argv` uses `Family::V4.binary()` unconditionally, so **no
host-side IPv6 rule is rendered at all**. The netns plan still carries
`DENIED_DESTINATIONS_V6`. This is deliberate and documented at
`sandbox_net.rs`.

### What was measured (aarch64, docker 29.1.3, gvisor-repro)
* `/etc/docker/daemon.json` declares only the `runsc` runtime — **no IPv6**.
* `docker info` reports no ipv6 key.
* Inside a job-shaped holder on a per-job network: `ip -6 addr` shows only
  `::1/128 scope host`. **Zero global IPv6 addresses.**

So on this host a job has no IPv6 egress path, and host-side v6 containment is
not exercised. **IPv6 is not-applicable here** — and that is a statement about
this host, not a safety property of the branch.

### The part that contradicts the code comment
The doc comment justifies omitting v6 rules like this: *"`ip6tables` has a
`DOCKER-USER` chain only when the daemon has IPv6 enabled, and a missing chain
is an install failure that would fail every job launch on a v4-only host."*

Measured on this v4-only host:

```
$ sudo ip6tables -S DOCKER-USER
-N DOCKER-USER
```

**The chain exists**, with IPv6 disabled on the daemon. The stated premise does
not hold here, so it is not the reason v6 rules are safe to omit. The honest
reason is the one gate 5 already gave: host-side v6 containment is
**UNMEASURED**. The comment should be corrected rather than relied on, and I
have not rewritten it in this branch because changing v6 behaviour is outside
the minimum repair maxie scoped — it is named here and in the PR instead.

### The hole, stated plainly
A host **with IPv6 enabled** is **not covered** by this branch. If jobs there
receive global v6 addresses, they have an egress path with no host-side
containment in front of it, and the per-job network does not help for
destinations reached by routing. That is an open hole in an unshipped
configuration — not a proof of safety, and not something this branch fixes.

## Baseline pinning (maxie's ruling, 9 Sep 2026)

Maxie: *"Pin baseline source evidence to a full immutable hash, not moving
`origin/main`."* Every baseline citation in this RUNLOG and in the gate5g
script now names the full commit

    b45f8651dc9cab5b71c962eadd7b84840f579106

and no longer identifies the baseline by a branch ref, which can move under a
reader and would make the reproduction unverifiable later.

The evidence FILES under `evidence/` still print the abbreviated `b45f865`,
and that is deliberate: they are verbatim records of runs that printed it.
Editing a recorded run's output to look tidier would falsify the record. The
abbreviation resolves to the full commit above, which a reviewer can confirm
with `git rev-parse b45f865`.

## Gate 5k — an unsupported daemon default runtime fails CLOSED

Maxie: *"Missing `--runtime` means daemon default, not guaranteed runc; test
unsupported defaults fail closed."* The correction lands on the product.
`holder_argv()` deliberately passes **no `--runtime`**, and a test locks it
there for a sound reason — a runsc holder's namespace is unusable, a job
joining it sees `lo` only (gate2a). So the design **assumes the daemon default
is runc**, and nothing in the product verifies that assumption.

Evidence: `evidence/gate5k-v3-PASS-daemon-default-20260910T0345Z.txt`.

| daemon default | listener | holder runtime | job |
|---|---|---|---|
| `runc` (supported) | SERVING | runc | `HEALTHY 172.18.0.3` → **REACHED** |
| `runsc` (unsupported) | SERVING | runsc | `SICK no-address` → **ENETUNREACH** |

`failing_checks=0`: both legs had a *serving* listener, so leg 2's denial is a
measurement and not a silence. Leg 1 is the positive control — it proves the
harness can observe reachability at all. `/etc/docker/daemon.json` was restored
and the default verified back to `runc` by the EXIT trap.

**Verdict: fails CLOSED.** With an unsupported default the job has no address
and no route, so there is no egress path to contain. That is an availability
failure, not a containment bypass — the product breaks loudly rather than
running jobs uncontained.

**The gap that remains, named not fixed:** nothing detects this at startup. A
seat whose daemon default is not runc will fail every job with no explanatory
signal. The honest fix is a doctor/readiness row asserting
`docker info --format '{{.DefaultRuntime}}' == runc`, which is a product change
outside the minimum containment repair maxie scoped, so it is recorded here for
the follow-up rather than smuggled into this branch.

### Three attempts, two failures kept

v1 (`gate5k-CONFOUNDED-harness-defect-*.txt`) started the listener with
`docker exec` into a runsc holder, which runsc refuses; the target never served
in either leg, including the control. v2
(`gate5k-v2-daemon-default-runtime-*.txt`) found the root cause: the sandbox
image has **no `nc` and no `wget`**, so every probe was doomed before it ran.
Both are committed. They are also the reason the earlier gates were re-checked:
gate5f/5g/5h/5i never use those tools — they probe with `--entrypoint node` and
inline JS — so their SERVING and REACHED readings stand.
