# Issue 599 Phase 0 kind-range considered-sites report

The kind-3407 addition was swept across the repository with searches for `3406`, explicit trade-kind
ranges, registry arrays, relay accept lists, and lifecycle tables. Each considered site is classified
below; “verified still correct” means its statement is specifically about ACCEPT rather than the end
of the owned range.

| Site | Result | Reason |
|---|---|---|
| `crates/maxplayer-core/src/kinds.rs` module range/table/registry test | CHANGED | Owned contiguous block and registry now end at 3407. |
| `docs/README.md` protocol map (the README occurrence at line 27 before this change) | CHANGED | Summary range now ends at 3407. |
| root `README.md` | VERIFIED-STILL-CORRECT | Contains no owned-kind range/count statement. |
| `docs/protocol.md` range, flow, and kind table | CHANGED | Summary now describes checks/reject and kind 3407. |
| `docs/protocol-v1.md` §4 table and §5 `t`-tag MUST clause | CHANGED | Kind and namespace range include 3407. |
| `docs/protocol-v1.md` existing §7.5, §11, §12 ACCEPT references | VERIFIED-STILL-CORRECT | These define ACCEPT's distinct kind 3406, not a range endpoint. |
| `docs/protocol-v1.md` §8 flows and §9 lifecycle-root list | CHANGED | Verify recompute and Reject flows/root membership were added. |
| `docs/DEPLOYMENT.md` relay accept-list | CHANGED | Operators must allow kind 3407. |
| `crates/maxplayer-core/src/job_lifecycle.rs` kind-3406 comments | VERIFIED-STILL-CORRECT | They describe ACCEPT-only parsing and publication. |
| `crates/maxplayer-core/src/buyer/mod.rs` kind-3406 comments/tests | VERIFIED-STILL-CORRECT | They distinguish persisted ACCEPT facts from AWARD and do not state a range/count. |
| `crates/maxplayer-core/src/seller_node/run.rs` kind-3406 comments/tests | VERIFIED-STILL-CORRECT | ACCEPT remains 3406; the live decision subscription was separately widened to 3407. |
| `web/app/js/kinds.js` ACCEPT constant/reader list | VERIFIED-STILL-CORRECT | This app does not yet consume REJECT; its 3406 references are ACCEPT-specific. Adding an ungated web reader would violate Phase 0's author-gate rule. |
| `web/network/js/kinds.js` ACCEPT constant/reader list | VERIFIED-STILL-CORRECT | The observatory does not yet consume REJECT; its 3406 references remain ACCEPT-specific. |
| `web/network/js/parse.js` 3405/3406 split comment | VERIFIED-STILL-CORRECT | It explains why ACCEPT must not parse as AWARD; it is not a range endpoint. |

No other tracked file contained a `3400`–`3406`/“through 3406” range statement after the sweep.
