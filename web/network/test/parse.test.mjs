import assert from "node:assert/strict";
import { createStore } from "../js/store.js";
import { verifyAdvertisedHarnesses } from "../js/profiles.js";
import {
  extractUsageAdjunct,
  parseEvent,
  parseProfile,
  percentile,
  PROFILE_CONTENT_MAX,
} from "../js/parse.js";
import {
  CLAIM,
  HANDLER,
  HEARTBEAT,
  OFFER,
  RECEIPT,
  RESULT,
  SELLER_HEARTBEAT_D,
} from "../js/kinds.js";
import { test } from "node:test";

function ok(ev) {
  const n = parseEvent(ev);
  assert.ok(n, "expected parse success");
  return n;
}

// Declared here rather than in the section that assigns them: a later section reads each
// one. Every other binding stays inside the test that owns it.
let store;
let offerId;
let funnel;
let adjunct;
let tRow;
let uRow;

test("defensive parse: hostile / malformed must not throw or blank store", () => {
  const garbage = [
    null,
    undefined,
    "",
    42,
    [],
    {},
    { id: "x" },
    { id: "a".repeat(64), pubkey: "b".repeat(64), kind: "nope", created_at: 1 },
    {
      id: "c".repeat(64),
      pubkey: "d".repeat(64),
      kind: OFFER,
      created_at: 1,
      tags: "not-array",
      content: null,
    },
    {
      id: "e".repeat(64),
      pubkey: "f".repeat(64),
      kind: RECEIPT,
      created_at: 1,
      tags: [["amount", "3", "sat"], ["e", "offer1", "", "root"]],
      content: "{not json",
    },
    {
      id: "g".repeat(64),
      pubkey: "h".repeat(64),
      kind: RECEIPT,
      created_at: 1,
      tags: [null, ["amount", 12], ["e", "offer1"]],
      content: JSON.stringify({
        usage_measure: { total_tokens: "NaN-ish" },
        measured_cost_tokens: { nested: true },
      }),
    },
  ];

  for (const g of garbage) {
    assert.doesNotThrow(() => parseEvent(g));
  }

  store = createStore();
  for (const g of garbage) {
    assert.doesNotThrow(() => store.ingest(parseEvent(g)));
  }

  // one good offer after garbage — funnel still renders numbers
  offerId = "1".repeat(64);
  store.ingest(
    ok({
      id: offerId,
      pubkey: "2".repeat(64),
      kind: OFFER,
      created_at: 100,
      tags: [
        ["i", "task"],
        ["amount", "21", "sat"],
        ["t", "maxplayer"],
        ["v", "2"],
      ],
      content: "",
    }),
  );

  funnel = store.funnel();
  assert.equal(funnel.offers, 1);
  assert.equal(funnel.leaks.unclaimed, 1);
  assert.ok(funnel.parseSkips >= 1, "malformed events counted as skips");
  assert.doesNotThrow(() => store.snapshot());
});

test("usage adjunct vocabulary (Scribe lock)", () => {
  adjunct = extractUsageAdjunct(
    {
      usage_measure: {
        total_tokens: 13693,
        input_tokens: 13346,
        output_tokens: 347,
        cache_read_tokens: 41088,
      },
      measured_cost_tokens: null,
      paid_price_tokens: 20000,
      usage_transport: "side-channel",
      harness_family: "cursor",
    },
    [
      ["amount", "21", "sat"],
      ["e", offerId, "", "root"],
    ],
  );

  assert.equal(adjunct.total_tokens, 13693);
  assert.equal(adjunct.measured_cost_tokens, null);
  assert.equal(adjunct.paid_price_tokens, 20000);
  assert.equal(adjunct.usage_transport, "side-channel");
  assert.equal(adjunct.harness_family, "cursor");
  assert.equal(adjunct.paid_price_sats, 21);
  // cache must NOT be folded into total by the parser
  assert.notEqual(adjunct.total_tokens, 13346 + 347 + 41088);

  // old receipt without adjunct fields — still parses
  const oldReceipt = ok({
    id: "3".repeat(64),
    pubkey: "4".repeat(64),
    kind: RECEIPT,
    created_at: 200,
    tags: [
      ["amount", "7", "sat"],
      ["e", offerId, "", "root"],
      ["e", "9".repeat(64), "", "reply"],
      ["mint", "https://testnut.cashu.space"],
    ],
    content: "",
  });
  assert.equal(oldReceipt.receipt.usage.total_tokens, null);
  assert.equal(oldReceipt.receipt.usage.measured_cost_tokens, null);
  assert.equal(oldReceipt.receipt.amount_sats, 7);

  store.ingest(oldReceipt);
  const eco = store.economics();
  assert.ok(eco.rows.length >= 1);
  assert.equal(eco.rows[0].measured_cost_tokens, null);
});

