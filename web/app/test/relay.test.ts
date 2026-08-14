/**
 * Relay source lifecycle — the paths that only appear when the network
 * misbehaves, driven through the injectable socket rather than a real one.
 *
 * These are the cases a browser hits constantly (a dropped socket mid-read)
 * and a test never hits by accident, which is why the module shipped with the
 * history-gap bug: every happy-path read looked identical to a correct one.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { createRelaySource } from "../src/source/relay.js";
import type { RawEvent } from "../src/model/events.js";

/** A socket the test drives by hand. Records every frame the source sends. */
class FakeSocket {
  readyState = 1;
  sent: unknown[][] = [];
  closed = false;
  onopen: (() => void) | null = null;
  onmessage: ((msg: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;

  send(text: string): void {
    this.sent.push(JSON.parse(text) as unknown[]);
  }
  close(): void {
    this.closed = true;
  }

  /** The REQ frames this socket was asked to make, in order. */
  get reqs(): { sub: string; filter: Record<string, unknown> }[] {
    return this.sent
      .filter((f) => f[0] === "REQ")
      .map((f) => ({ sub: String(f[1]), filter: f[2] as Record<string, unknown> }));
  }

  deliver(sub: string, event: RawEvent): void {
    this.onmessage?.({ data: JSON.stringify(["EVENT", sub, event]) });
  }
  eose(sub: string): void {
    this.onmessage?.({ data: JSON.stringify(["EOSE", sub]) });
  }
}

const T0 = 1_700_000_000;
const ev = (created_at: number): RawEvent =>
  ({ id: String(created_at).padStart(64, "0"), kind: 30078, pubkey: "a".repeat(64), created_at, tags: [], content: "" });

/** Drive the source with fake sockets and a timer we fire by hand. */
function harness(options: { sinceHint?: number | null; storeComplete?: boolean } = {}) {
  const sockets: FakeSocket[] = [];
  const timers: (() => void)[] = [];
  const events: RawEvent[] = [];
  const source = createRelaySource(
    {
      url: "wss://test.invalid",
      transport: "poll",
      ...options,
      openSocket: () => {
        const s = new FakeSocket();
        sockets.push(s);
        return s as unknown as WebSocket;
      },
      now: () => T0 + 10_000,
      setTimer: ((fn: () => void) => { timers.push(fn); return timers.length as unknown as ReturnType<typeof setTimeout>; }),
      clearTimer: () => {},
    },
    { onEvent: (e) => events.push(e), onStatus: () => {}, onSynced: () => {} },
  );
  return {
    source,
    sockets,
    events,
    /** Fire the pending reconnect backoff. */
    runTimer: () => timers.shift()?.(),
    latest: () => sockets[sockets.length - 1]!,
  };
}

test("a drop mid-history resumes the same stream where it stopped, never forward past unread events", () => {
  // THE BUG: history re-requested with `since: newestSeen + 1` after a drop.
  // newestSeen is advanced by every ingested event, so the second read asks
  // only for events NEWER than the newest one already seen — everything older
  // that had not yet been paged is never requested again. main.ts persists
  // with persist:true, so the hole is written to IndexedDB and survives every
  // future visit.
  const h = harness();
  h.source.start();
  const first = h.latest();
  first.onopen!();

  // One full page of history: newest at T0+900, oldest at T0+100.
  const page = first.reqs[0]!;
  assert.equal(page.filter.since, undefined, "a cold read walks backward, it does not ask for a since");
  first.deliver(page.sub, ev(T0 + 900));
  first.deliver(page.sub, ev(T0 + 100));
  first.eose(page.sub);

  // The socket drops before the next page arrives.
  first.onclose!();
  h.runTimer();
  const second = h.latest();
  assert.notEqual(second, first, "the drop reconnects");
  second.onopen!();

  const resumed = second.reqs[0]!.filter;
  assert.equal(
    resumed.since,
    undefined,
    "a reconnect mid-history must NOT jump forward past history it never read",
  );
  assert.equal(
    resumed.until,
    T0 + 99,
    "it resumes paging below the oldest event that stream had reached",
  );
});

test("the since-hint is used only when the cached store is known complete", () => {
  // A store is only a valid floor for a forward read if a history walk ever
  // finished. Trusting the hint unconditionally makes a partial cache
  // unrepairable: every later visit resumes above the gap it already has.
  const partial = harness({ sinceHint: T0 + 500, storeComplete: false });
  partial.source.start();
  partial.latest().onopen!();
  assert.equal(
    partial.latest().reqs[0]!.filter.since,
    undefined,
    "an unproven store is re-walked, not trusted",
  );

  const complete = harness({ sinceHint: T0 + 500, storeComplete: true });
  complete.source.start();
  complete.latest().onopen!();
  assert.equal(
    complete.latest().reqs[0]!.filter.since,
    T0 + 501,
    "a proven-complete store still gets the cheap forward read",
  );
});
