# Seller-tool onboarding — handoff to Petar's local agent

**Written 2026-09-10 by the worker session that authored every commit on this branch, on a human
order to stop implementing and hand off.** Implementation stopped mid-repair. This document is
written for someone with **no access to the authoring machine**: everything referenced is in this
repository, under `docs/handoff/reference/`.

Read this section first, then §6 (what is true vs what is not), then §3 (the four open findings).

---

## 0. TL;DR — state in five lines

- The prototype **works as a mechanism demo** and its **unit/integration tests genuinely pass (32/32)**.
- An external review **DENIED** it with five findings. **F5 and part of F4 are fixed. F1 is a broken
  work-in-progress. F2 and F3 are untouched.**
- **F2 is a real security defect** (not an evidence defect) and is the most important thing left.
- **The end-to-end demo does not run right now.** The last commit knowingly leaves `demo.sh` broken;
  see §6.1. One line fixes it.
- **Nothing is wired into production.** `seller_exec.rs` still passes `mcp_servers: Vec::new()`.

---

## 1. Exact head, base, remote

| | |
|---|---|
| Branch | `feat/seller-tool-onboarding` |
| Remote | `origin` = `https://github.com/maxy-player/maxplayerai.git` (a **fork**) |
| Upstream | `upstream` = `https://github.com/MakePrisms/maxplayerai.git` — **push disabled**, never pushed to |
| Base | `upstream/main` at `3faf895` |
| Commits ahead of base | 22 at the time of writing (23 including this document) |
| Head before this document | `11e6dee` (`WIP(tool-kit): F1 partial … HAS A KNOWN DEFECT`) |

`main` was never touched. No force-push, no rebase, no tag, no merge, no PR.

### Commit chain (oldest → newest)

Stage 0, documentation only:
`0662458` → `f0c0fbf` → `ab33c5b` → `1b3ec20` → `d6a0e5a` → `c46677a` → `b378de5`

Executable prototype:
`088d82e` (kit, 5 binaries) → `fc7abc5` (32/32 tests) → `9a0bd57` (standalone build + Dockerfile) →
`c4d02ba` → `8a3546c` (per-job socket dirs) → `2041c93` → `edc56bd` (demo.sh) → `a5e6880` (demo
evidence 27/27) → `3418a85` → `7d4286b` (supersession banners) → `e565ef9` → `255e957`

Post-review repairs:
`c936609` (**F5 complete**) → `9385031` (**F4 part 1**) → `11e6dee` (**F1 WIP, broken**)

> `c4d02ba` and `2041c93` were authored by a second session during a brief period when two sessions
> were active. They were verified first-hand here and carried forward. Every other commit is this
> session's.

---

## 2. Architecture

The governing constraint, from the ordering seat (`docs/handoff/reference/03-scope-correction-GOVERNING.md`,
sha256 `683da09559bfc12631062c84ecdf4080778c4b31a3e7c16b3a49a7aa89dc64c3`), in Petar's words:

> "the seller is defined by it's offering, there is no offering per job, it is per seller, tool
> should at all times be active together with the seller daemon"

That single sentence killed an earlier per-job token-grant design. **Authentication is per seller and
per daemon lifetime, not per job.** The withdrawn design is preserved, annotated, in
`docs/specs/seller-tool-onboarding/04-token-grant-contract.md` — read its banner before trusting any
part of that file.

### Components (`crates/maxplayer-tool-kit`, 2628 lines, `serde` + std only)

| Binary | Lines | Role |
|---|---|---|
| `vendor-service` | 206 | **Fake third-party vendor.** Owns the auth truth and the independent counters. |
| `vendor-cli` | 234 | The "seller's tool" — what a real seller would actually install. |
| `tool-holderd` | 610 | **The core.** Enrols once at startup, holds the session, serves per-job sockets. |
| `holderctl` | 68 | Operator control: `status│health│tools│attach│detach│reenroll│shutdown`. |
| `tool-mcp-bridge` | 88 | Presents the held tool to a job as MCP over stdio. |

Supporting modules: `validate.rs` (232, argument policy — the F2 site), `http.rs` (149),
`config.rs` (133), `proto.rs` (80), `lib.rs` (75), `client.rs` (43).

