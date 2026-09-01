# Container-delivery wiring — implementation spec (the go-live work)

> **Status: BLOCKED, not started.** This is the durable spec for the wiring that moves git delivery
> into the sandbox container. It exists so the plan is not lost while the blockers clear. Do NOT build
> it until the gates below are met — most of it is not validatable without a live relay + a real
> container.
>
> **Gates (all required):**
> 1. Relay: #929 (scope enforcement) merged + deployed, plus the longer-life-for-scoped-tokens change
>    (Requirement B in `2026-08-31-relay-scoped-token-lifetime.md`), **canary-verified**.
> 2. A running sandbox container to validate the host wiring + the driver relocation.
> 3. Security review confirming C3/C4/C6 + metering-at-proxy in the wiring code.

## Where this fits

| PR | State | What it did |
|----|-------|-------------|
| #937 | merged | interim host fix: `seller_git::neutralize_push_config` + neutralise-then-push on the host. Closes the exploit today. |
| #930 | merged | Track A: the `["ref", …]` scope tag on the delivery token. |
| #939 | merged | the orchestrator building blocks (`delivery_orchestrator`, `__deliver` CLI) — INERT; nothing invokes them. |
| #949 | draft/blocked | Task B8: the `expiration_unix` mint seam for the long-lived token. INERT. |
| **this spec** | — | how the inert pieces get wired so git actually runs in the container. |

**Today git is 100% host-side.** Only the agent runs in the container. Clone (`init_*_workdir`),
commit (`snapshot_delivery_at`), and push (`neutralize_then_push_off_runtime`) all run on the host in
`SellerNodeRunner::execute_job` (`run.rs`). The wiring below moves them into the container.

---

## The switch: `[sandbox] container_delivery`

Add a `bool` config field (default `false`) under `[sandbox]` (parsed in `seller_exec` /
`SandboxPolicy`). `false` = today's host path (unchanged, keeps #937). `true` = the container path
below. Ship the whole wiring behind this switch so production stays on the reviewed host path until
the switch is flipped after the canary passes. Removing the interim host path (final step) happens
only once the switch is the default and proven.

---

## Task B9 — relocate the ACP driver into the orchestrator

Today the host runs the agent: `execute_job` calls `seller_exec::run_agent_with_retry`, whose `run`
closure launches the agent **in docker** and drives the ACP session from the host. Move that into the
container orchestrator so ONE container does agent + git.

- The orchestrator's `spawn_agent` seam (`delivery_orchestrator::run_phase1`, currently
  `spawn_agent_child`) becomes a call to `run_agent_with_retry` with a **pass-through** sandbox policy
  — the agent runs directly in the container; NO nested docker (the container is already the sandbox).
- `run_agent_with_retry` is async; the `__deliver` subcommand builds a small Tokio runtime to drive it.
- `Phase1Inputs` gains what the host used to assemble for the run: the composed prompt
  (`compose_agent_prompt` output, or the task + the inputs to recompute it), the deadline, the agent
  argv / harness preset, and the **credential-proxy endpoint** (`host.docker.internal:<port>` +
  the placeholder env). No secret — the proxy holds the model key on the host.
- **The host no longer drives ACP.** The credential proxy STAYS on the host (`credential_proxy.rs`:
  "The proxy is a host process"); the agent reaches it over the network exactly as today. Nothing
  about credential custody changes.
- Heartbeats: the host based liveness on the ACP stream before; now base it on "container alive" (or
  a lightweight orchestrator ping). Small follow-up, not a blocker.
- ⚠ Only a real container + a real ACP harness exercises this. Unit-test the input contract + the
  pass-through wiring; validate the rest live.

## Task B2 — host wiring in `execute_job`

Behind the switch, replace the host clone / snapshot / push with a container hand-off. Concretely,
add a host function (e.g. `deliver_via_container(...)`) that `execute_job` calls when
`container_delivery` is on:

1. Compute the delivery branch, base (`base_oid`/clone url), `job_hash`, author date — as today.
2. **Mint the long-lived scoped token FIRST** (before launch), via the signer:
   `signer.http_auth_header(remote, Some(delivery_ref(branch)), Some(deadline + PUSH_MARGIN))` (the B8
   seam). The token must be valid at push time, which is the end of the run — hence the long life
   (relay Requirement B). Refuse at mint if `deadline + margin` exceeds the relay's cap (loud
   seller-side error, not a push-time 403).
3. Write `Phase1Inputs` (incl. the token, the relay url, and the B9 agent inputs) to a file the agent
   cannot read — a host path outside the bind-mounted workdir, mode `0600`, owned by the orchestrator
   uid (C3/C4). Fail closed if the perms cannot be set.
4. **Launch one container** whose entrypoint is `maxplayer __deliver phase1 <inputs>` — reuse
   `DockerPolicy::run_argv` (same mounts / uid / `--network` / `--cap-drop` / `--init` as today's agent
   run) but with the orchestrator as the command instead of the agent. Mount the workdir + the inputs
   dir + an out dir.
