import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { IDBFactory } from "fake-indexeddb";
import {
  auth,
  claim,
  Event,
  hash,
  identity,
  models,
  characters,
  namespace,
  NEMO,
  offerTags,
  outgoing,
  preimage,
  promptText,
  RELAY_HTTP,
  result,
  selectionTags,
  sign,
  validName,
  verify,
} from "../src/try/wire.js";
import { createRecord, openStore, RecordState } from "../src/try/store.js";
import { step, Transport } from "../src/try/controller.js";
import { forward } from "../api/try.js";
const buyer = "01".repeat(32),
  sellerKey = "02".repeat(32);
const fixture = () =>
  JSON.parse(
    readFileSync(new URL("./fixtures/try-rust.json", import.meta.url), "utf8"),
  );
const resign = (e: Event, tags = e.tags, content = e.content, key = buyer) =>
  sign(key, e.kind, tags, content, e.created_at);
function env(e: Event, time = e.created_at, evidence = {}) {
  const eventBody = JSON.stringify(e);
  return JSON.stringify({
    eventBody,
    relayAuth: auth(buyer, eventBody, time),
    evidence,
  });
}
const accepted: typeof fetch = async (_url, init) =>
  Response.json({
    accepted: true,
    event_id: JSON.parse(init!.body as string).id,
  });
