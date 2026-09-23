# Decision record preservation and coverage

Added following [Petar's 23 September request not to lose the prior discussion](https://discord.com/channels/1549984743666356239/1552254447013597214/1552259274980335667).

## Source and precedence

- [Later lifecycle scope adjustment](lifecycle-scope-adjustment.md): Petar’s 23 September approval to preserve the lifecycle, remove the post-award input handoff and remove mandatory Maxplayer ACK/start gating. This is a new decision record, not an edit to the verbatim snapshots.

- [Content privacy decisions](content-privacy-decisions.md): verbatim from [source](https://github.com/maxie-agent/maxplayerai/blob/1ac3fbb3f27b89fe6dd17936990af53afcf22fb8/docs/proposals/private-offers/content-privacy-decisions.md).
- [File storage decision](file-storage-decision.md): verbatim from [source](https://github.com/maxie-agent/maxplayerai/blob/1ac3fbb3f27b89fe6dd17936990af53afcf22fb8/docs/proposals/private-offers/file-storage-decision.md).
- [Implementation specification](../private-offers-and-deliveries.md): proposed implementation of those decisions plus the later open-pool decision cited in §0.

The snapshots include the full original field contract, clarifications, exclusions and rationale, not only implementation work items. Relative links between the two notes continue to resolve. These are historical snapshots at the named commit; compare future changes explicitly rather than assuming they synchronize automatically. They are not an archive of every chat message. Original discussion links remain in the records and §0.

Settled requirements take precedence over proposed defaults. No claim that all implementation work is done is implied by “covered”: the tests below are requirements, not executed evidence. Sections/tests refer to the implementation specification.

## Content-decision coverage

| Source requirement or clarification | Implementation coverage / acceptance gate |
| --- | --- |
| Same encrypted mechanism for task, follow-ups, answers, feedback, rejection and review | §3 private fields; §4.1 message types; tests 1, 4–6 |
| Maxplayer recipient copy for every private job-content message, not implicit relay access | §4.2 recipient policy; tests 1, 4; mandatory ACK removed by the later scope decision; copies and independent validation remain required |
| Maxplayer reads every private job repo; Git initially stores files/attachments | §§5–6 role and storage contract; tests 9–12 |
| External resources, permissions and externally managed credentials excluded; links inside private messages stay private | §0 exclusions; §§3, 6 private URLs and externally managed dependencies; test 6 |
| Existing lifecycle/retention policy; no deletion-policy redesign | §§0, 5–6, 9; trusted-backup clarification in §6 |
| Do not broaden payment-token/key/secret recipients | §§0, 4.2, 7; PR1 failure gate and test 13 |
| Public envelope, identities/references, times/prices/payment metadata | §3 explicit allowlist |
| Public coarse status/reasons; approved categories/numeric usage only, no free-form labels | §3 allowlist and rejection of content-bearing tags; test 6 |
| Settlement signatures/integrity identifiers public but not access grants | §§3, 6–7; test 9 |
| Opaque identifiers; no guessable task-plaintext hash | §§3–4 salted commitment, §7 v2 job/inline binding; test 6 |
| Task, requirements, criteria, descriptive titles/summaries private | §§3–4; tests 1, 6 |
| Follow-ups and previous-turn context private | §§3–4 and §6 explicit context carry-forward; tests 6, 14 |
| Progress/feedback/rejection/error explanations private | §§3–4, 7; test 6 |
| Review/classifier findings/excerpts private; no implicit public-verdict format | §§3–4; tests 6, 16 |
| Attachment manifests, filenames, paths, descriptive URLs/previews private | §§3–4, 6; tests 6, 9, 12 |
| File bytes, trees, history and commit messages protected | §§3, 6–7; tests 9, 12 |
| No duplicated plaintext in public lifecycle event | §3 serializer contract; test 6 |
| Review binds to the same offer/delivery version the participant receives | §4.1 required review subject; test 16 (explicitly strengthened in this update) |
| Public/open discovery text stays public | §§1, 3, 5; tests 2–3 |
| Public network UI must not display encrypted content as plaintext | §2 code anchor; PR3 UI work; test 6 |
| Unused episode/telemetry capture/emission explicitly excluded; not an active leak claim | §§0, 6; full historical source-inspection nuance preserved in snapshot |
| Seller memory explicitly excluded | §§0, 6 |
| Trusted internal pack cache need not be recipient-encrypted; permission applies on cache hits | §§2, 6; test 9 |
| Backups within trusted Maxplayer storage fit model; no new policy or claimed live bucket audit | §6 explicit clarification; full deployment observation preserved in snapshot |
| Cache example hypothetical; GitReadAuth already precedes hydration, but lacks job-level ACL | §§2, 6, 10; test 9; no independent cache redesign |

## File-decision coverage

| Source requirement, rationale or deferral | Implementation coverage / disposition |
| --- | --- |
| Git first; separate private job repo; authenticated buyer/seller/Maxplayer access | §§5–6; tests 9–12 |
| File bytes need not be recipient-encrypted; protected transport still required | §6 trusted-storage boundary; HTTPS/WSS requirement below |
| Input upload is new implementation work, not an existing API claim | §5 proposed idempotent storage operation; §6 targeted pre-claim input snapshots; PR2/PR3 |
| Per-file/total limits must be defined | §§6, 11 proposed limits; test 11 |
| From-scratch snapshot versus contribution history explains Git tradeoff | Full rationale retained verbatim in file snapshot; §6 counts both inputs and imported history |
| Quotas cover growing inputs/revisions and transfer/resource costs; Git is not resumable file upload | §6 cumulative accounting and transport limits; snapshot retains transport limitation |
| Every read path, including previews/downloads, needs authorization | §6 route inventory and gate; test 9 |
| Blob storage deferred until measured need; evaluate existing options first | §0 exclusion and §2 reuse; full trigger criteria and Blossom caveat retained in snapshot |
| Future replacement preserves permissions, review access, integrity, quotas, lifecycle and old references | Preserved in snapshot as a future-storage constraint, not added current-release work |

Private-job Git/API connections must use HTTPS and content relay connections WSS outside local test fixtures; do not follow a redirect that sends authorization to another origin. This makes the source note's protected-transport requirement explicit for implementers.

## Changes made after the comparison

Most core requirements were already represented, but the implementation PR previously only linked to the older decision branch. This update makes those records available in the same PR, adds this coverage map and precedence rule, explicitly binds review subjects to exact artifact versions, and spells out trusted-backup/transport scope. The later lifecycle decision explicitly removes mandatory ACKs and post-award input handoff; quotas and replacement behavior remain proposals.

## Later scope adjustment coverage

- Existing lifecycle/no new waiting or READY state: spec §§1, 3, 5; tests 1–2, 7.
- Complete executable open-pool task; private progress/answers/delivery, but no later buyer input: §§1, 5.2; test 2.
- Targeted private task/inputs fetched and validated before claim: §§1, 5.1, 6; tests 1, 12.
- No mandatory Maxplayer ACK/start gate; required recipient copy and durable retries remain: §§4.2–4.3; tests 4, 7, 15. No synchronous receipt/decryption proof is claimed.
- Wire/storage changes remain distinct from lifecycle changes: §§3, 5.3, 7.
- [Updated flow diagram](../private-offers-flow/README.md) supersedes the earlier Discord diagram’s post-selection input boxes and proposed ACK/start gate.