### Two design decisions worth keeping

1. **Job identity comes from the listener, never the request body.** Each job gets its own Unix
   socket at `runtime/jobs/<job_id>/job.sock`, in its own `0700` directory. A job cannot claim to be
   another job, because it never states who it is. Reserved arguments (`job_id`, `job_root`, `cwd`,
   `home`) are refused outright.
2. **One directory per socket** (not a flat `<job_id>.sock`), so per-job isolation is expressible as
   a container mount boundary: a job container gets exactly two mounts — its own work dir and its own
   single-socket dir — plus `--network none`.

`attach_job`/`detach_job` are **addressing and isolation only**; they never touch enrolment. Detach
returns `tool_still_enrolled: true`. That is the whole point of the corrected model.

### Independent oracle

Claims about login counts are taken from **`vendor-service`'s own counters** (`login_count`,
`transform_count`, `auth_failures`), never from the holder's self-report. A component asserting its
own correctness is not evidence.

---

## 3. Review findings F1–F5 — full status

Review verdict, verbatim and complete: **`docs/handoff/reference/01-advisor-verdict-executable-r1-DENY.md`**
(19092 bytes, DENY, against commit `7d4286b`). Every finding below was independently re-confirmed
against the source before being acted on.

| # | Finding | Kind | Status |
|---|---|---|---|
| **F1** | Credential-absence check could not fail | Evidence | 🟡 **WIP, broken** — `11e6dee` |
| **F2** | Path re-opened between check and use (TOCTOU) | **Security** | 🔴 **Not started** |
| **F3** | Lifecycle proof probes a detached endpoint | Evidence | 🔴 **Not started** |
| **F4** | Unpinned images, unlocked build, captures gitignored | Reproducibility | 🟡 **Part 1 done** — `9385031` |
| **F5** | Retention notes kept withdrawn grant gates | Documentation | ✅ **Done** — `c936609` |

### F1 — the absence check could not fail *(my own defect, conceded before review pressed on it)*

The old check passed the live secret **into** the probe container as `-e NEEDLE` and grepped from
inside. Worthless three ways over:

- it placed the credential inside the very container whose cleanliness was the claim, so any hit
  would have been self-inflicted;
- `grep -rl … || echo "NOT_FOUND"` printed `NOT_FOUND` on **any** non-match exit, including a scan
  that never ran;
- a trailing `|| true` swallowed docker failures, so an **unstarted container also read as "absent"**.

The same substitution flaw existed host-side in `tests/fixture_suite.rs:196,213-214`, which
substituted empty bytes.

**Intended shape** (per the ordering seat, and what `11e6dee` half-implements): export the job
container's filesystem to the host with **no secret in its environment**, scan on the host where the
secret already legitimately lives, treat capture failure as **fatal rather than as absence**, and add
a **negative control that plants the secret into a copy of the same archive and asserts the same
scanner reports a hit**. The control must use the *same* function as the real check, or it proves
nothing.

**⚠ `11e6dee` is broken — see §6.1. Do not run it expecting a result.**

### F2 — race-safe, no-follow consumption *(the real security defect — do this first)*

The validated path is **re-opened** at use time under the holder's authority, so what was checked and
what was read need not be the same file. Symlink and path-replacement controls sit on the wrong side
of the check/use boundary. Required: consume the path **race-safely and without following symlinks**
(open once, no-follow, operate on the held descriptor), with replacement controls asserted **at that
boundary**. Site: `src/validate.rs` and the holder's file handling in `src/bin/tool_holderd.rs`.

This is the one finding that is a defect in the **product**, not in the evidence. Everything else
weakens proof; this one is exploitable.

### F3 — lifecycle proof probes the wrong endpoint

The stop/restore sequence detaches, then probes an **already-detached** endpoint, so the observed
failure does not distinguish "tool lost" from "endpoint gone". Required: **reattach after restart,
then prove call → loss → restore** on a live attachment. Additionally the tool-list comparison is a
**regex prefix match**; it must be a **full list comparison**.

### F4 — reproducibility *(part 1 done, remainder owed)*