test("kind-31990 handler announces still PARSE (they show in the live tail)", () => {
  // Nothing derives a seat from them any more; the seat census reads kind-30340 below.

  const handler = ok({
    id: "5".repeat(64),
    pubkey: "6".repeat(64),
    kind: HANDLER,
    created_at: 300,
    tags: [
      ["d", "seller-a"],
      ["k", "5109"],
    ],
    content: JSON.stringify({
      harness_name: "cursor-agent",
      version: "2026.07.09",
      name: "fallback-name",
    }),
  });
  assert.equal(handler.handler.harness_name, "cursor-agent");
  assert.equal(handler.handler.version, "2026.07.09");
});

test("the seat census: resolved at the ADDRESS, bounded by freshness", () => {
  //
  // ⚠ THE COUNTS IN THIS FIXTURE ARE DELIBERATELY EQUAL. The retired kind-31990 source yields 3 rows
  // and the correct kind-30340 source yields 3 seats, and they are DIFFERENT SETS sharing one member.
  // This is a miniature of the live measurement on 2026-08-21, where the retired source rendered 21
  // rows against 21 distinct seat addresses on the wire — two totals that agree while the populations
  // differ by 13 residue rows plus one live seat missing entirely.
  //
  // So: DO NOT reduce this acceptance to a length comparison. A count that matches for the wrong
  // reason is the strongest form of a lying instrument, because it survives inspection. The check
  // that separates these populations is a SET JOIN and nothing else can be substituted for it.
  {
    const NOW = 10_000;
    const pk = (n) => String(n).repeat(64).slice(0, 64);
    const A = pk(1); // fresh, current address, ALSO has a 31990 → in both populations
    const B = pk(2); // fresh, current address, NO 31990 → correct only (the live seat the old source could not see)
    const E = pk(3); // fresh, current address, NO 31990 → correct only
    const C = pk(4); // STALE, current address, has a 31990 → retired source only (a fossil)
    const D = pk(5); // fresh but at the RETIRED address, has a 31990 → retired source only

    const seatStore = createStore();
    const beat = (pubkey, d, created_at, agents) =>
      ok({
        id: pubkey.slice(0, 2).repeat(32),
        pubkey,
        kind: HEARTBEAT,
        created_at,
        tags: [
          ["d", d],
          ["t", "maxplayer"],
          ["v", "1"],
          ["rate", "100"],
          ["accepting", "y"],
          ["queue_depth", "0"],
          ...(agents ? [["agents", ...agents]] : []),
        ],
        content: "",
      });
    seatStore.ingest(beat(A, SELLER_HEARTBEAT_D, NOW - 10, ["claude"]));
    seatStore.ingest(beat(B, SELLER_HEARTBEAT_D, NOW - 20, ["codex", "cursor"]));
    seatStore.ingest(beat(E, SELLER_HEARTBEAT_D, NOW - 30, null));
    seatStore.ingest(beat(C, SELLER_HEARTBEAT_D, NOW - 9000, ["claude"]));
    seatStore.ingest(beat(D, "mobee-seller", NOW - 15, ["claude"]));

    const census = seatStore.census(NOW);
    const correct = new Set(census.seats.map((s) => s.pubkey));
    const retired = new Set([A, C, D]); // what a kind-31990 index of this fixture would have held

    // The set join FIRST — the only check with access to the property we want. Order matters here:
    // an assertion that fails early silences every assertion below it, so putting the count check
    // first made a broken resolver report "the totals disagree" while these never ran at all. The
    // informative failure has to be the one that fires.
    assert.deepEqual([...correct].sort(), [A, B, E].sort());
    assert.ok(correct.has(B), "a live seat with no retired-kind announce must still appear");
    assert.ok(correct.has(E), "…and so must the second one");
    assert.ok(!correct.has(C), "a stale address must not render as a current seat");
    assert.ok(!correct.has(D), "an announcement at the retired d is not this seat address");
    assert.equal([...correct].filter((p) => retired.has(p)).length, 1, "the sets share ONE member");

    // The coincidence LAST, asserted as a fact of the fixture so it cannot be read as incidental:
    // these two totals agree and the populations above still differ. That is the whole point.
    assert.equal(correct.size, retired.size, "fixture invariant: the two totals agree");

    // The denominator ships with the number: 4 addresses at this d, 1 cut by the window.
    assert.equal(census.addressesSeen, 4);
    assert.equal(census.fossilsExcluded, 1);
    // Pinned as a LITERAL, not read from `SEAT_FRESH_WINDOW_S`. Asserting a constant against itself
    // passes for any value and would let this window change silently; the window is derived from the
    // measured beat cadence, so changing it must be a deliberate edit in two places.
    assert.equal(census.freshWindowS, 900);

    // Many seats share d="maxplayer-seller", so the address must include the author. Carried over
    // from the kind-31990 census, where the same collapse would have been the bug.
    assert.equal(correct.size, 3, "same d from distinct pubkeys must not collapse");

    // The advertised roster is a CLAIM, carried verbatim; an unstated roster is empty, never invented.
    const byPk = new Map(census.seats.map((s) => [s.pubkey, s]));
    assert.deepEqual(byPk.get(B).agents, ["codex", "cursor"]);
    assert.deepEqual(byPk.get(E).agents, [], "a seat advertising no harness advertises none");
    assert.equal(byPk.get(A).version, "1");

    // Newest first, so the panel's first row is the most recent announcement.
    assert.deepEqual(
      census.seats.map((s) => s.pubkey),
      [A, B, E],
    );
  }
});

