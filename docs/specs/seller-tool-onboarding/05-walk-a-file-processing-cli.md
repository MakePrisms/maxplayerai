# 05 — Paper walk A: authenticated file-processing CLI

Plan v3 §6 stage 0 requires one file-processing CLI walked through the mapping, custody, grant
and checker contracts. This is that walk.

## Status of the subject

The subject is an **archetype**, not a named vendor product, and not the stage-1 tool (Petar
selects that under plan v3 §7). Its shape is the common one for authenticated file-processing
CLIs: a persistent browser/device login, a server-side job model, an input file, an output
file, and a small set of conversion options.

Every capability attributed to it below is an **assumption**, listed here so the walk can be
falsified rather than believed:

| # | Assumption | If false |
| --- | --- | --- |
| A1 | login persists in a config dir under `$HOME` after an interactive enrollment | rung 4 unavailable; re-check §2 routing |
| A2 | the CLI accepts an explicit input path and an explicit output path | staging contract (03) does not apply as written |
| A3 | conversion options are a closed set of flags with enumerable values | no bounded-enum mapping; likely unsupported |
| A4 | the CLI can run non-interactively with no TTY | holder cannot drive it |
| A5 | credential refresh writes only inside its own auth file | see "custody" below — likely unsupported if false |

**A5 is the assumption that most often fails in practice** and it is the one that decides
supported vs unsupported. It is checked before anything else is built.

## Step 1 — Routing (plan v3 §2)

| Rung | Verdict for this tool |
| --- | --- |
| 1 public tool | No — it requires a login. |
| 2 direct vendor token | No. The persistent artifact is a refresh-capable session, not a job-scoped issuable token. Plan v3 §2 excludes routes exposing persistent refresh secrets, and requires enforceable vendor revocation or job-close binding, which this archetype does not offer. |
| 3 key-swap proxy | No. The CLI does not send auth in a single replaceable field it would let us intercept, and its request semantics are not constrainable by path/method. |
| **4 MCP in a persistent isolated container** | **Selected.** The real CLI must hold the login state, and it runs in the holder, never in the buyer-controlled job container. |
| 5 host executor | Not needed; no machine-bound or hardware licence constraint under A1/A4. |

Route is rung 4. Under plan v3 §2 this is the one rung whose template stage 2 ships.

## Step 2 — Manifest (02)

```yaml
schema_version: 1
offering:
  service_id: "doc-convert"
  display_name: "Document conversion"
  party_scope: "per-party"
holder:
  holder_id: "H-docconv-1"
  enrollment: "vendor-browser"
operations:
  - verb: "convert"
    profile: { name: "docconv-cli", version: 1, digest: "sha256:…" }
    bindings:
      target_format: "pdf"          # bounded enum
      quality: 90                   # bounded integer 1..=100
      doc_title: "quarterly-report" # bounded literal text, fixture grammar
    effects:
      max_calls: 1
      max_items: 1              # one input document -> one output document
      max_input_bytes: 8192     # staging ingest bound, enforced before invocation
      max_output_bytes: 262144  # slot quota, enforced by the supervisor at the slot
grant_policy:
  allowed_openers: ["opener:marketplace-core"]
  allowed_parties: ["party:P"]
  resources: { allow: ["res:R"] }
  max_job_lifetime: "PT15M"
```

Note what the manifest cannot say: which binary, which account, where the credential lives,
what the network may reach. All of that is profile-pinned.

## Step 3 — Source-to-sink mapping (03)

Authorized invocation, and nothing else:

```
/opt/docconv/bin/docconv convert \
  --config /holder/private/docconv.toml \   # constant
  --account seller-primary \                # constant
  --no-plugins \                            # constant
  --non-interactive \                       # constant (A4)
  --to pdf \                                # enum
  --quality 90 \                            # integer
  --title quarterly-report \                # literal text
  --in  /holder/private/job-J/in/1 \        # uploaded-artifact handle
  --out /holder/private/job-J/out/1         # destination slot
```

| Field | Source | Validation / encoding | Sink | Vendor interpretation | Allowed effect |
| --- | --- | --- | --- | --- | --- |
| executable | constant | image digest pinned | argv[0] | binary identity | — |
| `convert` | constant | fixed | argv[1] | subcommand | — |
| `--config` | constant | holder-private, seller-authored | argv[2..3] | config file | selects account + endpoint only |
| `--account` | constant | seller policy | argv[4..5] | account selector | binds billing identity |
| `--no-plugins` | constant | fixed | argv[6] | disables extension load | closes plugin sink |
| `--non-interactive` | constant | fixed | argv[7] | no TTY prompts | prevents prompt-driven divergence |
| `target_format` | enum `{pdf,docx,txt}` | membership | argv[8..9] | output codec | none beyond codec |
| `quality` | integer `1..=100` | range | argv[10..11] | encoder quality | bounds output size |
| `doc_title` | literal text | fixture grammar (02) | argv[12..13] | **data**: written into document metadata | none |
| input | uploaded handle | staged, digest-checked at consumption | argv[14..15] | reads one file | one read |
| output | destination slot | holder-created, private, checked before export | argv[16..17] | writes one file | one in-slot write |

Three mapping decisions worth defending:

1. **`doc_title` is data here because this tool writes it into metadata.** If a future version
   let `--title` name a template file, the same field becomes a text-to-URL-class mapping and
   **rejects**. The mapping is per-tool and per-version; it is not inherited across a version
   bump, which is why the profile digest pins version.
2. **`--config` is a constant, not a field.** A job-selectable config file is a
   configuration-selector sink, which the constant policy forbids outright.