Done in `9385031`, verified:
- both image stages **pinned by digest** instead of mutable tags;
- `cargo build --locked` with a **committed standalone `Cargo.lock`** (11 packages, 107 lines);
- `.gitignore`'s blanket `*.jsonl` had **silently swallowed every raw MCP transcript**; excepted via
  `!evidence/**/*.jsonl`, and the eight dropped captures from both 2026-09-09 runs are now retained.

**Still owed:** the source-to-build receipt, and recording the built image ID inside the demo
manifest. Deliberately not claimed as done.

### F5 — documentation *(done)*

`04-token-grant-contract.md` claimed "Parts II onward stand". False: those parts carried holder
admission rows, holder-enforced grant expiry, per-call token verification, per-job durable budgets,
and a rule that only a *newly authorized* job may run after re-enrolment — the last directly
contradicting same-job recovery. Each contradictory section is now marked **withdrawn in place**, so
a reader landing mid-document cannot mistake a dead gate for a live requirement. Retention is
narrowed to: trust boundary, credential custody, job-directory confinement, session persistence,
re-enrolment.

Two of my own overclaims were corrected in the same commit: file confinement is **not** safe against
replacement (F2), and the container evidence for credential absence and lifecycle is **under repair**
(F1, F3) and must not be cited.

---

## 4. Build and run

Everything runs from the crate directory. **The crate builds standalone** — it declares explicit
dependency versions and no workspace-membership requirement, so the image build cannot quietly pick
up something from `maxplayer-core`.

```bash
# Unit + integration tests — these genuinely pass (32/32).
cargo test -p maxplayer-tool-kit

# Image: pinned by digest, locked build. Verified exit 0 with --no-cache on 2026-09-10.
cd crates/maxplayer-tool-kit
docker build -f docker/Dockerfile -t maxplayer-tool-kit:demo .

# End-to-end demo — ⚠ BROKEN at this commit, see §6.1. Requires a Linux Docker daemon.
IMAGE=maxplayer-tool-kit:demo ./docker/demo.sh
```

`demo.sh` writes evidence to `evidence/<UTC-timestamp>/` and prints a `PASS`/`FAIL` line per check.
It needs a **Linux** daemon (it was developed against colima on macOS/aarch64); `docker buildx
imagetools` is unavailable there, so digests were resolved via `docker pull` + `RepoDigests`.

### Toolchain actually used

| | |
|---|---|
| rustc | 1.98.1 (48a229cea 2026-09-01), from the pinned build image |
| Docker server | 29.5.2, `linux/arm64` |
| Build base | `rust@sha256:ebd900bae66fd508b466cef82d64a83a5fb34682e4c8b2797a42908bddc95a57` |
| Runtime base | `debian@sha256:88200866dfff7ea7f5cbcb6ec7c8a701889efe6fe859fe64d6990e4b07ea4171` |
| Built image (2026-09-10) | `sha256:0fece64074862f51d7b51a9547a8a340f67ff134ede2c350b6fc6409b7b263b7` |

### Interface reference

- `vendor-cli` exit codes: **2** usage, **3** auth, **4** vendor, **5** IO.
- Holder RPC error codes: **1001** unhealthy, **1003** rejected, **1004** tool failed.
- Fixture operation `transform-file` → subcommand `transform`; params `input` (`--in`), `output`
  (`--out`), `mode` (`--mode`: `upper│lower│reverse`); `max_output_bytes: 262144`.

### Credentials

Synthetic only, always. `docker/demo.sh` derives a **fresh random secret per run**, writes it mode
`0600` outside the repository, and removes it on exit. `tests/common/mod.rs` writes a **fixed
synthetic literal that lives in the test source** — created at run time, but a constant. Nothing
credential-shaped is committed. No real credential, account, wallet or sat was ever involved.

---

## 5. Evidence and pins

`evidence/20260909T220053Z/` and `evidence/20260909T220105Z/` — two runs twelve seconds apart, both
**27/27 PASS**, `linux/arm64`, same daemon, separate networks/volumes/container names. Each bundle
carries `manifest.json`, `results.txt`, `holder.log`, the RPC captures, and the raw `*.jsonl` MCP
transcripts recovered in `9385031`.

