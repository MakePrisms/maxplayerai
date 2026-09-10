---
name: seller-tool-onboarding
description: Onboard a third-party vendor tool into the seller-level tool holder (maxplayer-tool-kit). Use this when you add a new vendor CLI to a seller's offering, write or review a seller tool config, map vendor subcommands to holder operations, or run the holder tests and the Linux container demo. Covers the per-seller enrolment model, the file-access safety rules, and the evidence a run must produce.
---

# Seller tool onboarding

Use this skill to onboard a vendor tool into the holder in `crates/maxplayer-tool-kit`.

## The model

Read this first. It decides the whole shape of the work.

1. The seller is defined by the offering. There is no offering per job.
2. The holder enrols the tool one time, when the seller daemon starts.
3. The tool stays logged in for the life of the daemon.
4. A job start, a job end, a payment, or an award does not enrol, log out, or renew the tool.
5. A job gets an endpoint and a directory. A job never gets a grant.

The governing source is
`docs/handoff/reference/03-scope-correction-GOVERNING.md`. Do not add a per-job grant, an award
gate, or a marketplace state change. Those requirements are withdrawn.

## Components

| Binary | Role |
| --- | --- |
| `vendor-service` | A fake third-party vendor. It owns the auth truth and the counters. |
| `vendor-cli` | The seller's tool. A real seller installs a tool like this. |
| `tool-holderd` | The holder. It enrols once, holds the session, and serves per-job sockets. |
| `holderctl` | The operator CLI: `status`, `health`, `tools`, `attach`, `detach`, `reenroll`, `shutdown`. |
| `tool-mcp-bridge` | The MCP server a job runs. It forwards to the holder's per-job socket. |

## Steps to onboard a tool

Follow these steps.

1. Copy `crates/maxplayer-tool-kit/templates/seller-tool-config.template.json` to a new file.
2. Fill the fields. See `crates/maxplayer-tool-kit/templates/README.md` for each field.
3. Add one operation per vendor CLI subcommand you offer.
4. Map each subcommand argument to one parameter. Choose the parameter `kind`:
   - Use `job_input_file` for an input path.
   - Use `job_output_file` for an output path.
   - Use `choice` for a closed option set.
   - Use `text` for bounded free text.
5. Set `max_output_bytes` for each operation.
6. Confirm the vendor CLI reads its credential from its own home directory. It must not take a
   credential on the command line.
7. Run the tests and the demo. See "How to test" below.
8. Ask a human with authority over the seller account to review the mapping.

## Safety invariants

The holder enforces these invariants. Keep them true in any change.

1. The holder builds argv in the spec order. It runs no shell.
2. The holder resolves a job file path with no-follow opens, in `src/safeio.rs`.
3. The holder copies an input into a private staging directory. The vendor CLI reads the staged
   copy.
4. The holder publishes an output with a no-follow create. It refuses a symlink at the output
   name.
5. The credential never enters a job container.
6. The vendor's own counters are the oracle. The holder's self-report is not evidence.

The file-access rules close a check/use race (advisor F2). If you change `src/validate.rs` or
`src/bin/tool_holderd.rs`, keep the resolve step at the point of use, on a held descriptor.

## How to test

Run both gates.

```bash
# Unit and integration tests.
cargo test -p maxplayer-tool-kit

# End-to-end Linux container demo. It needs a Linux Docker daemon.
cd crates/maxplayer-tool-kit
docker build -f docker/Dockerfile -t maxplayer-tool-kit:demo .
IMAGE=maxplayer-tool-kit:demo ./docker/demo.sh
```

The demo writes evidence to `evidence/<UTC-timestamp>/`. Each bundle holds a `manifest.json`, a
`results.txt`, both MCP transcripts, the holder and vendor logs, and the vendor counter
snapshots. The `manifest.json` also holds a source-to-build receipt and the built image id.

## Evidence rules

State the truth about a run. Keep these rules.

1. A run against `vendor-cli` and `vendor-service` proves the mechanism only. Both were written
   for this contract, so they cannot falsify it.
2. A run does not prove third-party acceptance. It does not prove general onboarding acceptance.
3. A check that cannot fail is worse than no check. Keep a negative control for each claim.

## Out of scope

These items are not part of onboarding a tool with this kit.

- Production wiring into `seller_exec.rs`. The prototype runs beside the product, not inside it.
- A per-job grant, an award gate, or a marketplace state change. These are withdrawn.
- A TLS transport. This kit ships `http://` only.
- Browser-based authentication. No ruling for it has arrived. Treat it as undecided.
