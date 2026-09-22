# Content privacy decisions — 22 September 2026

Design decisions, not implemented by this documentation update.
[Petar's instruction](https://discord.com/channels/1549984743666356239/1551929287270203404/1551966060775739444).
This note supersedes conflicting text-storage recommendations in the original proposal.

## Agreed direction

- Use the same recipient-encrypted message mechanism for private task text, follow-ups,
  delivery answers, feedback explanations, rejection details, and review findings:
  NIP-44 encryption with the NIP-17/NIP-59 per-recipient envelope pattern discussed.
- Every private job-content message must also be readable by Maxplayer through a
  recipient copy for its designated service identity. Buyer/seller copies remain
  available to the appropriate participants. This is not automatic merely because
  Maxplayer operates the relay. The exact marketplace wire schema remains to implement.
- Maxplayer can read every private job repository. Files and attachments use Git
  initially; see [the file-storage decision](file-storage-decision.md).
- External resources, their permissions, and externally managed credentials are
  outside this effort. Links contained in a private message remain private as part
  of that message; no external-resource mirroring or access-management feature is required.
- Do not add a retention/deletion-policy redesign to this effort. Existing lifecycle
  policy remains the baseline. Enforcing job-scoped access is still required.
- This does not broaden the existing recipient set for payment tokens, private keys,
  or other payment secrets. “Every message” here concerns private job content.

## Explicit public/private field contract

The following is the documented design allowlist, not a claim about current v1 behaviour.
All unspecified or new fields require classification before public publication.

| Field/content | Private-job treatment |
| --- | --- |
| Event kind, protocol version, event ID/signature, author | Public coordination envelope |
| Buyer/seller identities, selected seller, offer/claim/award/result references | Public |
| Timestamps, deadlines, price/amount, payment mode, currency, mint | Public |
| Coarse lifecycle status and enumerated reason codes | Public; no interpolated task text |
| Standard capability/output categories and approved numeric usage fields | Public only through an explicit schema allowlist; no free-form labels |
| Settlement receipt signatures and delivery integrity identifiers | Public under the existing verification contract; identifiers do not grant read access |
| Opaque job/content identifiers | Public if needed for routing; never derive them from guessable task plaintext |
| Task, requirements, acceptance criteria, descriptive title/summary | Recipient-encrypted, including Maxplayer copy |
| Follow-ups, previous-turn context, delivery answers | Recipient-encrypted, including Maxplayer copy |
| Free-form progress, feedback, rejection/error explanations | Recipient-encrypted, including Maxplayer copy |
| Review/classifier explanations, findings, excerpts | Recipient-encrypted, including Maxplayer copy; any future public verdict needs its own schema |
| Attachment manifests, filenames, paths, descriptive URLs and previews | Private message or protected job repository, not public tags |
| Input/output file bytes, Git trees, history, commit messages | Protected job repository, readable by Maxplayer |
| Payment secrets and credentials | Never public; preserve existing payment protection; external credential management out of scope |

A public lifecycle event must not duplicate plaintext from its private payload.
A reviewer assessment must bind to the same offer/delivery version the participant
receives. Do not publish a plain hash of short task text as a privacy substitute.
Public/open offers remain explicitly public; do not apply this private-job content
classification to hide their discovery text.

## Concrete secondary paths checked (source, not a live deployment audit)

Inspected upstream commit `135e4ea0bd5330f7ab0272d501aa83a718edc777`:

- **Public network UI:** `web/network/js/parse.js` extracts the offer task from the
  `i` tag and feedback text from event content. Private producers must omit plaintext;
  the public UI must handle encrypted/private content without presenting it as text.
- **Episode/telemetry definitions:** `crates/maxplayer-core/src/episode.rs` defines
  `offer_task` in `episodes.jsonl`; `telemetry.rs` defines forwarding a full episode to
  an optional sink command and mirror file. This inspection does not establish that
  these capture/emission paths are wired into every current seller job. If used for
  private jobs, they must not forward plaintext to public/shared destinations.
  Local storage by an authorised participant is not itself a privacy violation.
- **Seller memory:** `seller_memory.rs` defines episode/transcript distillation and
  `MEMORY.md` injection. `seller_node/run.rs` calls `job_memory_section` for job prompts.
  Cross-job reuse of private facts would be a disclosure route; the distillation
  template alone is not proof of an active automatic distillation path or an actual leak.
- **Git pack cache:** `crates/buzz/crates/buzz-relay/src/api/git/pack_cache.rs` stores
  immutable pack/index pairs; `hydrate.rs` builds temporary repositories from them.
  Job authorization must still constrain the served repository/object set on cache
  hits. A trusted server's internal cache does not itself require recipient encryption.
- **Backups:** `nix/relay-host.nix` configures Postgres event-log backups to S3 and
  describes versioned Git-CAS/media objects in S3. This is deployment configuration,
  not verification of live bucket access. No new backup/retention policy is required
  here: backups staying within trusted Maxplayer storage are consistent with the model.

These are bounded implementation checks, not a generic logs/caches/backups workstream
or a claim that any of these mechanisms currently leaks private jobs.