**These bundles are superseded record, not proof.** They were produced by the pre-repair script, so
every credential-absence and lifecycle claim in them is invalidated by F1 and F3. `mechanism_only:
true` is set in the manifests. Why two nearly-identical runs exist is **unknown**; an earlier commit
guessed a cause and that guess was retracted in `255e957`. `evidence/README.md` states only what the
artifacts support.

Checks the demo asserted (historical, and to be re-earned after F1–F3): `vendor_login_count_after_two_jobs`
= 1, `transform_count` = 2, `auth_failures` = 0 then 1 after a deliberate revoke,
`restart_resumed_existing_session`, `enrollments_this_process: 0`, `cross_job_absolute_path_refused`
code 1003, `tool_unavailable_once_daemon_stops`, re-enrol → `login_count` 2.

---

## 6. What is true, and what is not — read before trusting any number

### 6.1 ⚠ The demo is broken at this commit, deliberately and knowingly

`11e6dee` introduces `$RUN_TMP` into `docker/demo.sh` and **never defines it**. The script runs under
`set -euo pipefail`, so the **first expansion aborts the run**. I found this by grepping for the
definition after writing the code, and stopped there because the order to hand off arrived before I
could fix it.

**The one-line repair:** the script already has a host scratch directory, `HOSTDIR`
(`"$HOME/.mtk-demo/$RUN_ID"`, created `0700`, removed by the `EXIT` trap). Define `RUN_TMP` as a
subdirectory of it, or substitute `$HOSTDIR`. Then the F1 rewrite can be exercised for the first
time.

`bash -n docker/demo.sh` passes — which proves **syntax only** and is exactly why it did not catch
this.

### 6.2 Tests actually executed

| What | Result | Where |
|---|---|---|
| `cargo test -p maxplayer-tool-kit` | **32/32 pass** — 6 `fixture_suite` + 26 `negative_controls` | run at `fc7abc5`, code unchanged since |
| `docker build` pinned + `--locked` + `--no-cache` | **exit 0**, image `0fece640…` | run 2026-09-10 at `9385031` |
| `bash -n docker/demo.sh` | passes (**syntax only**) | at `11e6dee` |

Two real bugs were found and fixed by running these, not by reading: a macOS `SUN_LEN` socket-path
overflow (fixture root moved to `/tmp/mtk/<pid>-<n>`), and a negative control that passed for the
wrong reason (`TextTooLong` fired before `LooksLikeFlag`, so probes were shortened to `--force`,
`-rf`, `--out=x`).

### 6.3 Checks that were INVALID, or were never run

- **`credential_absent_from_job_container` (old form) was invalid.** It could not fail. See F1. Any
  report citing it — including my own earlier "Done" report — overstated what was proven.
- **The 27/27 demo result belongs to the OLD script at `a5e6880`.** It does **not** describe the
  current file. The demo has **not** been executed since `11e6dee`.
- **The F1 rewrite has never produced a result.** Not one of its checks has been observed to pass.
- **`tool_still_enrolled` after restart is not proven** as claimed: the probe hit a detached
  endpoint. See F3.
- **File confinement is not proven race-safe.** See F2.
- **Stale prose still in the tree:** `docker/demo.sh:88` and the manifest credential line still say
  the secret is "never on a command line". That was false under the old injection; the injection is
  gone from source but unverified, so the wording is flagged rather than quietly reworded.

### 6.4 Reporting failures on my side, recorded so they are not repeated

I reported "Done" on the executable prototype while it contained an evidence check that could not
fail. The mechanism was real and the unit tests were real, but the containment claim was not earned.
I also fast-forwarded the fork across a later do-not-push order that was already in flight;
disclosed at the time, no force, no history rewritten.

---

## 7. Production integration gaps

**Nothing in this branch is wired into the product.** The prototype runs entirely beside it.