5. Read the OID the orchestrator wrote (`read_delivery_oid`) — a plain string; the host runs NO git.
6. Publish the kind-3403 naming that OID; sign the delivery co-signature; payment — all unchanged.

The clone (`init_*_workdir`), the snapshot/gate/sentinel (`snapshot_delivery_at`), and the push
(`push_delivery`) now all run INSIDE the orchestrator (`run_phase1` + `push_delivery`, from #939).

**Token freshness / one container.** The host mints the long-lived token up front and injects it in
the inputs; the orchestrator pushes with it at the end of the (minutes-long) run. Safe because the
token is branch-scoped (a leak is worthless) and the relay allows the longer life for scoped tokens.
*Fallback if the relay freshness change is rejected:* keep tokens 60 s and have the host drop a fresh
token file into the shared workdir after the agent exits, which the orchestrator then reads and pushes
(`2026-08-28-…` plan, "Alternative"). One container either way.

## Task B7 — sandbox image carries the binary

The orchestrator entrypoint needs the `maxplayer` binary in the image.

- Add a `FROM rust:1-bookworm AS builder` stage to `docker/maxplayer-sandbox/Dockerfile` (mirror the
  top-level `Dockerfile`: `cargo build --release -p maxplayer --features acp,wallet` → COPY
  `/usr/local/bin/maxplayer`).
- Switch the sandbox image build **context to the repo root** (today it is `docker/maxplayer-sandbox`,
  which cannot see the crate source), and update `.github/workflows/publish-sandbox-image.yml`.
- Additive; no `ENTRYPOINT` change — the host passes the full command at `docker run`.

## Task B10 — meter usage at the credential proxy

Under B9 the host no longer sees the ACP stream, so the per-job roster model refresh loses its source
and any container-reported usage is spoofable by the job.

- Move usage / budget accounting to the host credential proxy, which forwards every model call and is
  the authoritative, tamper-proof meter. Record the model from the API traffic too and feed the roster
  from that. (Ties to #863.)
- The container may still emit a usage summary for observability; money decisions use the proxy count.

## Final step — remove the interim host push

Once the container path is the default AND the relay is confirmed enforcing the scope: delete the
host-side `neutralize_then_push_off_runtime` call in `execute_job` and (if unused elsewhere) the
`neutralize_then_push_off_runtime` wrapper. `seller_git::neutralize_push_config` stays — the container
push reuses it via `delivery_orchestrator::push_delivery`.

## Security checks that land WITH this wiring (reviewer C3/C4/C6)

- **C3** — the inputs-file delete in `run_phase1_entry` must be fail-closed (abort before spawning the
  agent if the delete fails), and the host must create the file `0600` outside the agent-writable
  mount.
- **C4** — add a defensive env allowlist at `spawn_agent_child` (the token/`job_hash` must never be in
  the agent's env even by accident).
- **C6** — carry the gated `expected_oid` into the push and fail closed on mismatch
  (`OrchestratorError` already has the shape) — a background process surviving the agent could
  otherwise re-point the branch between gate and push.

## Validation plan

- The offline unit tests cover the primitives (`neutralize`, gate, retry, inputs, OID).
- Everything above needs a **real container run** end to end: one from-scratch job and one
  contribution job, delivered + accepted + paid, on a seat with `container_delivery = true`.
- The **relay canary** (mint a scoped token, push a different ref, expect 403) proves the scope is
  enforced before long-lived tokens are trusted.

## Code anchors

- Host delivery path: `crates/maxplayer-core/src/seller_node/run.rs` — `execute_job` (provision →
  `run_agent_with_retry` → `snapshot_delivery_at` → `neutralize_then_push_off_runtime`).
- Orchestrator (inert, #939): `crates/maxplayer-core/src/delivery_orchestrator.rs`
  (`run_phase1`, `push_delivery`, `run_phase1_entry`, `spawn_agent_child`, `Phase1Inputs`).
- CLI entrypoint: `crates/maxplayer/src/deliver_cli.rs` (`maxplayer __deliver`).
- Token mint seam (B8, #949): `git_transport::nip98_authorization_header_with_keys(…, expiration_unix)`
  threaded through `seller_node::signer` `http_auth_header`.
- Docker launch: `crates/maxplayer-core/src/seller_exec.rs` — `DockerPolicy::run_argv`,
  `CONTAINER_WORKDIR`.
- Credential proxy (host-side): `crates/maxplayer-core/src/credential_proxy.rs`, `PROXY_HOST_ALIAS`.
- Agent driver: `seller_exec::run_agent_with_retry` / `run_agent_job`, `compose_agent_prompt`.
- Sandbox image: `docker/maxplayer-sandbox/Dockerfile`.