test("latency path", () => {
  store.ingest(
    ok({
      id: "7".repeat(64),
      pubkey: "8".repeat(64),
      kind: CLAIM,
      created_at: 130,
      tags: [
        ["status", "processing"],
        ["e", offerId],
        ["t", "maxplayer"],
        ["v", "2"],
      ],
      content: "",
    }),
  );
  store.ingest(
    ok({
      id: "a".repeat(64),
      pubkey: "b".repeat(64),
      kind: RESULT,
      created_at: 180,
      tags: [
        ["e", offerId, "", "root"],
        ["amount", "21", "sat"],
        ["t", "maxplayer"],
        ["v", "2"],
      ],
      content: "done",
    }),
  );
  const lat = store.latency();
  assert.equal(lat.toClaim.n, 1);
  assert.equal(lat.toClaim.p50, 30);
  assert.equal(lat.toResult.p50, 50);

  assert.equal(percentile([1, 2, 3, 4], 50), 2.5);
  assert.equal(percentile([], 50), null);
});

test("kind-0 profiles + newest-first tail + id dedupe", () => {
  const goodProfile = ok({
    id: "c0".padEnd(64, "0"),
    pubkey: "aa".repeat(32),
    kind: 0,
    created_at: 400,
    tags: [],
    content: JSON.stringify({
      name: "ok-name",
      display_name: "Ok Display",
      picture: "https://example.com/a.png",
      about: "hello",
    }),
  });
  assert.equal(goodProfile.role, "profile");
  assert.equal(goodProfile.profile.name, "ok-name");
  assert.equal(goodProfile.profile.display_name, "Ok Display");
  assert.equal(goodProfile.profile.picture, "https://example.com/a.png");

  // Hostile 2MB content must not throw / blank.
  assert.doesNotThrow(() =>
    parseProfile({
      content: "Z".repeat(2_000_000),
      created_at: 1,
    }),
  );
  assert.equal(
    parseProfile({
      content: JSON.stringify({ picture: "javascript:alert(1)" }),
      created_at: 1,
    }).picture,
    null,
  );
  // Oversized JSON: truncated then fail-closed to empty fields (page stays up).
  const oversized = parseProfile({
    content: JSON.stringify({
      name: "will-truncate",
      junk: "Z".repeat(PROFILE_CONTENT_MAX),
    }),
    created_at: 1,
  });
  assert.equal(oversized.name, null);

  const v12 = createStore();
  const older = ok({
    id: "d1".padEnd(64, "1"),
    pubkey: "aa".repeat(32),
    kind: OFFER,
    created_at: 10,
    tags: [
      ["i", "task"],
      ["amount", "1", "sat"],
      ["t", "maxplayer"],
      ["v", "2"],
    ],
    content: "",
  });
  const newer = ok({
    id: "d2".padEnd(64, "2"),
    pubkey: "bb".repeat(32),
    kind: OFFER,
    created_at: 20,
    tags: [
      ["i", "task"],
      ["amount", "1", "sat"],
      ["t", "maxplayer"],
      ["v", "2"],
    ],
    content: "",
  });
  // Deliver out of order — tail must still be newest-first.
  assert.equal(v12.ingest(newer).ingested, true);
  assert.equal(v12.ingest(older).ingested, true);
  assert.equal(v12.ingest(newer).ingested, false, "id dedupe");
  const tail = v12.tail();
  assert.equal(tail[0].id, newer.id);
  assert.equal(tail[1].id, older.id);
  assert.equal(tail[0].profile, null);

  const profileIn = v12.ingest(goodProfile);
  assert.equal(profileIn.ingested, true);
  assert.equal(profileIn.newAuthor, null);
  assert.equal(v12.getProfile("aa".repeat(32))?.display_name, "Ok Display");
  assert.equal(v12.tail().length, 2, "profiles stay out of live tail");
  assert.equal(v12.funnel().profiles, 1);
  assert.equal(v12.tail()[1].profile?.name, "ok-name");
});