| Gap | Evidence in-repo |
|---|---|
| Seller execution does not launch the bridge | `seller_exec.rs:2408-2412` passes `mcp_servers: Vec::new()` |
| Bridge is never registered as an MCP server | `McpServer { name, command }` at `driver/acp.rs:49-59` is the shape to populate |
| No seller-level config surface for a held tool | `SellerConfig` at `home.rs:193` |
| Sandbox does not mount a per-job socket | `SandboxConfig` at `home.rs:550` |
| Holder is not supervised with the seller daemon | Petar's constraint requires the tool live **exactly** as long as the daemon |

The next real step is **`tool-mcp-bridge` → `seller_exec.rs`**: construct an `McpServer` pointing at
the bridge, launch it with the job's own socket mounted, and keep `tool-holderd` under the seller
daemon's supervision. **F2 should land before that**, because integration would carry the path
defect into the product.

---

## 8. Still owed — skill, templates, examples

Deliverables named by the ordering seat and **not** produced:

- **A skill** capturing seller-tool onboarding as a repeatable procedure.
- **Docs and templates** for onboarding a new vendor tool (a manifest template plus a filled example;
  `docs/specs/seller-tool-onboarding/02-manifest-schema.md` is the schema to template from).
- **Worked examples**: Walk A (file-processing CLI) reached rung 4; Walk B (tenant-aware HTTP)
  stopped at rung 3 and is deferred — see `05-walk-a-…` and `06-walk-b-…`.
- F4 remainder (§3), and the four open findings.

### Petar's routing decisions — preserved, and deliberately NOT acted on

Recorded here for the follow-on. These arrived **after** the scope correction and were explicitly
held out of the repair work:

- **Direct, unauthenticated CLI/API access is allowed** where the vendor supports it.
- **Reuse an authenticated MCP server via a compatible proxy** rather than re-implementing auth.
- **Token refresh happens outside jobs, before execution**, when the token's remaining lifetime
  covers the job timeout plus a margin; **otherwise a sidecar performs renewal**.
- **Skill, docs and templates are deliverables**, not optional extras.

> **One gap in what reached me:** browser-based authentication was named in the ordering seat's list
> of routing decisions, but **no detailed ruling text for it ever arrived in this session.** I am not
> going to invent one. Treat browser-based auth as **undecided pending Petar's confirmation** — do
> not read silence here as a decision either way.

Also note: `docs/specs/seller-tool-onboarding/08-gaps-and-unsupported.md` is the standing list of
what the manifest model cannot express.

---

## 9. Reference documents (in-repo, no external access needed)

| File | What it is |
|---|---|
| `reference/01-advisor-verdict-executable-r1-DENY.md` | **The review that governs this handoff.** DENY, F1–F5 in full, against `7d4286b`. |
| `reference/02-advisor-verdict-stage0.md` | Earlier review of the stage-0 documentation. |
| `reference/03-scope-correction-GOVERNING.md` | **The binding scope correction.** Per-seller, not per-job. sha256 `683da095…`. |
| `reference/04-implementation-brief.md` | The executable-prototype brief. |
| `reference/05-plan-v3.md` | Plan the work was cut from. |

Each carries a provenance header; content is otherwise unmodified. All five were scanned for
credentials — none found. **A private seat memory file was deliberately excluded** as a private log;
its only load-bearing content (the fix order, and that the mutation hold was lifted) is stated in
this document.

---

## 10. Suggested order of work

1. **Define `RUN_TMP`** (§6.1) — one line, unblocks everything below.
2. **Run the demo.** Expect failures; the F1 rewrite has never executed.
3. **F2** — race-safe no-follow consumption. The only exploitable finding.
4. **F3** — reattach after restart, then prove call → loss → restore; full tool-list compare.
5. **Finish F1** — confirm the negative control genuinely fails when the secret is present. If the
   control passes while the secret is planted, the scanner is blind and the check is worthless again.
6. **F4 remainder** — source-to-build receipt, image ID in the manifest.
7. **Re-run both gates, and replace the superseded evidence bundles** rather than citing them.
8. **Then** integration (§7) and the owed kit (§8).

A closing note on standard, since it cost real time here: **a check that cannot fail is worse than no
check**, because it manufactures confidence. The negative control in F1 exists precisely so the
absence claim can be disbelieved. Keep it that way.
