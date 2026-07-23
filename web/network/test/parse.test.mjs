import assert from "node:assert/strict";
import { createStore } from "../js/store.js";
import {
  extractUsageAdjunct,
  parseEvent,
  parseProfile,
  percentile,
  PROFILE_CONTENT_MAX,
} from "../js/parse.js";
import { CLAIM, HANDLER, OFFER, RECEIPT, RESULT } from "../js/kinds.js";

function ok(ev) {
  const n = parseEvent(ev);
  assert.ok(n, "expected parse success");
  return n;
}

// ——— defensive parse: hostile / malformed must not throw or blank store ———

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

const store = createStore();
for (const g of garbage) {
  assert.doesNotThrow(() => store.ingest(parseEvent(g)));
}

// one good offer after garbage — funnel still renders numbers
const offerId = "1".repeat(64);
store.ingest(
  ok({
    id: offerId,
    pubkey: "2".repeat(64),
    kind: OFFER,
    created_at: 100,
    tags: [
      ["i", "task"],
      ["amount", "21", "sat"],
      ["t", "mobee"],
      ["v", "2"],
    ],
    content: "",
  }),
);

const funnel = store.funnel();
assert.equal(funnel.offers, 1);
assert.equal(funnel.leaks.unclaimed, 1);
assert.ok(funnel.parseSkips >= 1, "malformed events counted as skips");
assert.doesNotThrow(() => store.snapshot());

// ——— usage adjunct vocabulary (Scribe lock) ———

const adjunct = extractUsageAdjunct(
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

// ——— census harness_name + version ———

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
store.ingest(handler);
assert.equal(store.census()[0].harness_name, "cursor-agent");

// Same d-tag from distinct pubkeys must not collapse (NIP-89 addressable key includes author).
store.ingest(
  ok({
    id: "f0".repeat(32),
    pubkey: "f1".repeat(32),
    kind: HANDLER,
    created_at: 301,
    tags: [
      ["d", "seller-a"],
      ["k", "5109"],
    ],
    content: JSON.stringify({
      harness_name: "other-agent",
      version: "1.0.0",
    }),
  }),
);
{
  const census = store.census();
  assert.equal(census.length, 2);
  const pubkeys = new Set(census.map((r) => r.pubkey));
  assert.equal(pubkeys.size, 2);
  assert.ok(pubkeys.has("6".repeat(64)));
  assert.ok(pubkeys.has("f1".repeat(32)));
}

// ——— latency path ———
store.ingest(
  ok({
    id: "7".repeat(64),
    pubkey: "8".repeat(64),
    kind: CLAIM,
    created_at: 130,
    tags: [
      ["status", "processing"],
      ["e", offerId],
      ["t", "mobee"],
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
      ["t", "mobee"],
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

// ——— kind-0 profiles + newest-first tail + id dedupe ———

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
    ["t", "mobee"],
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
    ["t", "mobee"],
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

// ——— usage adjunct reads from result tags (spec wins) ———

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
    ["t", "mobee"],
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
    ["t", "mobee"],
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

// harness_family mapping across the spec enum; unknown → "other"; absent → null.
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
  "other",
);
assert.equal(extractUsageAdjunct(null, []).harness_family, null);

// Dashboard END-TO-END: a tagged result fills its economics row; an untagged one dashes.
const eco2 = createStore();
eco2.ingest(taggedResult);
eco2.ingest(untaggedResult);
const e2rows = eco2.economics().rows;
const tRow = e2rows.find((r) => r.id === taggedResult.id);
const uRow = e2rows.find((r) => r.id === untaggedResult.id);
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

// ——— row SOURCE: "delivered" (6109-only) must never read as "paid" (3400-backed) ———

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

// ——— JOIN via the receipt's exact reply-tag binding → PAID row shows the result's usage ———
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

console.log("ok — parse/store suite passed");