test("usage adjunct reads from result tags (spec wins)", () => {
  // (1) OLD / untagged 6109 result (content is a non-JSON delivery string) → every usage field
  // dashes. Absent-stays-absent applies to legacy rows too: NO fabricated zeros/backfill.
  const untaggedResult = ok({
    id: "e1".padEnd(64, "0"),
    pubkey: "f1".padEnd(64, "0"),
    kind: RESULT,
    created_at: 500,
    tags: [
      ["e", offerId, "", "root"],
      ["amount", "21", "sat"],
      ["t", "maxplayer"],
      ["v", "2"],
    ],
    content: "delivery commit abcdef0123",
  });
  {
    const u = untaggedResult.result.usage;
    assert.equal(u.total_tokens, null);
    assert.equal(u.input_tokens, null);
    assert.equal(u.output_tokens, null);
    assert.equal(u.reasoning_tokens, null);
    assert.equal(u.model, null);
    assert.equal(u.cost_usd, null);
    assert.equal(u.cost_basis, null);
    assert.equal(u.usage_transport, null);
    assert.equal(u.harness_family, null);
    // the amount tag is still read (it is not usage-adjunct data)
    assert.equal(u.paid_price_sats, 21);
  }

  // (2) NEW tagged 6109 result → fills per the result schema; harness mapped to the spec enum.
  const taggedResult = ok({
    id: "e2".padEnd(64, "0"),
    pubkey: "f2".padEnd(64, "0"),
    kind: RESULT,
    created_at: 510,
    tags: [
      ["e", offerId, "", "root"],
      ["amount", "21", "sat"],
      ["harness", "claude-agent-acp"],
      ["usage_transport", "acp-native"],
      ["metadata_trust", "seller-claimed"],
      ["model", "claude-opus-4-8"],
      ["tokens", "140", "total"],
      ["tokens", "100", "input"],
      ["tokens", "40", "output"],
      ["tokens", "4096", "cache_read"],
      ["cost", "0.0123", "usd", "harness-reported-usd"],
      ["wall_time", "4321", "ms"],
      ["t", "maxplayer"],
      ["v", "2"],
    ],
    content: "delivery commit abcdef0123",
  });
  {
    const u = taggedResult.result.usage;
    assert.equal(u.total_tokens, 140);
    assert.equal(u.input_tokens, 100);
    assert.equal(u.output_tokens, 40);
    assert.equal(u.cache_read_tokens, 4096);
    assert.equal(u.reasoning_tokens, null); // absent = unknown, NOT zero
    assert.equal(u.model, "claude-opus-4-8");
    assert.equal(u.cost_usd, 0.0123);
    assert.equal(u.cost_basis, "harness-reported-usd");
    assert.equal(u.usage_transport, "acp-native");
    assert.equal(u.harness_family, "claude"); // claude-agent-acp → claude
    assert.equal(u.paid_price_sats, 21);
    // cache siblings must NOT be folded into total by the reader
    assert.notEqual(u.total_tokens, 100 + 40 + 4096);
  }

  // harness_family mapping across the spec enum; unreadable → null (see the reading-versus-claim
  // section at the end of this file for why it is not "other"); absent → null.
  assert.equal(
    extractUsageAdjunct(null, [["harness", "cursor-agent"]]).harness_family,
    "cursor",
  );
  assert.equal(
    extractUsageAdjunct(null, [["harness", "codex-acp-ng"]]).harness_family,
    "codex",
  );
  assert.equal(
    extractUsageAdjunct(null, [["harness", "some-tool"]]).harness_family,
    null,
  );
  assert.equal(
    extractUsageAdjunct(null, [["harness", "some-tool"]]).harness_id,
    "some-tool",
  );
  assert.equal(extractUsageAdjunct(null, []).harness_family, null);

  // Dashboard END-TO-END: a tagged result fills its economics row; an untagged one dashes.
  const eco2 = createStore();
  eco2.ingest(taggedResult);
  eco2.ingest(untaggedResult);
  const e2rows = eco2.economics().rows;
  tRow = e2rows.find((r) => r.id === taggedResult.id);
  uRow = e2rows.find((r) => r.id === untaggedResult.id);
  assert.ok(tRow, "tagged 6109 result fills an economics row");
  assert.equal(tRow.total_tokens, 140);
  assert.equal(tRow.harness_family, "claude");
  assert.equal(tRow.usage_transport, "acp-native");
  // input / output columns fill from the ["tokens",N,"input"|"output"] tags.
  assert.equal(tRow.input_tokens, 100, "input column fills from the input tag");
  assert.equal(tRow.output_tokens, 40, "output column fills from the output tag");
  assert.ok(uRow, "untagged 6109 result still rows out");
  assert.equal(uRow.total_tokens, null, "untagged usage stays dashed — never fabricated");
  assert.equal(uRow.harness_family, null);
  // absent input/output → dash (null), NEVER a fabricated 0.
  assert.equal(uRow.input_tokens, null, "absent input → dash, never a fabricated 0");
  assert.equal(uRow.output_tokens, null, "absent output → dash, never a fabricated 0");
});