test("Rust golden events: signatures, exact builder tags, job/content hashes and canonical co-signature", () => {
  const f = fixture(),
    seller = f.claim.pubkey;
  for (const e of [f.offer, f.claim, f.award, f.result, f.accept]) verify(e);
  assert.deepEqual(
    offerTags(f.offer.tags[0][1], f.offer.created_at, seller),
    f.offer.tags,
  );
  assert.deepEqual(selectionTags(f.offer, f.claim), f.award.tags);
  assert.deepEqual(f.award.tags, f.accept.tags);
  outgoing(f.offer, {}, seller);
  outgoing(f.award, { offer: f.offer, claim: f.claim }, seller);
  outgoing(
    f.accept,
    { offer: f.offer, claim: f.claim, result: f.result },
    seller,
  );
  assert.equal(preimage(f.offer, f.result, seller), f.preimage);
  assert.equal(hash(f.preimage), f.digest);
  assert.equal(hash(f.result.content), f.contentHash);
  assert.equal(
    result(f.result, f.offer, f.claim, seller).answer,
    f.result.content,
  );
});
test("Unicode code points, byte bound, trimming once and internal whitespace", () => {
  assert.equal(promptText("  hi  🏎️\nthere  "), "hi  🏎️\nthere");
  assert.equal([...promptText("😀".repeat(1000))].length, 1000);
  for (const s of ["", "  ", "x".repeat(1001), "😀".repeat(1001), "\ud800"])
    assert.throws(() => promptText(s));
});
test("names use the specified lowercase word lists without suffix; kind-0 is restricted", () => {
  const spec = readFileSync(new URL("../../../docs/specs/try-it.md", import.meta.url), "utf8");
  for (const [label, words] of [["Model", models], ["Character", characters]] as const) {
    assert.ok(words.length >= 40);
    assert.equal(new Set(words).size, words.length);
    assert.ok(words.every(w => /^[a-z]+$/.test(w)));
    const line = spec.split("\n").find(line => line.startsWith(`- ${label} words:`))!;
    assert.deepEqual([...line.matchAll(/`([a-z]+)`/g)].map(m => m[1]), words);
  }
  for (let i = 0; i < 100; i++) {
    const x = identity();
    assert.ok(validName(x.name));
    outgoing(
      sign(
        x.secret,
        0,
        [],
        JSON.stringify({ name: x.name, display_name: x.name }),
      ),
    );
  }
  assert.throws(() =>
    outgoing(
      sign(
        buyer,
        0,
        [],
        JSON.stringify({ name: "worker-nemo", display_name: "worker-nemo" }),
      ),
    ),
  );
});
test("the displayed visitor identity becomes the reserved buyer profile", () => {
  const visitor = identity();
  const record = createRecord("Why slick tyres?", 1800000000, visitor);
  assert.equal(record.name, visitor.name);
  assert.equal(record.secret, visitor.secret);
  assert.deepEqual(JSON.parse(record.profile.content), { name: visitor.name, display_name: visitor.name });
  assert.equal(record.offer.pubkey, record.profile.pubkey);
});
test("reject malformed/duplicate critical tags and extra offer targets", () => {
  const f = fixture();
  for (const tags of [
    [...f.offer.tags, ["p", NEMO]],
    [...f.offer.tags, ["amount", "0", "sat"]],
    f.offer.tags.filter((t: string[]) => t[0] !== "i"),
  ])
    assert.throws(() => outgoing(resign(f.offer, tags), {}, f.claim.pubkey));
});
test("reject paid/missing-mode claims and foreign seller/root", () => {
  const f = fixture();
  for (const tags of [
    f.claim.tags.filter((t: string[]) => t[0] !== "payment"),
    [...f.claim.tags, ["creq", "creqA"]],
    f.claim.tags.map((t: string[]) =>
      t[0] === "e" ? ["e", "a".repeat(64), "", "root"] : t,
    ),
  ])
    assert.throws(() =>
      claim(resign(f.claim, tags, "", sellerKey), f.offer, f.claim.pubkey),
    );
  assert.throws(() =>
    claim(resign(f.claim, undefined, "", buyer), f.offer, f.claim.pubkey),
  );
});
test("altered answer or seller co-signature never validates; duplicate/git tags refused", () => {
  const f = fixture();
  for (const r of [
    resign(f.result, undefined, f.result.content + " ", sellerKey),
    resign(
      f.result,
      [...f.result.tags, ["repo", "https://example.com"]],
      undefined,
      sellerKey,
    ),
    resign(
      f.result,
      [...f.result.tags, ["sig", "seller", "0".repeat(128)]],
      undefined,
      sellerKey,
    ),
  ])
    assert.throws(() => result(r, f.offer, f.claim, f.claim.pubkey));
});
test("oversized/empty result and unrelated signed selection rejected", () => {
  const f = fixture();
  for (const content of ["", "x".repeat(16385)])
    assert.throws(() =>
      result(
        resign(f.result, undefined, content, sellerKey),
        f.offer,
        f.claim,
        f.claim.pubkey,
      ),
    );
  assert.throws(() =>
    outgoing(
      resign(
        f.accept,
        selectionTags(f.offer, { ...f.claim, id: "a".repeat(64) }),
      ),
      { offer: f.offer, claim: f.claim, result: f.result },
      f.claim.pubkey,
    ),
  );
});
test("atomic IndexedDB reservation wins once across connections and reload", async () => {
  const factory = new IDBFactory(),
    a = await openStore(factory),
    b = await openStore(factory);
  const values = await Promise.all([
    a.reserve(createRecord("one", 100)),
    b.reserve(createRecord("two", 100)),
  ]);
  assert.equal(values[0]!.offer.id, values[1]!.offer.id);
  a.close();
  const c = await openStore(factory);
  assert.equal((await c.read())!.offer.id, values[0]!.offer.id);
  b.close();
  c.close();
});
test("storage unavailable fails closed and validation does not consume a slot", async () => {
  await assert.rejects(
    openStore({
      open() {
        throw Error("disabled");
      },
    } as unknown as IDBFactory),
  );
  const db = await openStore(new IDBFactory());
  assert.throws(() => createRecord(" ", 100));
  assert.equal(await db.read(), undefined);
  db.close();
});
async function setup() {
  const f = fixture();
  const db = await openStore(new IDBFactory());
  const s: RecordState = {
    version: 1,
    secret: buyer,
    name: "stradale-nero",
    profile: sign(
      buyer,
      0,
      [],
      JSON.stringify({ name: "stradale-nero", display_name: "stradale-nero" }),
      f.offer.created_at,
    ),
    offer: f.offer,
    phase: "publishing",
  };
  await db.reserve(s);
  return { f, db, s };
}
function mock(events: Event[] = []) {
  const published: Event[] = [];
  const t: Transport = {
    async read(filter: any) {
      return filter.ids
        ? [...events, ...published].filter((e) => filter.ids.includes(e.id))
        : events;
    },
    async publish(e) {
      published.push(e);
    },
  };
  return { t, published };
}
test("ordinary four-write lifecycle; duplicates and out-of-order result; binding persisted before ACCEPT", async () => {
  const { f, db } = await setup(),
    { t, published } = mock([f.result, f.claim, f.result]);
  const publish = t.publish;
  t.publish = async (e, k, v) => {
    if (e.kind === 3406) {
      assert.equal((await db.read())!.binding!.resultId, f.result.id);
      assert.equal((await db.read())!.accept!.id, e.id);
    }
    await publish(e, k, v);
  };
  const s = await step(db, t, f.offer.created_at + 5, f.claim.pubkey);
  assert.equal(s!.phase, "done");
  assert.deepEqual(
    published.map((e) => e.kind),
    [0, 3401, 3405, 3406],
  );
  await step(db, t, f.offer.created_at + 6, f.claim.pubkey);
  assert.equal(published.length, 4);
  db.close();
});
test("lost offer acknowledgement reconciles exact ID on reload, never reserves a replacement", async () => {
  const { f, db } = await setup(),
    { t, published } = mock();
  const p = t.publish;
  t.publish = async (e, k, v) => {
    await p(e, k, v);
    if (e.kind === 3401) throw Error("lost ack");
  };
  await assert.rejects(step(db, t, f.offer.created_at + 5, f.claim.pubkey));
  assert.equal((await db.read())!.offerAck, undefined);
  await step(db, t, f.offer.created_at + 6, f.claim.pubkey);
  assert.equal(published.filter((e) => e.kind === 3401).length, 1);
  assert.equal(
    (await db.reserve(createRecord("replacement", 100)))!.offer.id,
    f.offer.id,
  );
  db.close();
});
test("conflicting claims stop without award; expired claims never awarded", async () => {
  const { f, db } = await setup(),
    m = mock([
      f.claim,
      sign(sellerKey, 3402, f.claim.tags, "", f.claim.created_at + 1),
    ]);
  assert.equal(
    (await step(db, m.t, f.offer.created_at + 5, f.claim.pubkey))!.phase,
    "conflict",
  );
  assert.ok(!m.published.some((e) => e.kind === 3405));
  db.close();
  const x = await setup(),
    n = mock([f.claim]);
  assert.equal(
    (await step(x.db, n.t, f.offer.created_at + 331, f.claim.pubkey))!.phase,
    "timeout",
  );
  assert.ok(!n.published.some((e) => e.kind === 3405));
  x.db.close();
});
test("late valid result completes previously awarded job after local timeout", async () => {
  const { f, db } = await setup();
  await db.update((s) => ({
    ...s,
    profileAck: true,
    offerAck: true,
    claim: f.claim,
    award: f.award,
    awardAck: true,
    phase: "timeout",
  }));
  const m = mock([f.result]);
  assert.equal(
    (await step(db, m.t, f.offer.created_at + 500, f.claim.pubkey))!.phase,
    "done",
  );
  db.close();
});
test("invalid result does not render a trusted answer or publish ACCEPT", async () => {
  const { f, db } = await setup(),
    m = mock([f.claim, resign(f.result, undefined, "altered", sellerKey)]);
  const s = await step(db, m.t, f.offer.created_at + 5, f.claim.pubkey);
  assert.equal(s!.phase, "invalid");
  assert.equal(s!.binding, undefined);
  assert.ok(!m.published.some((e) => e.kind === 3406));
  db.close();
});
test("ACCEPT pending retains verified answer and exact signed accept", async () => {
  const { f, db } = await setup(),
    m = mock([f.claim, f.result]);
  const p = m.t.publish;
  m.t.publish = async (e, k, v) => {
    if (e.kind === 3406) throw Error("offline");
    await p(e, k, v);
  };
  await assert.rejects(step(db, m.t, f.offer.created_at + 5, f.claim.pubkey));
  const s = await db.read();
  assert.equal(s!.phase, "accept-pending");
  assert.equal(s!.binding!.answer, f.result.content);
  assert.ok(s!.accept);
  db.close();
});
test("API off is 404, request capped at 64 KiB, unrelated kind rejected", async () => {
  assert.equal((await forward("{}", false)).status, 404);
  assert.equal((await forward("x".repeat(65537), true)).status, 413);
  assert.equal(
    (await forward(env(sign(buyer, 1, [])), true, accepted)).status,
    400,
  );
});
test("NIP-98 binds exact body, URL and same signing identity", async () => {
  const e = sign(buyer, 3401, offerTags("hello", 1800000000), "", 1800000000);
  for (const change of [
    (x: any) => (x.eventBody += " "),
    (x: any) =>
      (x.relayAuth = auth(
        buyer,
        x.eventBody,
        e.created_at,
        "https://evil.test/events",
      )),
    (x: any) => (x.relayAuth = auth(sellerKey, x.eventBody, e.created_at)),
  ]) {
    const x = JSON.parse(env(e));
    change(x);
    assert.equal(
      (await forward(JSON.stringify(x), true, accepted, e.created_at)).status,
      400,
    );
  }
});
test("same event retry gets fresh auth; original bytes and only fixed destination forwarded", async () => {
  const e = sign(buyer, 3401, offerTags("hello", 1800000000), "", 1800000000),
    a = env(e),
    b = env(e);
  assert.notEqual(JSON.parse(a).relayAuth.id, JSON.parse(b).relayAuth.id);
  let calls = 0;
  const fetcher: typeof fetch = async (url, init) => {
    calls++;
    assert.equal(url, RELAY_HTTP);
    assert.equal(init!.body, JSON.parse(a).eventBody);
    assert.equal(init!.redirect, "error");
    assert.ok((init!.headers as any).Authorization.startsWith("Nostr "));
    return Response.json({ accepted: true, event_id: e.id });
  };
  assert.equal((await forward(a, true, fetcher, e.created_at)).status, 200);
  assert.equal((await forward(b, true, fetcher, e.created_at)).status, 200);
  assert.equal(calls, 2);
});
test("API validates Rust evidence for four writes and rejects mismatched relations", async () => {
  const f = fixture(),
    profile = sign(
      buyer,
      0,
      [],
      JSON.stringify({ name: "stradale-nero", display_name: "stradale-nero" }),
      f.offer.created_at,
    );
  for (const e of [profile, f.offer, f.award, f.accept])
    assert.equal(
      (
        await forward(
          env(e, e.created_at, {
            offer: f.offer,
            claim: f.claim,
            result: f.result,
          }),
          true,
          accepted,
          e.created_at,
          f.claim.pubkey,
        )
      ).status,
      200,
    );
  assert.equal(
    (
      await forward(
        env(f.accept, f.accept.created_at, {
          offer: f.offer,
          claim: { ...f.claim, id: "a".repeat(64) },
          result: f.result,
        }),
        true,
        accepted,
        f.accept.created_at,
        f.claim.pubkey,
      )
    ).status,
    400,
  );
});
test("HTTP 200 accepted=false or wrong ID, outage, firewall 429 and stale forwarding", async () => {
  const e = sign(buyer, 3401, offerTags("hi", 1800000000), "", 1800000000);
  for (const receipt of [
    { accepted: false, event_id: e.id },
    { accepted: true, event_id: "wrong" },
  ])
    assert.equal(
      (
        await forward(
          env(e),
          true,
          async () => Response.json(receipt),
          e.created_at,
        )
      ).status,
      502,
    );
  assert.equal(
    (
      await forward(
        env(e),
        true,
        async () => {
          throw new TypeError("network");
        },
        e.created_at,
      )
    ).status,
    502,
  );
  const limited = await forward(
    env(e),
    true,
    async () =>
      new Response(null, { status: 429, headers: { "Retry-After": "120" } }),
    e.created_at,
  );
  assert.equal(limited.status, 429);
  assert.equal(limited.headers.get("Retry-After"), "120");
  assert.equal(
    (
      await forward(
        env(e, e.created_at + 100),
        true,
        accepted,
        e.created_at + 100,
      )
    ).status,
    409,
  );
});

