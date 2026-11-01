# Initial file storage: Git

**Decision: Petar, 22 September 2026. Design decision only; not implemented by this documentation update.**

[Discussion and approval](https://discord.com/channels/1549984743666356239/1551929287270203404/1551965351590502527)

## Decision

Use Git for job input attachments and delivery files for now. Do not introduce a
separate blob/file-storage service for the initial private-jobs implementation.
For private jobs, use the proposed per-job private repository with authenticated,
job-scoped access for the buyer and seller and authorised Maxplayer access.
Maxplayer is trusted to read the content; recipient-level encryption of Git file
bytes is not required by this privacy model. Transport must remain protected.

This selects the file-storage approach, not the final text-message protocol,
repository permissions over time, retention period, or numeric size limits.
Input attachment upload/client support still needs implementation; this decision
does not imply that an attachment API already exists. Credentials stay outside Git.

## Rationale and limits

The normal from-scratch delivery is a final workdir snapshot in one root commit,
not the agent's incremental editing history. Contribution deliveries instead have
the buyer-pinned base as their parent. Thus ordinary single-snapshot deliveries do
not have the repeated-binary-version overhead of a long-lived development repo.
Input uploads and subsequent deliveries/revisions may still add stored objects.

Define and enforce per-file and total-repository limits during implementation.
Large pushes/fetches still consume disk, memory, bandwidth, and processing time.
Git transport is not a resumable per-file upload API. Private access checks must
cover every read path, including any file preview/download endpoint.

## Deferred improvement: separate blob storage

Revisit file storage when measured workloads require very large binaries,
resumable uploads/downloads, independent file retention, or Git transfer/storage
costs become problematic. There is no commitment to build this in the first release.

Evaluate existing options such as a Blossom implementation or an object-storage
backend before building a custom service. Blossom is mentioned in the deployment
roadmap, not a confirmed integrated private-job storage service.

Any replacement must preserve the same job-level permissions and Maxplayer review
access, integrity-bound file references, quotas, and cleanup/backup policy.
Knowing a URL or content hash must not grant access. Storage changes must preserve
verification of already-issued delivery references.