test("row SOURCE: \"delivered\" (6109-only) must never read as \"paid\" (3400-backed)", () => {
  // result-only rows are DELIVERED, not paid.
  assert.equal(tRow.source, "delivered", "6109-result-only row is delivered, not paid");
  assert.equal(uRow.source, "delivered");

  // a kind-3400 receipt-backed row is PAID.
  const paidStore = createStore();
  const paidOffer = "b1".padEnd(64, "0");
  const paidReceipt = ok({
    id: "b2".padEnd(64, "0"),
    pubkey: "b3".padEnd(64, "0"),
    kind: RECEIPT,
    created_at: 600,
    tags: [
      ["amount", "9", "sat"],
      ["e", paidOffer, "", "root"],
      ["e", "b9".padEnd(64, "0"), "", "reply"],
      ["mint", "https://testnut.cashu.space"],
    ],
    content: "",
  });
  paidStore.ingest(paidReceipt);
  const paidRow = paidStore.economics().rows.find((r) => r.id === paidReceipt.id);
  assert.ok(paidRow, "receipt produces an economics row");
  assert.equal(paidRow.source, "paid", "kind-3400 receipt-backed row is paid");

  // dedup: a job with BOTH a result and a receipt → the receipt wins → PAID (no duplicate row).
  const bothStore = createStore();
  bothStore.ingest(
    ok({
      id: "c1".padEnd(64, "0"),
      pubkey: "c2".padEnd(64, "0"),
      kind: RESULT,
      created_at: 700,
      tags: [
        ["e", paidOffer, "", "root"],
        ["amount", "9", "sat"],
        ["harness", "claude-agent-acp"],
        ["usage_transport", "acp-native"],
        ["tokens", "5", "total"],
        ["tokens", "3", "input"],
        ["tokens", "2", "output"],
      ],
      content: "delivery commit c0ffee",
    }),
  );
  bothStore.ingest(paidReceipt); // same offer (paidOffer) → receipt wins STATUS, result fills USAGE
  const bothRows = bothStore.economics().rows.filter((r) => r.source);
  assert.equal(
    bothRows.filter((r) => r.source === "delivered").length,
    0,
    "result echo is suppressed once a receipt exists for the offer",
  );
  assert.equal(
    bothRows.filter((r) => r.source === "paid").length,
    1,
    "the settled job shows exactly one paid row",
  );
  // JOIN: the single paid row carries the RESULT's usage (not receipt dashes).
  const bothPaid = bothRows.find((r) => r.source === "paid");
  assert.equal(bothPaid.total_tokens, 5, "paid row JOINS the result's tokens (offer fallback)");
  assert.equal(bothPaid.input_tokens, 3);
  assert.equal(bothPaid.output_tokens, 2);
  assert.equal(bothPaid.harness_family, "claude");
  assert.equal(bothPaid.usage_transport, "acp-native");
});

