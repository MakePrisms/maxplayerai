# maxplayer Seller Sandboxing — Architecture Decision

**Status:** Accepted · **Date:** 2026-08-12

## Decision

- **v1 — hardened Docker everywhere; gVisor on Linux only.** One config surface and one image across platforms; the runtime differs where it must. On Linux — the only platform whose default container shares the host kernel — gVisor (`runsc`) is the primary boundary. Macs run hardened Docker inside the platform Linux VM they already have: Docker Desktop cannot load a custom runtime (checked 2026-08 on a live install: only `runc` is offered), and forcing sellers onto Colima costs more than gVisor adds behind an existing hardware boundary. Still a real step up from the shipped bubblewrap on every platform; we accept it's a strong *isolation* layer, not a per-job hardware boundary.
- **v2 — per-OS microVMs, for optimal isolation.** Move each platform onto a hardware (hypervisor) boundary using its best native option: **Kata Containers** on Linux, **Apple `container`** on Apple Silicon. Intel Macs stay on v1.

The split trades breadth for depth: v1 works the same everywhere, immediately; v2 adds a real VM boundary wherever the OS can natively provide one.

## 1. Threat model

The seller runs an AI coding agent on **task text written by strangers** — adversarial input driving an agent that holds a filesystem and the seller's credentials. Risks, most to least common: destructive actions (`rm -rf`, force-push); prompt injection (task text tells the agent to run or exfiltrate); credential theft; and — for the open pool — sandbox escape to the host. The first three are blast-radius problems; only the last needs a hardware boundary.

Two things **no sandbox tier fixes**, so handle them directly:

- **The model credential lives inside the sandbox.** The agent needs it to call the model API, so isolation can't stop adversarial task text from *using* it. Provider-scoped or short-lived tokens don't exist for OAuth logins today, so the concrete v1 mechanism is [#647](https://github.com/MakePrisms/maxplayerai/issues/647): a host-side auth proxy holds the real credential, and a **per-job token** is forwarded into the container — worthless after the job ends, valid only against our proxy. The git credential is a different story: it **never enters the sandbox**. The daemon pushes from the host after the job; `git_env` forwards author/committer identity only (`crates/maxplayer-core/src/seller_git.rs`). Constraint to preserve: **never move `git push` into the sandbox.**

  **Contained set.** All four known model-credential variables are contained by the same value-based mechanism — `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`. Each is removed from the container (replaced by a per-job, vendor-shaped placeholder) and substituted for the real value at egress, routed to its vendor upstream (`api.anthropic.com` / `api.openai.com`, or the operator's `*_BASE_URL`). Verbatim-travel is **proven** for `ANTHROPIC_API_KEY` (the spike confirmed a single verbatim `x-api-key` header); the other three are contained on the identical mechanism but their verbatim-travel is **empirically verified via the red-team / throwaway-login test**, not yet proven. This is **fail-closed**: if a client derives its token rather than sending it verbatim, the substitution misses and the job cannot authenticate — a break, never a leak. What the proxy cannot contain is an operator-added `[sandbox] forward_env` variable the daemon does not recognize (it may be a credential, and the daemon cannot know); the seller node logs a loud boot line and `maxplayer doctor` WARNs when one is set.

  **ADR — the #647 proxy is an in-daemon listener (PR #807), and that couples a running job's egress to the daemon's lifetime.** The per-job proxy is a tokio HTTP listener spawned *inside* the seller daemon and torn down (`Drop`) at job end. This is deliberate: the deterministic teardown is a strength — the placeholder stops working the instant the job ends, with no separate reaper to leak a listener (cf. the container-reaping concern in [#221](https://github.com/MakePrisms/maxplayerai/pull/221) F2). The cost, a **known limitation**: if the daemon dies or restarts, every still-running job container **loses its egress proxy** and can no longer reach the model API — the container is useless until the job is re-driven. **Future direction:** make the proxy resilient to a daemon restart — most likely by running it as its own container (compose-networked to the job) or a separate long-lived host process — weighed against the current `Drop`-based deterministic teardown. Tracked in [#808](https://github.com/MakePrisms/maxplayerai/issues/808).
- **Egress stays open by default.** A dev agent legitimately fetches docs pages, README links, and obscure registries mid-job; a default-deny list fails jobs unpredictably. And registries must stay open regardless — malicious code arrives via `npm install` as easily as via `curl | bash`, so an allowlist doesn't stop hostile ingress either. What bounds exfiltration is the credential design above: once the container holds no durable secret and no writable git endpoint, there is little left to steal. An egress allowlist remains available as *optional* strict hardening (enforced host-side — see §4); connection/DNS logging is the cheap observability middle ground.

## 2. Isolation tiers

A shared-kernel sandbox enforces its boundary with the *same kernel the payload runs on*, so a kernel exploit escapes it. Only a VM adds a second, hardware-enforced wall.

| Tier | Boundary | Escape resistance |
|---|---|---|
| bubblewrap (shipped) | namespaces | weakest |
| **Docker (v1, Mac)** | namespaces + default seccomp + dropped caps | opportunistic exploits bounce; serious ones don't |
| **gVisor (v1, Linux)** | userspace kernel (Sentry) reimplements syscalls; most never reach the host kernel | strong — host-kernel surface hugely reduced; not a VM |
| **microVM (v2)** | real guest kernel behind a hypervisor (KVM / Virtualization.framework) | hardware boundary — escape needs a hypervisor exploit |

## 3. Platform reality (why gVisor is Linux-only in v1, and v2 is per-OS)

A microVM needs hardware virtualization (KVM on Linux; Virtualization.framework on Mac). Availability is uneven — and inverted from intuition:

| Platform | Local per-job hardware VM | Default container posture |
|---|---|---|
| Linux (x86_64) | Yes — native KVM | **weakest**: container shares the host kernel, no VM boundary |
| Apple Silicon | Yes — Virtualization.framework (Apple `container`) | already inside a platform Linux VM (hardware boundary for free) |
| Intel Mac | No per-container option | already inside a platform Linux VM |

The Linux laptop is the only platform that *can* run a local microVM and the only one whose default container has *no* VM boundary — so both gVisor (v1) and Kata (v2) matter most there. Every Mac already runs its containers inside a platform Linux VM, so Macs start with a hardware boundary — v1 leans on it directly (no gVisor on Macs; §4 states the residual risk that choice accepts).

## 4. v1 — hardened Docker; gVisor on Linux

On Linux, ship gVisor (`runsc`) as the Docker runtime — the primary boundary on the platform whose containers otherwise share the host kernel. Install `runsc` from the signed repo (it's part of the security boundary — keep it patched) and confirm **systrap** + **directfs** are active (the defaults that make it fast). On Macs, no gVisor: the boundary is the platform VM plus the hardening below.

**Mac residual risk (accepted).** Job-to-job isolation inside the shared VM is Docker's defaults, so a kernel LPE that works from a confined, non-root container yields root in the VM. Such bugs are rare and expensive — most public LPEs die against Docker's seccomp + dropped caps — but the consequences stack:

- *Integrity:* read/write every concurrent job on the seat — a backdoor planted in another buyer's delivery. Job content being open source kills the secrecy loss, not this one.
- *Availability:* a kernel panic (far cheaper than a full LPE) kills every running job at once. Bounded: the VM restarts, the jobs fail once.
- *The seller's own files — the one that matters.* To be clear: **we never mount `/Users`.** A container gets exactly one mount, the per-job workdir (pinned by the test `docker_policy_mounts_only_the_job_workdir`). The exposure sits one level up, in Docker Desktop itself: its **File sharing** allowlist (Settings → Resources → File sharing) defaults to `/Users` (plus `/Volumes`, `/private`, `/tmp`), and every directory on that list is open to the VM — the VM sees the shared roots under `/host_mnt`, and a VM-root attacker also controls `dockerd`, which may bind-mount anything the list permits. The blast radius of a VM escape is therefore that settings list, not our `-v` flags. **Mitigation, a one-time seller setting:** trim the list to the workdir root (`$MAXPLAYER_HOME/seller-jobs`); the key and wallet live one directory up and drop out of reach.

  *Cost of the trim — state it to sellers.* The list is a **global Docker Desktop setting**, not a maxplayer one: it governs every container on the machine. Removing `/Users` breaks the seller's *own* Docker work that bind-mounts from `/Users` — so it's free on a dedicated seat box, disruptive on a daily-driver laptop. There is no "share `/Users` except `~/.maxplayer`": the list is an allowlist of roots and everything under a shared root is mountable, so the only fix is to drop the broad root and add back the narrow job path. Docker Desktop may also restore defaults on reset/upgrade. Therefore: recommend the trim for dedicated and open-pool machines; **warn, don't auto-edit** (writing another app's settings file and forcing a restart is fragile). `maxplayer doctor` reads `~/Library/Group Containers/group.com.docker/settings-store.json` (key `filesharingDirectories`) and warns when the list still holds `/Users` or any ancestor of the wallet — and must treat an **absent** key as the broad default, not as empty.

**Hardening stack** — a menu with interactions, not a monolith; each item notes what it fights:

- `--cap-drop=ALL`. Cheap and safe: a non-root `--user` already has zero effective caps (verified), so this mainly pins the bounding set.
- `--security-opt no-new-privileges` — already shipped ([PR #221](https://github.com/MakePrisms/maxplayerai/pull/221)).
- `--read-only` rootfs — **rejected for v1, deliberately.** It freezes the container's *own* filesystem (not the mounts — a bind mount carries its own ro/rw), which buys little here: the container is single-job, non-root, and discarded at `--rm`, and a dev agent must have a workspace that is writable *and executable* regardless (it compiles and runs code in `/work` and `/home/agent`). So a read-only rootfs removes no primitive an attacker can't route around; it mainly blocks edits to the image's files, which are rebuilt fresh every job. It also carries a real cost — `/home/agent` holds toolchain installs (GB-scale), and `--read-only` would force that onto a RAM-backed tmpfs. Cost beats benefit in this container shape. *Revisit only if* the provisioning track (§6) moves big installs out of `/home/agent` (e.g. a read-only Nix store), shrinking it enough for a tmpfs and making `--read-only` cheap.
- Scoped bind mount: the job directory only. Never mount host secrets (`~/.ssh`, cloud creds, `~/.maxplayer`).
- userns-remap — **not used in v1; `--user <seller-uid>` alone is the choice.** Remap exists to make container-*root* map to a harmless host uid. But the stack already prevents the job from *being* container-root: `--user` runs it as a non-root uid, `no-new-privileges` + `--cap-drop=ALL` close the setuid → root route, and on Linux gVisor independently gives the "sandbox-root ≠ host-root" property remap would add. So remap neutralizes a state we already prevent. It also breaks the reason `--user` exists — it shifts the uid, so `/work` output would be owned by a translated uid the daemon (running as the real seller uid) cannot read back or clean up. All cost, no marginal benefit here. *Revisit only if* the `/work` ownership/readback model changes.
- Per-job scoped credentials ([#647](https://github.com/MakePrisms/maxplayerai/issues/647)) — the layer that actually contains exfiltration.

If a seller enables the optional egress allowlist (§1), enforce it **host-side** (a restricted docker network / host firewall). Do not grant `NET_ADMIN` inside the sandbox for a firewall-init step — that hands the strongest network capability back to the code the stack just stripped.

**Seccomp:** the two obvious moves are already done, so spend nothing here. Docker's default profile is deny-by-default, and modern Engines also block the `io_uring` family (a 2023 hardening — pin and verify the seller Engine version floor). Don't hand-write a profile: dev toolchains touch a broad, shifting syscall surface and custom profiles break on runtime probes (`clone3`, `statx`, …). Under `runsc` the OCI seccomp profile is **ignored** unless `--oci-seccomp` is set — gVisor filters syscalls itself — so the Docker profile governs only the non-gVisor seats: Macs, and any Linux `runc` fallback.

**Known limits:** gVisor is not a VM — the Sentry is a large Go codebase, and a logic bug in it is conceivable attack surface (but it's memory-safe, unprivileged, and itself sandboxed, so no direct chain to host root). No defense against CPU side-channels. On Macs the boundary is the platform VM plus Docker defaults; the residual job-to-job risk is stated above and accepted.

### Docker-in-sandbox (DinD)

Some repos need a Docker daemon (e.g. `docker compose` for tests). **Never mount the host Docker socket** — that's a host-root escape that defeats the sandbox. Run a real nested daemon instead. (*"DinD seat"* below = a seat whose sandbox config permits this nested daemon; "seat" is a maxplayer job slot, open-pool or targeted.) *Interaction:* a nested `dockerd` runs as **container-root (uid 0)** and needs capabilities and a loose seccomp profile, so a DinD seat drops `--cap-drop=ALL`, the default seccomp filter, `--read-only`, and the non-root user. The danger isn't the uid — it's that this bundle re-opens the wide kernel syscall surface the hardened seat closes, which is exactly where kernel panic/escape bugs live. That is a posture change, not a functional one — DinD runs fine on every platform. The only question is what still contains the now-unhardened nested daemon.

**Decision: DinD is offered on every seat, including the open pool.** The isolation cost differs by platform.

**Linux — no isolation compromise.** Run the DinD container under gVisor. The nested daemon and every container it starts sit atop the Sentry, so even inner-container-root is contained — cross-job isolation holds exactly as for a non-DinD seat. The cost here is *functional*, not security: `dockerd` is syscall/IO-heavy (gVisor's weak spot) and needs the storage-driver fix below (overlay-on-overlay). We accept that friction to keep the boundary. (Sysbox is the fallback if gVisor's DinD friction proves too high — a userns boundary instead of the Sentry, same effect.)

**Mac (stock Docker Desktop) — accepted compromise.** Docker Desktop cannot load gVisor or Sysbox, so a Mac DinD seat drops the hardening with **nothing left to contain** the nested daemon. That opens one specific hole — the cross-job attack. It is the same escape the §4 "Mac residual risk" describes, but through a much wider door, because DinD removed the seccomp/capability blocks that normally keep it shut.

```
macOS            ← the seller's wallet, keys, /Users  (the real prize)
 └─ Linux VM     ← Docker Desktop's VM, hardware-isolated by the hypervisor
     └─ dockerd  ← the VM's daemon
         └─ job container   ← a stranger's DinD job runs here
```

1. DinD forced the hardening off, so the job runs as container-root with a **wide-open kernel syscall surface**.
2. The stranger's job fires a **kernel exploit** through a syscall the hardened seat would have blocked.
3. It becomes **root in the shared Linux VM** — outside its own container.
4. As VM-root it reads/writes a **sibling job's files** (a backdoor planted in another buyer's delivery), or triggers a **kernel panic** that kills every running job at once.

**What the attack does NOT reach:** macOS, the wallet, the keys. The VM→macOS boundary is the hypervisor — a hardware wall the VM-kernel exploit never crosses — and the File-sharing trim (§4) shares only the workdir into the VM, so nothing of the seller's is reachable.

**Why we accept it on Mac.** The loss is bounded to *cross-job* integrity and availability among concurrent jobs on the same machine — not the seller's money, not the host. Job content is open source, so there is no secrecy to lose; the worst case is a poisoned sibling delivery or a shared restart. Blocking the whole feature to prevent a bounded, Mac-only, cross-job risk is the wrong trade. Linux sellers keep full isolation via gVisor, and v2's microVM closes the Mac gap entirely. A Mac seller who wants the isolation now can run Colima (which *can* load gVisor/Sysbox) instead of stock Docker Desktop.

Mechanics for a DinD seat (both platforms):

- **Give the inner daemon a non-overlay `/var/lib/docker`** — a per-job tmpfs (small, fast, RAM-bounded) or a dedicated volume (for large image pulls). This is what sidesteps **overlay-on-overlay**: the inner `overlay2` then sits on a clean filesystem, not on the outer container's own overlay layer. This is a *general* DinD requirement, not a gVisor one — even plain `runc` DinD needs it; the standard `docker:dind` guidance mounts exactly such a volume. gVisor only sharpens it (its Sentry's overlay support is narrower than the host kernel's), which is why gVisor seats *also* need the storage-driver fix below.
- **Cap resources, because an open-pool DinD job pulls and runs arbitrary images.** Set `pids`, `memory`, and a disk quota on `/var/lib/docker`, or one job exhausts the machine and starves its siblings — a cheaper denial than any exploit.
- **Bake into the golden image:** the Docker engine stack; under gVisor, a `daemon.json` storage-driver fix (`{"features":{"containerd-snapshotter":false}}`, since gVisor can't stack overlay-on-overlay); an entrypoint that starts `dockerd` and waits for readiness; a DNS entry (netstack is isolated from host DNS); CA certs + registry config.
- **Set on the launcher/host (not image):** the outer container's capabilities, and a high-enough `user.max_user_namespaces` (a low value surfaces as a confusing "resource temporarily unavailable").

### Performance

gVisor overhead (Linux seats) is workload-shaped: CPU-bound work is near-native, while syscall/IO/network-heavy work is slower — and **`npm install` is close to the worst case** (thousands of small file ops plus fetches). Modern gVisor (systrap + directfs) is far better than its old reputation; mitigate by keeping cost off the hot path — bake dependency installs into the image at build time, put `node_modules` and caches on a tmpfs overlay, keep a warm package cache. Measure `npm ci` under `runc` vs `runsc` on a real project before judging.

### Image distribution — build-your-own today, publish next (future improvement)

**Today the sandbox image is not published anywhere.** `[sandbox] image` is a bare local tag (`maxplayer-sandbox:latest`), and the only way to get it is `docker build docker/maxplayer-sandbox`. Every docker-mode seller builds it themselves — and, for a real agent, extends it with the agent CLI (as with `maxplayer-sandbox-claude`). Four consequences make this untenable at scale:

- **No pull path.** A bare tag makes `docker run` try Docker Hub, 404, and the seat cannot run jobs — so building is mandatory, not optional.
- **npm sellers have no Dockerfile.** The daemon ships via npm (the binary); that package does not carry `docker/maxplayer-sandbox/`. So the common install cannot build the image without separately cloning the repo. *(Confirm what the npm package includes.)*
- **No version pinning.** `:latest` is not reproducible; a buyer cannot know which image ran their job — weak for a marketplace built on verifiable delivery.
- **The boot gate does not check the image.** `maxplayer doctor` verifies docker is installed but NOT that the configured image exists, so a missing image fails at the pre-advertise probe with a raw docker pull error, not a build/pull hint.

Target end-state:

- **Publish versioned images to a registry** (e.g. `ghcr.io/makeprisms/maxplayer-sandbox:<version>`) via a CI build-and-push job. Sellers **pull**, not build.
- **Layer:** `maxplayer-sandbox` (base: node + git + CA certs + ACP adapters) and `maxplayer-sandbox-<agent>` (base + the agent CLI). The CLI is now **bakeable with no secret** — the auth token became a runtime `-e` value once credential forwarding landed, so the Dockerfile's "provisioning the CLI is a seller-operator step" note can be revisited.
- **Digest-pin** the image (`…@sha256:…`) so a buyer can tell exactly what ran.
- **`maxplayer doctor` should verify the configured image is present or pullable**, and on absence print the exact `docker pull` (or build) command — so a seller gets a clear signal, not a probe-time docker error.

## 5. v2 — per-OS microVMs

Each platform moves onto a per-job hardware boundary via its best native microVM. Execution stays on the seller's machine throughout, and the v1 hardening, credential scoping, and the provisioning-track store model all carry over.

**Linux → Kata Containers.** Per-job microVM over KVM, driven by **Cloud Hypervisor** (lean Rust VMM; recommended) or **QEMU** (heavier, most compatible). Keeps OCI ergonomics (`docker run --runtime=kata`) and, critically, **virtiofs** — so the job directory and the shared Nix store bind-mount exactly as in v1. A real guest kernel means DinD "just works," with none of the v1 overlayfs/DNS/sysctl workarounds. Boot is sub-second to low-seconds depending on VMM.

*Why not Firecracker:* the obvious microVM choice has no shared-filesystem support — only block devices — so it can't bind-mount the job directory or the Nix store the harness relies on. Getting data in would mean packing a per-job disk image and reading results back out, a heavier and different I/O model. Its maintainers rejected shared-FS support on attack-surface grounds, so Kata is the microVM that keeps the hardware boundary without the constraint.

**Apple Silicon → Apple `container`.** Apple's native tool (stable 1.0, June 2026) runs each OCI container in its own lightweight VM via Virtualization.framework — a per-container Kata kernel that boots in under a second, moving isolation from the shared kernel to the hypervisor. This replaces the v1 stack outright on Apple Silicon: the per-container VM supplies host isolation, job-to-job isolation, and ephemerality in one layer, with a Docker-compatible CLI and unchanged OCI images. Requires macOS 26 for full networking; Apple Silicon only.

**Intel Mac → stays on v1.** No per-container microVM exists for Intel Macs (Apple `container` is Apple-Silicon-only). They remain on v1 — hardened Docker inside the platform Linux VM, already a hardware boundary against macOS. Acceptable given the shrinking Intel install base.

## 6. Toolchain provisioning — its own track (deferred out of v1)

Split out deliberately: this is the largest engineering item in the plan and it changes the **buyer protocol** (a flake has to travel with the task), so it gates on its own schedule and must not hold v1 — which is otherwise config-level work on an existing seam.

**Interim — today's shipped behavior:** prebaked images plus per-job installs into the container-only `/home/agent`. Slow for exotic toolchains, but correct: nothing lands in the delivery, and nothing is shared between jobs.

**The plan when the track opens:** accept a **buyer-provided Nix flake** and materialize it at job start (`nix develop`); the pinned `flake.lock` keeps it reproducible. A **persistent shared `/nix/store`, mounted read-only into jobs**, so common dependencies are fetched or built once — and a writable shared store can't become a job-to-job poisoning channel. New builds run **inside the job's own sandbox against a per-job ephemeral overlay**, ideally pulling from an internal binary cache. GC the store from a trusted process using live-job GC roots. The model carries to v2 unchanged — the store is a read-only bind mount under Docker, Kata/virtiofs, and Apple `container` alike.

**Rejected: building a stranger's flake on a trusted host `nix-daemon`.** Flake evaluation and builds run buyer-controlled code — fixed-output derivations get network access, import-from-derivation runs at evaluation time — which moves adversarial input to exactly the wrong side of the boundary the sandbox exists to enforce. All materialization happens inside the sandbox.

## 7. Where this lands in the code

- **Sandbox seam:** `crates/maxplayer-core/src/seller_exec.rs` — `SandboxPolicy` / `DockerPolicy`. `run_argv` now emits the v1 hardening (`--cap-drop=ALL`, `--security-opt no-new-privileges`, `--init`, single workdir mount, non-root `--user`) and an optional `--runtime <name>` from config ([PR #221](https://github.com/MakePrisms/maxplayerai/pull/221)). `--read-only` and userns-remap are the rejected-for-v1 items above, not omissions.
- **Config surface:** `[sandbox]` in `crates/maxplayer-core/src/home.rs` (`mode`, `image`, `forward_env`, `runtime`); operator docs in `docs/DOCKER.md`.
- **Containment probe:** `crates/maxplayer/src/sandbox_probe.rs` — a **self-check** protecting honest sellers from config mistakes (canary read + workdir write), *not* attestation. Extend the container payload to detect gVisor (`/proc/version` reports a gVisor string) so a Linux seat can verify its own runtime tier before joining the pool.
- **Git delivery:** `crates/maxplayer-core/src/seller_git.rs` — `git_env` forwards author/committer identity only; the push is host-side. §1 leans on this property.
- **Credential lockdown:** [#647](https://github.com/MakePrisms/maxplayerai/issues/647). **Container-path CI coverage gap:** [#643](https://github.com/MakePrisms/maxplayerai/issues/643).

## 8. Open questions

- **Kata VMM:** Cloud Hypervisor (lean) vs QEMU (max compatibility) as the Linux default — decide against a real job mix.
- **Nested virtualization availability:** Kata needs KVM, and many cloud VPSes don't expose it — those sellers stay on v1 permanently. Does the open pool keep accepting a v1 Linux seat once v2 exists?
- **Open-pool floor, per platform:** Linux — require gVisor? Mac — platform VM + hardened Docker? Either way the pool only ever sees a **self-reported** tier: the probe is a self-check and a malicious seller can lie, so reputation and economics carry the rest.
- **DinD floor:** decided (§4) — DinD is offered on every seat. Linux keeps full isolation via gVisor; Mac (stock Docker Desktop) accepts a bounded cross-job risk. Residual: surface the Mac compromise to buyers as a seat capability flag, and nudge Mac sellers toward Colima for the gVisor path?
- **Apple `container` rollout:** depends on sellers being on macOS 26 — track adoption before defaulting Apple Silicon to v2.
- **Provisioning track (§6):** store poisoning, GC policy, substituter restrictions — all live there, not in v1.
- **Image distribution (§4):** publish versioned sandbox images (base + per-agent) to a registry so sellers pull instead of build, digest-pin for reproducibility, and make `doctor` check image presence — see §4 for the full write-up.
