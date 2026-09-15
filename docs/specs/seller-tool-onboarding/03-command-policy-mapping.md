# 03 — Command-policy profile and source-to-sink mapping

Paper artifact. **PROPOSED** throughout. Anchored in plan v3 §3.

## The profile is the unit of review

A command-policy profile is the only place a command comes into existence. It pins:

| Pinned element | Note |
| --- | --- |
| executable / image identity | by digest, not by name on `PATH` |
| subcommand | fixed string, not a field |
| constant options | fixed, and reviewed under the constant policy below |
| permitted environment names | allowlist; environment otherwise starts empty |
| stdin format | declared shape, or "no stdin" |
| credential lookup location | a store name, resolved by the holder — never a manifest value |
| operand interpretation | what the vendor does with each operand |
| permitted effects | the declared maximum per invocation |

Any change to any of these requires review and a **new profile version**. A profile version is
content-addressed; drift invalidates a prior acceptance run (plan v3 §5.12).

Review is by a human with authority over the seller's account. A seller-agent's description of
what a flag does is evidence to check, never an approval input.

## Source-to-sink mapping — required for every field

For every field the profile declares, and for every constant it pins, the profile carries a
row with all five columns. A field with an unmapped column is rejected: **reject unknown
mappings** is a validation rule, not a review guideline.

| Column | Meaning |
| --- | --- |
| **source type** | one of the seven manifest types, or `constant` |
| **validation / encoding** | grammar or bound checked, and the encoding applied on the way out |
| **sink** | the exact argv position, or the exact stdin/HTTP field path |
| **vendor interpretation** | what the vendor's own parser does with this value |
| **allowed effect** | the effect this field can cause, bounded |

### The literal-text rule, stated as a sink constraint

Bounded literal text is permitted **only where that command treats the value as data**. It is
forbidden where the vendor interpretation column would read: a URL, a filesystem path, an
expression or format string, a response/output file selector, or a configuration selector.

A mapping whose source is literal text and whose vendor interpretation is any of those is a
`text-to-URL`-class mapping, and rejects at profile review — and, as a defence in depth, at
schema load (plan v3 §5.1 requires the load-time rejection with zero child calls).

**Resource membership does not replace vendor-specific encoding.** Proving `res:R` is in the
job's grant says nothing about how the vendor parses the string `res:R` in argv position 4.
Both checks are required; neither substitutes for the other.

**Enum values and constants get the same authority review as variable fields.** An enum whose
third member happens to name a plugin, and a constant that happens to be `--config`, are
exactly as dangerous as an unvalidated free-text field, and are reviewed identically.

## Constant policy

Profile constants bind account, tenant, endpoint and credential selector to seller-approved
policy. A constant is **rejected** when it enables any of:

- shell execution, or any option that spawns or evaluates;
- plugin, extension or module loading;
- arbitrary configuration file selection;
- debug/verbose modes that dump secrets;
- an uncontrolled destination (upload target, callback, mirror, remote write).

There is no shell anywhere in the invocation path: the supervisor `exec`s the pinned
executable with an explicit argv vector. A `--` terminator may be pinned to help the vendor's
own parser, but it establishes parsing behaviour only, never operand safety.

## Environment

Starts empty. Only reviewed tool/runtime variable names may be added by the profile. Never
job-controlled: `PATH`, `HOME`, any `*_PROXY`, any loader variable (`LD_*`, `DYLD_*`), or any
credential selector variable. `HOME` is set by the supervisor to a private per-job directory
(see [04](04-token-grant-contract.md)), not by the profile and never by the manifest.

## Uploaded artifacts — the staging contract

An uploaded artifact becomes a **holder-owned immutable input object**:

1. **ingest** under a declared size bound;
2. **open and validate without following links** — no symlink traversal, no device, no FIFO;
3. **copy** into a holder-private staging directory that the job cannot reach;
4. **retain unchanged** through CLI consumption, verified by digest at consumption time.

The CLI receives only the private staged path. The job holds an opaque handle and never a
path. The job cannot rename the staging parent and cannot swap the content: the digest checked
at consumption must equal the digest recorded at validation, which is what makes the
validate-then-swap race in plan v3 §5.4 a detectable FAIL rather than a silent success.

