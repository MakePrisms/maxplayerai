/**
 * The snapshot loader's failure paths. The boot sequence depends on this
 * returning — under every condition, including the one that never returns on
 * its own.
 */
import assert from "node:assert/strict";
import { test } from "node:test";
import { loadSnapshot } from "../src/store/snapshot.js";

const URL_ = "./snapshot.json";

test("a STALLED fetch gives up on its own deadline instead of holding the boot open", async () => {
  // The failure the original code could not survive: not a rejection (its
  // catch handled those) but a request that simply never settles. Without a
  // signal this promise is still pending when the page has been open for an
  // hour — no relay, no error, skeletons forever.
  let observed: AbortSignal | undefined;
  const hang: typeof fetch = (_url, init) =>
    new Promise((_resolve, reject) => {
      observed = init?.signal ?? undefined;
      observed?.addEventListener("abort", () => reject(observed!.reason));
    });

  // AbortSignal.timeout's timer does NOT hold the event loop open. With nothing
  // else pending, the loop drains while this fetch is still in flight, and the
  // runner tears the file down ("Promise resolution is still pending but the
  // event loop has already resolved") — which reports as CANCELLED, not failed.
  // A browser always has a live loop; a test has to supply one.
  const keepAlive = setTimeout(() => {}, 10_000);
  const started = Date.now();
  const result = await loadSnapshot({ url: URL_, timeoutMs: 50, fetchImpl: hang });
  clearTimeout(keepAlive);

  assert.ok(observed, "the fetch is given an abort signal");
  assert.equal(result.outcome, "absent");
  assert.deepEqual(result.events, []);
  assert.ok(Date.now() - started < 2000, "it returned on the deadline, not never");
});

test("absent and unreadable are different outcomes, and neither throws", async () => {
  const missing: typeof fetch = async () => new Response("not found", { status: 404 });
  assert.equal((await loadSnapshot({ url: URL_, timeoutMs: 50, fetchImpl: missing })).outcome, "absent");

  // A truncated bake — served with 200, so only parsing reveals it. This is a
  // deploy fault, not a missing file, and must not be reported as the same.
  const truncated: typeof fetch = async () => new Response('[{"id":"aa', { status: 200 });
  assert.equal((await loadSnapshot({ url: URL_, timeoutMs: 50, fetchImpl: truncated })).outcome, "unreadable");

  // Valid JSON that is not a list of events would otherwise be spread into
  // the ingest loop as garbage.
  const wrongShape: typeof fetch = async () => new Response('{"events":[]}', { status: 200 });
  assert.equal((await loadSnapshot({ url: URL_, timeoutMs: 50, fetchImpl: wrongShape })).outcome, "unreadable");
});

test("a good snapshot loads", async () => {
  const ok: typeof fetch = async () => new Response(JSON.stringify([{ id: "a".repeat(64), created_at: 1 }]), { status: 200 });
  const result = await loadSnapshot({ url: URL_, timeoutMs: 50, fetchImpl: ok });
  assert.equal(result.outcome, "loaded");
  assert.equal(result.events.length, 1);
});