test("JOIN via the receipt's exact reply-tag binding → PAID row shows the result's usage", () => {
  const joinStore = createStore();
  const jOffer = "d0".padEnd(64, "0");
  const jResultId = "d1".padEnd(64, "0");
  joinStore.ingest(
    ok({
      id: jResultId,
      pubkey: "d2".padEnd(64, "0"),
      kind: RESULT,
      created_at: 800,
      tags: [
        ["e", jOffer, "", "root"],
        ["amount", "9", "sat"],
        ["harness", "claude-agent-acp"],
        ["usage_transport", "acp-native"],
        ["tokens", "140", "total"],
        ["tokens", "100", "input"],
        ["tokens", "40", "output"],
      ],
      content: "delivery commit d0ffee",
    }),
  );
  const jReceipt = ok({
    id: "d3".padEnd(64, "0"),
    pubkey: "d4".padEnd(64, "0"),
    kind: RECEIPT,
    created_at: 810,
    tags: [
      ["amount", "9", "sat"],
      ["e", jOffer, "", "root"],
      ["e", jResultId, "", "reply"], // binds THIS result
      ["mint", "https://testnut.cashu.space"],
    ],
    content: "",
  });
  joinStore.ingest(jReceipt);
  const jRows = joinStore.economics().rows;
  assert.equal(
    jRows.filter((r) => r.source === "delivered").length,
    0,
    "no duplicate delivered row for a paid job",
  );
  const jPaid = jRows.find((r) => r.id === jReceipt.id);
  assert.ok(jPaid, "receipt row present");
  assert.equal(jPaid.source, "paid");
  assert.equal(jPaid.total_tokens, 140, "PAID row shows the bound RESULT's tokens, not dashes");
  assert.equal(jPaid.input_tokens, 100);
  assert.equal(jPaid.output_tokens, 40);
  assert.equal(jPaid.harness_family, "claude");
  assert.equal(jPaid.usage_transport, "acp-native");

  // receipt with NO visible result → PAID with usage dashes (honest, never fabricated).
  const orphanStore = createStore();
  const orphanReceipt = ok({
    id: "e9".padEnd(64, "0"),
    pubkey: "ea".padEnd(64, "0"),
    kind: RECEIPT,
    created_at: 820,
    tags: [
      ["amount", "9", "sat"],
      ["e", "eb".padEnd(64, "0"), "", "root"],
      ["e", "ec".padEnd(64, "0"), "", "reply"],
      ["mint", "https://testnut.cashu.space"],
    ],
    content: "",
  });
  orphanStore.ingest(orphanReceipt);
  const orphanRow = orphanStore.economics().rows.find((r) => r.id === orphanReceipt.id);
  assert.equal(orphanRow.source, "paid");
  assert.equal(orphanRow.total_tokens, null, "receipt with no result → usage dashes, not fabricated");
  assert.equal(orphanRow.input_tokens, null);
  assert.equal(orphanRow.output_tokens, null);
});

