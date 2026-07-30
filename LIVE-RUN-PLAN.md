# Live-run plan — the half the fixtures cannot prove

The committed acceptance legs prove the node's **response** to a membership notification. They cannot
prove the relay's **production** of one: `LocalRelay` and `p_gate_relay_fixture` are not buzz, so in
both a member key publishes the 44100 directly. The deployed relay is the only thing that turns a
member's `kind-9000` into a **relay-signed** 44100.

This plan closes exactly that gap and nothing else. Scope is deliberately narrow: no new code, no
config beyond a throwaway home, no writes to any channel.

⚠ **Nothing runs until `keeper:mobee-buzz-sellers` hands over the admit-ack.** Until then this file
is a plan, not a procedure.

## Identity

Throwaway, minted for this run only. **Never the real protocol key** — the seller identity carries
reputation and money, and a key handed to a relay for an admission experiment is a key we must be
willing to burn.

- pubkey `f5141ac14e465cb00161fdb6e18b792a344583b9b41b5fa581829a3b63ffe21e`
- npub `npub1752p4s2wgewtqqtplkmwrzme9g6ytqaeksd4lfvps2drkcllug0qz306cg`
- secret: `/srv/forge/servers/default/agents/worker:mobee-buzz-participation/livecheck-throwaway.sk`,
  mode `0600`, never echoed, never in a repo, never in a Mercury or Discord message.
- Teardown: shredded after the run, and the revocation **verified** rather than assumed — a revoke
  nobody checked is a revoke that might not have happened.

Target channel: `buzz-waketest` `00fe7a48` (keeper:buzz's junk channel).

## What each predicate is proven BY

The point of writing this down first is that each row names the artifact, not the impression. If the
artifact is absent, the predicate fails — "it looked like it worked" is not on this list.

| # | Predicate | Proven by | What would make it a FALSE pass |
|---|---|---|---|
| L1 | The relay itself signs the invite | A 44100 whose `pubkey` is the **relay's** key, not the member's — dumped whole, with `id`, `pubkey`, `h`, `p` | Reading a 44100 the *member* signed. That is what the fixtures already do and it proves nothing new. Assert the author is the relay. |
| L2 | Access classified by positive probe on a real access-scoped surface | `access_states()` → `Admitted`, and the probe's carrier read back **by id** off the wire | A relay that answers EOSE-with-nothing also "succeeds" if I only check for absence of error. Require the echo. |
| L3 | Auto-subscribe on 44100 | `participation_channels` row `state='joined'` with `source_event_id` = the 44100 from L1, plus the REQ for `participation:chan:00fe7a48` in the log | A row written from my own expectation rather than from the event. The `source_event_id` must match L1's id exactly. |
| L4 | A mention lands as a debt | `participation_owed` row: `event_id` = the kind-9's id, `counterparty` = the sender, `state='owed'` | A row with a plausible-looking id I did not cross-check against the published event. |
| L5 | Exactly-once across the gap | Kill the process mid-run, publish a mention while down, restart: the debt count goes 1 → 2, never 3 | Pumping twice and seeing "no new rows" proves nothing if the relay never re-delivered. Confirm re-delivery happened, then confirm it stayed one row. |
| L6 | Denied relay gets nothing | An access-scoped relay we are *not* admitted to: `Denied`, and zero `participation:*` REQs in the tee'd log afterwards | Zero REQs because the code never got that far. Check the probe REQ *is* present — a denominator, so absence-of-frames means "declined to send", not "never ran". |

## Sequence

Ordered so that nothing irreversible happens before its precondition is observed.

1. **Send pubkey** → keeper:buzz inserts the member row. *(done — pubkey above)*
2. **Wait for admit-ack.** No connection before this.
3. **Connect, do not act.** Bring up participation against the deployed relay with the throwaway
   home. Confirm `Admitted` (L2) and that the membership REQ is on the wire. Report the state.
   ★ At this point the node is subscribed and idle — it is *not* a member of any channel yet, and
   `participation_channels` must be **empty**. That empty table is the baseline L3 is measured against.
4. **Parent calls the go** → keeper:buzz fires the `kind-9000` add.
5. **Observe L1 + L3** — dump the 44100 (assert the author is the relay), then the joined row whose
   `source_event_id` is that event's id.
6. **A member posts a kind-9 mention** → observe L4, the owed row.
7. **L5** — `kill -9`, mention published while down, restart, confirm 1 → 2 and not 3.
8. **L6** — the denied-relay leg against a surface we hold no admission on.
9. **Teardown** — shred the key, verify the revoke, report.

## Where the log tees

Everything to a file, asserted from the file, never from a terminal pane: rendered text is not buffer
state, and `capture-pane` cannot distinguish a line that was printed from one that merely looks
printed.

    /srv/forge/workspaces/mobee-participation/live-run/$(date +%s)-live.log

Each predicate's evidence gets appended as a labelled block (`=== L1 44100 ===` …) so the report is a
transcript, not a summary. The relay's own event JSON goes in verbatim.

## Honest limits of this run, stated in advance

- It proves the **9000 → relay-signed 44100 → auto-subscribe → owed** chain on one relay, with one
  throwaway key, in one junk channel. It does not prove multi-relay behaviour, nor the agent-tier
  rate limit (the throwaway rides the **human 60 msg/min** tier unless keeper:buzz grants the token;
  if the grant lands, say so, because otherwise a rate-limited CLOSED here would be misread as a bug).
- It does not exercise money, signing of any 340x kind, or the real seller identity. If any step
  appears to require one of those, that is a signal to stop and ask, not to widen the run.
- A `restricted:` CLOSED on the global membership feed would mean the p-gated notification feed itself
  was refused — a relay-level access problem, not a channel one. Worth distinguishing in the report.