test("ordinary seller capability and execution metadata are accepted without trusting their claims", () => {
  const f = fixture();
  const c = resign(
    f.claim,
    [
      ...f.claim.tags,
      ["agents", "codex"],
      ["harness_family", "codex"],
      ["capabilities", "tool_use"],
      ["harness_model", "codex", "example"],
    ],
    "",
    sellerKey,
  );
  claim(c, f.offer, f.claim.pubkey);
  const r = resign(
    f.result,
    [
      ...f.result.tags,
      ["harness", "codex-acp"],
      ["usage_transport", "acp"],
      ["metadata_trust", "seller-claimed"],
      ["wall_time", "42", "ms"],
      ["tokens", "10", "total"],
    ],
    f.result.content,
    sellerKey,
  );
  assert.equal(result(r, f.offer, c, f.claim.pubkey).answer, f.result.content);
});
test("expired unacknowledged offer is reconciled without any new writes", async () => {
  const { f, db } = await setup(),
    m = mock();
  const s = await step(db, m.t, f.offer.created_at + 331, f.claim.pubkey);
  assert.equal(s!.phase, "timeout");
  assert.equal(m.published.length, 0);
  assert.equal(s!.offer.id, f.offer.id);
  db.close();
});
test("forged and unrelated delivery events are ignored, not rendered or accepted", async () => {
  const { f, db } = await setup();
  const unrelated = resign(
    f.result,
    f.result.tags.map((t: string[]) =>
      t[0] === "e" ? ["e", "a".repeat(64), "", "root"] : t,
    ),
    f.result.content,
    sellerKey,
  );
  const m = mock([f.claim, { ...f.result, sig: "0".repeat(128) }, unrelated]);
  const s = await step(db, m.t, f.offer.created_at + 5, f.claim.pubkey);
  assert.equal(s!.phase, "working");
  assert.equal(s!.binding, undefined);
  db.close();
});
test("refusal feedback requires the matching seller and thread; delayed state preserves prompt", async () => {
  const { f, db } = await setup();
  await db.update((s) => ({ ...s, profileAck: true, offerAck: true }));
  const feedback = sign(
    sellerKey,
    3404,
    [
      ["status", "error"],
      ["e", f.offer.id, "", "root"],
      ["p", f.offer.pubkey],
      ...namespace,
    ],
    "",
    f.offer.created_at + 10,
  );
  assert.equal(
    (await step(
      db,
      mock([feedback]).t,
      f.offer.created_at + 20,
      f.claim.pubkey,
    ))!.phase,
    "refused",
  );
  const x = await setup();
  assert.equal(
    (await step(x.db, mock().t, f.offer.created_at + 31, f.claim.pubkey))!
      .phase,
    "delayed",
  );
  assert.equal((await x.db.read())!.offer.id, f.offer.id);
  db.close();
  x.db.close();
});
test("API malformed evidence is 400, transport timeout is 502, and replay admission stays with the relay", async () => {
  const f = fixture();
  assert.equal(
    (
      await forward(
        env(f.accept),
        true,
        accepted,
        f.accept.created_at,
        f.claim.pubkey,
      )
    ).status,
    400,
  );
  const e = sign(buyer, 3401, offerTags("hi", 1800000000), "", 1800000000);
  assert.equal(
    (
      await forward(
        env(e),
        true,
        async () => {
          throw new DOMException("timeout", "TimeoutError");
        },
        e.created_at,
      )
    ).status,
    502,
  );
  const raw = env(e),
    seen = new Set<string>();
  const bridge: typeof fetch = async (_u, i) => {
    const id = JSON.parse(
      Buffer.from(
        (i!.headers as any).Authorization.slice(6),
        "base64",
      ).toString(),
    ).id;
    if (seen.has(id)) return new Response(null, { status: 401 });
    seen.add(id);
    return Response.json({ accepted: true, event_id: e.id });
  };
  assert.equal((await forward(raw, true, bridge, e.created_at)).status, 200);
  assert.equal((await forward(raw, true, bridge, e.created_at)).status, 502);
  assert.equal((await forward(env(e), true, bridge, e.created_at)).status, 200);
});
test("Vercel entry point requires both enable flags, restricts method and bounds streamed bodies", async () => {
  const handler = (await import("../api/try.js")).default;
  const a = process.env.TRY_IT_ENABLED,
    b = process.env.TRY_IT_API_ENABLED;
  try {
    for (const flags of [
      ["false", "false"],
      ["true", "false"],
      ["false", "true"],
    ]) {
      process.env.TRY_IT_ENABLED = flags[0];
      process.env.TRY_IT_API_ENABLED = flags[1];
      assert.equal(
        (await handler.fetch(new Request("http://localhost/api/try"))).status,
        404,
      );
    }
    process.env.TRY_IT_ENABLED = "true";
    process.env.TRY_IT_API_ENABLED = "true";
    assert.equal(
      (await handler.fetch(new Request("http://localhost/api/try"))).status,
      405,
    );
    assert.equal(
      (
        await handler.fetch(
          new Request("http://localhost/api/try", {
            method: "POST",
            body: "x".repeat(65537),
          }),
        )
      ).status,
      413,
    );
  } finally {
    if (a === undefined) delete process.env.TRY_IT_ENABLED;
    else process.env.TRY_IT_ENABLED = a;
    if (b === undefined) delete process.env.TRY_IT_API_ENABLED;
    else process.env.TRY_IT_API_ENABLED = b;
  }
});