test("harness family is a READING; the harness id is the seat's CLAIM", () => {
  //
  // `harness_family` is what WE read off the seat's `harness` id. `"other"` asserts a family, so an
  // id we cannot place must not produce it. The emitter (`harness_and_transport`, `seller_exec.rs`)
  // reaches an unrecognized id by two paths it does not distinguish for us — a config-defined preset
  // name, where a family outside the enum is the truth, and the argv0 BASENAME fallback, where the id
  // names the program that STARTED a harness. Nothing on the receipt separates them, so the family is
  // unavailable and the id is rendered verbatim instead.

  /** The `harness` tag value as it reaches the reader, with no other usage tags to lean on. */
  function familyOf(harnessId) {
    const tags = harnessId == null ? [] : [["harness", harnessId]];
    return extractUsageAdjunct(null, tags);
  }

  // POSITIVE CONTROLS — a classifier that matched nothing would pass every negative case below.
  assert.equal(familyOf("claude-agent-acp").harness_family, "claude");
  assert.equal(familyOf("codex-acp-ng").harness_family, "codex");
  assert.equal(familyOf("cursor-agent").harness_family, "cursor");
  assert.equal(familyOf("claude-agent-acp").harness_id, "claude-agent-acp", "claim carried verbatim");

  // `sh` — the LIVE instance. The emitter's argv0 basename fallback produces it today; measured on
  // 2 of 441 kind-3403 results on relay.maxplayer.ai, 2026-08-21 (one read at one time).
  assert.equal(familyOf("sh").harness_family, null, "a launcher basename names no family");
  assert.equal(familyOf("sh").harness_id, "sh", "the unplaceable id survives for the UI to show");

  // `npx` — the instance this defect was REPORTED as. Measured 0 of 441 on the wire, because the
  // emitter now prefers the preset label and its hatch scans the full argv. Asserted anyway: the fix
  // must key on the SHAPE, so it has to hold where the bug is not, and this is the case a guard
  // written only for the reported string would have special-cased.
  assert.equal(familyOf("npx").harness_family, null);
  assert.equal(familyOf("npx").harness_id, "npx");

  // `deepseek-v4-flash` — a config-defined `[agents]` preset name, 26 of 441 on the wire. The preset
  // name IS the harness identity, so being outside the enum is the truth here, not a failure to read.
  // It shares `harness_family: null` with the two above; the id is what tells them apart, which is
  // exactly why the id must reach the renderer.
  assert.equal(familyOf("deepseek-v4-flash").harness_family, null);
  assert.equal(familyOf("deepseek-v4-flash").harness_id, "deepseek-v4-flash");

  // Absent stays absent — never an empty string, which would read as a value.
  assert.equal(familyOf(null).harness_family, null);
  assert.equal(familyOf(null).harness_id, null);

  // `"other"` is NEVER inferred from an id. It survives only where a seller states it outright in the
  // legacy JSON field, where it is the seller's own claim. This assertion is what fails if the
  // present-but-unrecognized → "other" mapping is ever restored.
  for (const id of ["sh", "npx", "deepseek-v4-flash", "mytool", "grok", "unknown", "bash", "uvx"]) {
    assert.notEqual(familyOf(id).harness_family, "other", `inferred "other" from ${id}`);
  }
  assert.equal(
    extractUsageAdjunct({ harness_family: "other" }, []).harness_family,
    "other",
    "a seller stating other outright is a claim we carry, not an inference we made",
  );

  // A seller-supplied id containing the old key separator must not merge or split economics groups.
  // The key was a "|" join over this very field, so the value could forge a group boundary.
  {
    const sepStore = createStore();
    const mk = (n, harnessId) =>
      ok({
        id: String(n).padStart(2, "f").padEnd(64, "0"),
        pubkey: "fa".padEnd(64, "0"),
        kind: RESULT,
        created_at: 900 + n,
        tags: [
          ["e", String(n).padStart(2, "e").padEnd(64, "0"), "", "root"],
          ["harness", harnessId],
          ["usage_transport", "side-channel"],
        ],
        content: "",
      });
    sepStore.ingest(mk(1, "a|side-channel"));
    sepStore.ingest(mk(2, "a"));
    const groups = sepStore.economics().groups;
    assert.equal(groups.length, 2, "a | inside the id must not collide two distinct harnesses");
    const ids = groups.map(([, g]) => g.harness_id).sort();
    assert.deepEqual(ids, ["a", "a|side-channel"]);
    for (const [, g] of groups) {
      assert.equal(g.transport, "side-channel", "transport read from the value, never split off a key");
      assert.equal(g.family, null);
    }
  }
});