Output slots are holder-created, holder-private, and checked **before** export: reject
symlinks, devices and any path resolving outside the slot. No raw host path enters the API in
either direction.

## Worked mapping — fixture CLI `acme-render-cli` v3

Invocation the profile authorizes, and nothing else:

```
/opt/acme/bin/acme-render render \
  --config /holder/private/acme.toml   \  # constant
  --account seller-primary             \  # constant
  --no-plugins                         \  # constant
  --format pdf                         \  # enum field
  --pages 20                           \  # integer field
  --title hello-world                  \  # literal-text field
  --out /holder/private/job-J/out/1     \  # destination slot
  --                                    \
  res:R                                    # grant-bound resource id
```

| Field | Source | Validation / encoding | Sink | Vendor interpretation | Allowed effect |
| --- | --- | --- | --- | --- | --- |
| executable | constant | image digest pinned | argv[0] | binary identity | — |
| `render` | constant | fixed | argv[1] | subcommand | — |
| `--config` value | constant | holder-private, seller-authored | argv[2..3] | config file | selects account + endpoint only |
| `--account` value | constant | seller policy | argv[4..5] | account selector | binds billing identity |
| `--no-plugins` | constant | fixed | argv[6] | disables extension load | closes plugin sink |
| `format` | bounded enum `{pdf,png}` | membership | argv[7..8] | output codec | none beyond codec |
| `page_limit` | bounded integer `1..=20` | range | argv[9..10] | page cap, enforced by the tool | items ≤ the bound value |
| `title` | bounded literal text | fixture grammar (§02) | argv[11..12] | **data**: drawn into the document header | none |
| `out` | destination slot | holder-created, private | argv[13..14] | output file path | writes one file in-slot |
| `res:R` | grant-bound resource id | grant membership **and** `res:[A-Za-z0-9_-]{1,32}` encoding | argv[16], after `--` | record selector | reads one record |

Notes that make this mapping reviewable rather than decorative:

- `title` is mapped to data because this command draws it into a header. The **same field name
  on a different tool** whose `--title` selects a template file would be a text-to-URL-class
  mapping and would reject. The mapping is per-tool, never inherited.
- `res:R` carries two independent checks: grant membership, and the `res:` encoding grammar.
  Passing the first without the second is the mistake plan v3 §3 names explicitly.
### Deriving the effect maximum

A declared maximum is **derived from the binding, not asserted beside it**. For this profile:

| Counter | Maximum | Derivation | Enforcement |
| --- | --- | --- | --- |
| calls | 1 | this verb invokes the tool exactly once; no retry is authorized | supervisor invokes once; a retry needs a fresh reservation |
| items | value of `page_limit`, ≤ 20 | the page cap is the item cap | tool-enforced page cap, plus supervisor count of exported objects |
| input bytes | 8 KiB | the staging ingest bound (§ uploads) | rejected at ingest, before invocation |
| output bytes | 256 KiB | slot quota | supervisor truncates-and-fails at the slot boundary |
| network | 0 additional | egress default-deny except the one pinned upstream | egress policy |

**Byte counters are separate and named.** "Bytes" alone is ambiguous; input, output and network
are reserved and accounted independently, and a manifest that declares one does not bound the
others.

Two rules this table enforces:

- **The profile's `page_limit` upper bound and the item ceiling are the same number.** Where a
  profile permits `1..=50` pages, its item maximum is 50, not a smaller figure chosen for a
  worked example. A manifest binding `page_limit: 20` with `max_items: 3` is **internally
  inconsistent and rejects at validation** — it is not a tighter ceiling, it is a contradiction.
- **An effect with no enforceable control is unbounded, and unbounded rejects before
  execution.** A quality or format setting alone supplies no maximum. If neither the tool nor
  the supervisor can cap a counter, the profile does not ship.

## Deferred: HTTP profiles

A deferred HTTP profile must additionally constrain nested body/query values, batch counts,
resource selectors, redirects and destinations. **Fixed host plus fixed method plus arbitrary
nested strings is not an acceptable policy** and must not be described as one.

No claim of HTTP support is made until that profile and checker work passes its own
acceptance. Until then the router returns `recognized shape, template deferred`. See
[06](06-walk-b-tenant-aware-http.md) for the walk that demonstrates why.