3. **There is no vendor-side resource identifier in this walk, so the handle *is* the granted
   resource.** The input arrives as an uploaded artifact rather than a vendor record, so no
   `res:` string appears in argv, and none is invented. But an unused `res:R` in the manifest
   would bound nothing at all — so the grant check must attach to the object that actually
   carries authority here: the handle.

### Handle and slot ownership

Every uploaded-artifact handle and destination slot is **holder-issued and immutably bound at
creation** to a triple:

```
handle H1 -> { holder: H-docconv-1, party: P, job: J }
```

The binding is recorded in the holder's durable admission record
([04](04-token-grant-contract.md) Part I), not in the handle string, and the handle is opaque —
unguessable and carrying no path.

**Admission membership check, run on every call before any child starts:** for each handle and
slot named in the request, the recorded triple must equal the presenting token's holder, party
and job. Not merely "a valid handle" and separately "a valid token" — the *same* triple.

This closes a hole that generic forged-handle and filesystem-isolation cases do **not** close.
Forged handles test unguessability; mount isolation tests the filesystem. Neither prevents a
**valid handle owned by job `K`, presented through job `J`'s otherwise entirely valid token**.
Both objects are genuine; only the relation between them is wrong. That case is enumerated as a
required negative in [07](07-test-entrypoints-and-evidence.md) check 6.

## Step 4 — Custody (04)

- **Enrollment**: seller opens the vendor browser flow *inside* holder `H-docconv-1`. The CLI
  writes its session under the holder's config dir. No host login is copied in (plan v3 §7).
- **Per-job binding**: `HOME` is a private per-job dir; the credential store is mounted for the
  trusted child only; cache is private and discarded at close; the job container never sees the
  credential mount.
- **A5 decides supported vs unsupported.** If the CLI refreshes its session by rewriting only
  its auth file, that write goes through the serialized credential-maintenance operation
  outside job control, and job-generated config/cache never merges back. If instead it
  rewrites a combined state file mixing auth with per-run history, the profile is **marked
  unsupported in the initial release** rather than shipped with a lock and a hope.
- **Egress**: default deny, with a reviewed allowlist of the vendor API host only. The
  `CHATGPT_CODEX_UPSTREAM` constant in `codex_subscription.rs:10` is the existing precedent for
  pinning exactly one upstream.

## Step 5 — Grant (04)

> **SUPERSEDED.** No per-job token is minted, bound or expired. The daemon enrols once and the
> job is given an endpoint and a directory. The step that actually runs here is *attach*, and
> the per-call re-verification below is replaced by parameter validation and job-directory
> confinement, which are enforced on every call regardless of any grant. See
> [00](00-README.md); implemented in `crates/maxplayer-tool-kit`.

Token binds holder `H-docconv-1`, party `P`, service `doc-convert`, job `J`, grant version, and
an expiry inside `max_job_lifetime`. Every call re-verifies signature, audience, clock, an
**active** record, party/service equality, verb `convert` ∈ grant, and remaining budget.

Reservation before execution uses profile-declared maxima, not observed values, and each
counter names what it counts and what enforces it:

| Counter | Reserved | Enforced by |
| --- | --- | --- |
| calls | 1 | supervisor invokes once; a retry requires a fresh reservation |
| items | 1 | one input document, one output document |
| input bytes | 8 KiB | staging ingest bound, rejected **before** invocation |
| output bytes | 256 KiB | supervisor fails the job at the slot quota |
| network | 0 beyond the pinned upstream | default-deny egress |

`quality` does **not** bound output size; it only affects encoder quality. An earlier version of
this walk implied it did. Any counter without an enforceable tool or supervisor control is
**unbounded, and unbounded rejects before execution**.

Close on delivered and on every failure path. Timeout **does** reach `Failed`, but the write is
best-effort, so the holder closes on its own enforced expiry rather than waiting for that record
([04](04-token-grant-contract.md) Part II).

## Step 6 — Checker applicability (07)

All twelve checks apply. Three carry walk-specific oracles:

- **5.2 discovery/result**: the expected output is an independently produced file with a
  recorded digest, compared byte-wise. Not the CLI's exit code, and not the holder's own report.
- **5.4 artifact consumption**: the driver swaps a symlink at the upload entry during staging
  and again after validation. The consumption-time digest check must reject the swapped
  content, and no outside-file-read marker may fire.
- **5.6 grant authority**: includes the valid-cross-job-handle case above — `K`'s genuine handle
  presented with `J`'s genuine token must reject with zero child starts.
- **5.7 leakage**: the synthetic session value may appear only in the fake vendor's declared
  auth channel. Not in the output PDF's metadata — a real hazard for a tool that writes
  document metadata, and the reason `doc_title` is reviewed as a sink at all.

## Verdict

**Conditionally supported at rung 4 — conditional on A1–A5, not on A5 alone.** Since the subject
is an archetype, none of its assumed capabilities is verified; each becomes a required,
inspection-verified assumption before any profile ships. A1/A4 failing removes the route
entirely; A2 breaks the staging contract; A3 removes bounded-enum mapping; A5 decides custody.
The classification stays conditional until a real tool is inspected against all five.

Required profile properties:

1. constants must include an interactive-disabling flag, a plugin-disabling flag and a pinned
   config; a tool lacking any of these needs a documented equivalent or is unsupported;
2. auth-file writes must be separable from job state (A5), verified before build, not assumed;
3. output must be a single file into a holder-created slot, checked for symlinks and
   out-of-slot escapes before export;
4. literal-text sinks must be re-reviewed at every profile version bump.

Explicit unsupported/deferred cases for this archetype are carried into
[08](08-gaps-and-unsupported.md).
