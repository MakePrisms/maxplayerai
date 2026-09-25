# Independent review resolution — 23 September 2026

Documentation review by **GPT-5.6-Sol**, separate from the authoring model. Original reviewed head: `207f38c8cc419eaf185b33cbd36a39b93de55008`. Petar requested resolution in [the implementation thread](https://discord.com/channels/1549984743666356239/1552254447013597214/1552330073783271477).

## Findings and disposition

- **Message rules:** [wire contract §1](../private-offers-wire-v2.md#1-mains-event-rules-remain-authoritative) inherits main's authors/phases and specifies the encrypted-content carrier and audience. Follow-ups remain new offers; reviews are subject-bound artifacts, not new lifecycle roles.
- **Public fields:** wire contract §2 defines closed per-kind tag shapes, enum/scalar values, nested invoice validation, private dispatch details and explicitly public discovery payloads. Unknown fields fail closed.
- **Delivery/payment binding:** §3 defines exact inline bytes, v2 receipt preimage, durable result-specific bind and existing RECEIPT reply reference. Main's public ACCEPT remains intentionally job-scoped. Random routing IDs do not replace offer-event IDs in payment identities.
- **Job-ID reuse:** §4 reserves one signed offer per buyer/job binding, including before award and after closure.
- **Diagram audience:** targeted seller input-read access before claim is explicit; selected-seller delivery writes require award.

The first re-review found four residual inconsistencies. Resolved: explicit shared targeted/public task payload; explicit publisher-selected coarse output category; 40-hex SHA-1 Git/contribution OIDs matching main; no inline REJECT path invented; consistent job-wide `(job_id,message_id)` deduplication.

## Final re-review

The reviewer inspected the revised working-tree specification and returned:

> APPROVE. No remaining blockers found.

It confirmed all four corrections and closure of the original findings without new lifecycle states, ACK gating, post-award input handoff or author roles. This is an independent **documentation review**, not implementation/security certification or runtime test evidence. No further product question was needed to resolve these findings.

## Validation

Whitespace checks, local document-link targets and SVG XML passed. The rendered PNG was visually inspected. Historical content-privacy and file-storage snapshots remain byte-identical. Acceptance scenarios (now 19) and cross-implementation vectors are requirements for future implementation, not tests executed by this documentation change. No runtime code, deployment, payment or merge performed.