test("API refuses an award forwarded after deadline even with a fresh valid auth and recent event", async () => {
  const f = fixture(),
    e = sign(
      buyer,
      3405,
      selectionTags(f.offer, f.claim),
      "",
      f.offer.created_at + 299,
    );
  assert.equal(
    (
      await forward(
        env(e, e.created_at + 2, { offer: f.offer, claim: f.claim }),
        true,
        accepted,
        e.created_at + 2,
        f.claim.pubkey,
      )
    ).status,
    409,
  );
  const early = sign(buyer, 3406, f.accept.tags, "", f.claim.created_at);
  assert.throws(() =>
    outgoing(
      early,
      { offer: f.offer, claim: f.claim, result: f.result },
      f.claim.pubkey,
    ),
  );
});

test("Vercel retry delay handles delta seconds, HTTP dates and malformed headers with bounds", async () => {
  const { retryDelay } = await import("../src/try/transport.js");
  assert.equal(retryDelay("120", 0), 120000);
  assert.equal(retryDelay("Thu, 01 Jan 1970 00:02:00 GMT", 0), 120000);
  assert.equal(retryDelay(null, 0), 60000);
  assert.equal(retryDelay("invalid", 0), 60000);
  assert.equal(retryDelay("0", 0), 1000);
  assert.equal(retryDelay("9999999999", 0), 86400000);
});