test("advertised versus delivered: a claim paired with its falsifier", () => {
  //
  // ⚠ EVERY BRANCH BELOW EXCEPT `agreed` IS UNREACHABLE ON LIVE DATA. Measured 2026-08-21: all 9 live
  // seats agreed, 9 of 9, and all 9 had receipts. So the live reading cannot show that `unverified`
  // and `contradicted` fire at all, let alone that they stay distinct. These assertions are the only
  // coverage those branches have.
  {
    const seat = "1".repeat(64);
    const other = "2".repeat(64);
    let seq = 0;
    const delivery = (pubkey, harnessId) => {
      seq += 1;
      return ok({
        id: String(seq).padStart(2, "d").padEnd(64, "0"),
        pubkey,
        kind: RESULT,
        created_at: 5000 + seq,
        tags: [
          ["e", String(seq).padStart(2, "c").padEnd(64, "0"), "", "root"],
          ["harness", harnessId],
        ],
        content: "",
      });
    };
    const verify = (advertised, deliveries) =>
      verifyAdvertisedHarnesses(deliveries, seat, advertised);

    // AGREED across the namespace gap: `agents` advertises a preset label, a receipt carries the
    // adapter identity. A string comparison would call this a mismatch on every honest seat.
    {
      const v = verify(["claude"], [delivery(seat, "claude-agent-acp")]);
      assert.equal(v.claims[0].verdict, "agreed");
      // `on` names WHICH vocabulary bridged it. `wire` and `family` are both family readings, as
      // opposed to `id` which is an exact string match; they are reported separately so a test can
      // tell which one did the work. Here the wire vocabulary reads both sides — a preset label and
      // an adapter identity both resolve to `claude-code`.
      assert.equal(v.claims[0].on, "wire", "bridged by family, not by string");
      assert.deepEqual(v.contradictedBy, []);
    }

    // AGREED on the exact id, for an out-of-enum preset name where the name IS the identity on both
    // sides. A family comparison alone maps both sides to null and reads that as no information.
    {
      const v = verify(["deepseek-v4-flash"], [delivery(seat, "deepseek-v4-flash")]);
      assert.equal(v.claims[0].verdict, "agreed");
      assert.equal(v.claims[0].on, "id");
    }

    // UNVERIFIED, with no receipts at all. Must NOT be contradicted: a claim nobody tested is not a
    // claim disproved, and an unverified seat must never render like a lying one.
    {
      const v = verify(["claude"], []);
      assert.equal(v.claims[0].verdict, "unverified");
      assert.equal(v.hasDeliveries, false);
      assert.deepEqual(v.contradictedBy, [], "no receipts cannot contradict anything");
    }

    // UNVERIFIED PER ENTRY — the semantic that matters most. A seat advertising two harnesses and
    // delivering on one has NOT been caught out on the other: dispatch is exact-or-nothing, so the
    // absence of a codex receipt means no job asked for codex.
    {
      const v = verify(["claude", "codex"], [delivery(seat, "claude-agent-acp")]);
      const byLabel = new Map(v.claims.map((c) => [c.advertised, c]));
      assert.equal(byLabel.get("claude").verdict, "agreed");
      assert.equal(byLabel.get("codex").verdict, "unverified");
      assert.deepEqual(v.contradictedBy, [], "an unserved advertisement is not a contradiction");
    }

    // CONTRADICTED — the only real falsifier: a READABLE delivery outside a STATED roster.
    {
      const v = verify(["claude"], [delivery(seat, "codex-acp-ng")]);
      assert.equal(v.claims[0].verdict, "unverified", "the claude claim is untested, not disproved");
      assert.equal(v.contradictedBy.length, 1);
      assert.equal(v.contradictedBy[0].deliveredId, "codex-acp-ng");
      assert.deepEqual(v.incomparableDeliveries, []);
    }

    // INCOMPARABLE, and this is where a false accusation would hide. `sh` is the argv0 basename
    // fallback, so it names the program that STARTED a harness — it could BE claude launched through
    // a shell. Calling that an off-menu delivery asserts knowledge we do not have. Same principle as
    // the family reading: an id whose family we cannot read carries no family.
    {
      const v = verify(["claude"], [delivery(seat, "sh")]);
      assert.deepEqual(v.contradictedBy, [], "an unreadable id cannot contradict a roster");
      assert.equal(v.incomparableDeliveries.length, 1);
      assert.equal(v.incomparableDeliveries[0].deliveredId, "sh");
    }

    // INCOMPARABLE on the CLAIM side: an out-of-enum advertised label cannot be bridged to an adapter
    // identity at all, so the verdict is "could not compare", never "unverified" and never a
    // disagreement. Collapsing these is how a well-behaved seat wears the lying-seat costume.
    {
      const v = verify(["deepseek-v4-flash"], [delivery(seat, "claude-agent-acp")]);
      assert.equal(v.claims[0].verdict, "incomparable");
      assert.deepEqual(v.contradictedBy, [], "no readable label to compare against");
    }

    // …and the contrast that proves the two are distinguished: a READABLE label with no match is
    // `unverified`, because the comparison actually ran.
    {
      const v = verify(["codex"], [delivery(seat, "claude-agent-acp")]);
      assert.equal(v.claims[0].verdict, "unverified");
      assert.notEqual(v.claims[0].verdict, "incomparable", "the comparison ran; it did not fail to run");
      assert.equal(v.contradictedBy.length, 1, "claude is readable and codex is stated → off-menu");
    }

    // A seat advertising NOTHING cannot be contradicted, because it has made no claim. This is the
    // case that produced 7 false positives when the detector asked "is this delivery declared?"
    // instead of "does this delivery contradict a declaration?" — on the relay, every apparent
    // off-menu delivery was a seat with an empty roster, and rendering those as a falsification is
    // exactly the failure the one rule forbids.
    {
      const v = verify([], [delivery(seat, "sh")]);
      assert.equal(v.advertisesNothing, true);
      assert.deepEqual(v.claims, []);
      assert.deepEqual(v.contradictedBy, [], "no roster, no claim, nothing to contradict");
    }

    // Only the seat's OWN receipts count. Another seat's delivery is not evidence about this one.
    {
      const v = verify(["claude"], [delivery(other, "claude-agent-acp")]);
      assert.equal(v.claims[0].verdict, "unverified");
      assert.deepEqual(v.deliveredIds, [], "a different author's receipt is not this seat's evidence");
    }
  }

  console.log("ok — parse/store suite passed");
});
