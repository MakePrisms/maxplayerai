# Lifecycle-preserving scope adjustment — 23 September 2026

**Decision: Petar, 13:06 UTC. Documentation/design only; no runtime implementation.**

[Approval: “agree adjust accordingly”](https://discord.com/channels/1549984743666356239/1552254447013597214/1552305210293223466), following [the protocol-scope question](https://discord.com/channels/1549984743666356239/1552254447013597214/1552304227433447536) in the implementation-spec thread. The assistant recommendation immediately preceding the approval supplies the agreed adjustment below.

## Approved adjustment

- Preserve `offer → claim → award → execute → result → verify → accept → pay → receipt`.
- Do not add a post-award buyer-input handoff or a waiting-for-inputs lifecycle state.
- Open-pool offers must contain a complete executable task with prerequisites available when sellers assess it. Claims/award remain public. Subsequent progress explanations, answers and delivery files are private; no additional buyer input is required to start.
- Targeted-private offers provide task and required inputs to the target before it claims. Confidential prerequisites not suitable for public discovery use this mode initially.
- Remove the mandatory Maxplayer content acknowledgment/start gate. Every private job-content message still has a Maxplayer encrypted recipient copy; durable retries remain required. Offline Maxplayer consumption does not block execution.
- Necessary encrypted-content reference, private Git authorization and verification changes remain in scope. This preserves the lifecycle, not byte-for-byte protocol compatibility with unchanged clients.

## Superseded proposal details

This replaces the earlier spec's service `content-ack` handshake, post-award input/readiness gate, and separate activate/readiness storage-control workflow. It narrows the first-release open-pool behavior; it does not revoke the requirement for private execution content, Maxplayer access, or public initial discovery text.

No acknowledgment means clients cannot synchronously prove service decryption/receipt. Each recipient still verifies the same salted content commitment when received. The implementation must not claim stronger receipt assurance than it implements.

## Decision preservation

The verbatim September 22 content/file notes are unchanged historical records. Read them alongside this later adjustment and the current implementation specification. Numeric quotas and replacement-job behavior remain proposed defaults; this approval does not silently approve all other implementation choices.
