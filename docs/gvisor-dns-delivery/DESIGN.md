# Fix design — gVisor named-network DNS, and a delivery preflight that can fail

Status: **drafted, not yet executed as code.** The measurements it rests on are
real (`RUNLOG.md`, `evidence/gate1-runsc-vs-runc-20260910T0046Z.txt`); the code
below is not written until a disposable Linux host is ruled, because an
unexecuted fix is not a fix.

## What has to change, and why each piece exists

### 1. The job sandbox gets a resolver it can actually reach

Measured: under runsc, `127.0.0.11:53` answers nothing at all, and `--dns` does
not move it — docker writes `nameserver 127.0.0.11` on any user-defined network
regardless. So the only lever that works from outside the daemon is the file
itself: mount the job's `/etc/resolv.conf` read-only, naming real upstream
resolvers.

Lands in `seller_exec.rs::run_argv` (the same argv that already carries
`--runtime`, `--cap-drop ALL`, `--user`, `--security-opt no-new-privileges`) as
one more `-v <generated>:/etc/resolv.conf:ro`. The generated file is per-seat,
not per-job — it holds no job data — and is written under the seller home with
mode 0444.

Resolver selection, in order, with **no silent fallback**:

1. explicit config (`[sandbox] dns_servers`), if set — an operator on a VPS with
   a mandated resolver needs this and it is the only way to express it;
2. otherwise the host's real upstream resolvers, discovered by reading
   `/etc/resolv.conf` and, when that names only a local stub (`127.0.0.53`,
   systemd-resolved — exactly what Bob's VPS shows), `resolvectl status` for the
   actual upstreams;
3. otherwise **fail loudly at boot**. Guessing `8.8.8.8` here would be a silent
   host-side fallback and is precisely what the brief forbids.

A stub address (`127.0.0.0/8`) is never written into the sandbox file: it is
unreachable from inside the sandbox by construction, and writing it would
reproduce the bug with a different address.

### 2. The egress policy opens port 53 to exactly those resolvers

`NetPolicy` currently carries `gateway`, `proxy_ports`, `log_connections`, and
denies the private ranges wholesale (`DENIED_DESTINATIONS`) while never denying
loopback. DNS to an upstream resolver is new traffic that the deny ranges may
shadow, so the policy grows one field: the resolver addresses.

Rules added in `NetPolicy::rules()`, before the range denies for the same reason
the proxy pinhole is:

- `-p udp -d <resolver>/32 --dport 53 -j ACCEPT` and the tcp counterpart, one
  pair per resolver, **/32 (or /128) only** — never a subnet, never "port 53
  anywhere in the private range". If an operator's resolver is itself a private
  address, this opens that single host and nothing else, and the check in §3
  proves what it opened.
- `verify_readback` learns the same rules, so a namespace missing them is
  reported rather than assumed.

That keeps gate 5 intact: every other private, loopback, link-local and metadata
destination stays denied, and a DNS name resolving to a denied address still
dies at the deny rules because resolution and reachability are separate rules.

### 3. Doctor/readiness runs the real route, and a failure blocks ready

Today `check_sandbox_egress` answers "can a namespace be built" (docker network
exists) and is deliberately `Warn`-only (petar, 2026-08-18: automate first, then
require). That check is not touched — it answers a different question, and its
advisory status was a ruling, not an oversight.

The new check is a different thing: **it launches the actual contained sandbox
and makes it resolve and complete a certificate-validated TLS handshake to the
delivery host**, under the production runtime, user, cap-drop and namespace.
Host connectivity is never consulted, so a host that can reach the internet
while the sandbox cannot produces a FAIL, which is the whole point.

- Status `Fail`, so `readiness_ok` (any `Fail` ⇒ not ready) refuses the seat.
- `transient: true`, so a genuine network blip is retried on the existing
  bounded schedule (5 attempts, 20s/40s/60s/80s) and a still-broken route is
  then refused. Transient-retry is not transient-forgiveness.
- The failure message names the resolver it used, the runtime, and the exact
  docker command to reproduce, because "DNS failed" sends nobody anywhere.

### 4. Delivery is proven from inside the sandbox, not beside it

Gate 4 requires a real container-side git push whose remote hash matches. The
preflight in §3 proves DNS+TLS; the delivery gate proves the actual `git push`
path from the same contained sandbox to a disposable remote, with the remote's
hash read back afterwards. A host-side upload passing while the sandbox path is
broken is the exact false-ready this work exists to kill.

## What is deliberately NOT done

- No `--network=host`, no runsc network passthrough, no runc fallback: the
  measured fix needs none of them.
- No broad private-range allowance; resolver pinholes are single addresses.
- No relaxation of `check_sandbox_egress`'s existing ruling in either direction.
