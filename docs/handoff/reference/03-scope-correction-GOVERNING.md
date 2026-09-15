<!-- PROVENANCE: copied verbatim into this repository for handoff on 2026-09-10.
     Original lived on the authoring machine outside this repo and is NOT reachable to you.
     Absolute /Users/... paths appearing below are historical references from the time of
     writing; the in-repo copies under docs/handoff/reference/ are now authoritative.
     Content is unmodified apart from this header. Scanned for credentials: none found. -->

# Governing scope correction — Petar, 2026-09-09 11:18 PDT
Source: message 1547310280747257927, thread 1546802028091670630.
Petar: "the seller is defined by it's offering, there is no offering per job, it is per seller, tool should at all times be active together with the seller daemon".

This supersedes contradictory award/per-job admission requirements in v3 and the stage-0 contract. Offering and allowed tool operations are seller configuration. Tool availability follows the seller daemon, not a job award, payment, job start or job completion. Jobs of that seller use the configured interface; finishing a job does not stop the tool or log it out.

Remove marketplace award eligibility adapters, per-job tool grant issuance, award replay gates and marketplace job-state changes from this task. Do not make their review a prerequisite for the demonstration. Keep job file/work isolation where needed; it does not create an offering or tool entitlement per job.

Preserve credentials outside buyer-controlled job containers; no arbitrary shell/argv passthrough; validate operation parameters and confine file access. Restrict endpoint access to the seller's authorized clients, not the public or other sellers. Auth persistence/re-enrollment, daemon start/stop supervision, health failures and cleanup remain relevant. Availability is intended while the daemon runs, not a guarantee against vendor failures. Seller controls allowed operations and accepts that buyer-directed jobs can exercise them; do not claim protection from every authorized-operation abuse.

Implementation next: same worker/worktree, custom CLI + fake authenticated service in Linux Docker, synthetic credentials only. Demonstrate one enrollment persisting across two sequential jobs, shared seller-level MCP operation list, credential unreadability from job containers, invalid-input/cross-job-file rejection, tool startup/stop tied to daemon lifecycle and visible unhealthy state on auth failure. Include real job-container MCP connection (or explicitly label a stand-in if blocked); no claim of production integration from a mock alone.

Update superseded docs concisely alongside implementation; no new broad paper-only review loop. First deliver the bounded executable prototype with runnable tests and exact evidence, then maxie verifies and advisor reviews the implementation against this correction. Preserve truthful distinction between synthetic mechanism evidence and independent real-tool acceptance. No live account, spend, merge, tag or main push. Original final deliverable remains reviewed ready PR and link in the originating thread.
