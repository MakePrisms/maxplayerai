import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { test } from "node:test";

import { parseEvent } from "../js/parse.js";
import { createStore } from "../js/store.js";
import { HEARTBEAT, RESULT, SELLER_HEARTBEAT_D } from "../js/kinds.js";

/**
 * A seat announcement carries OPERATOR-SUPPLIED TEXT that arrived over a network: the advertised
 * roster, the status, and — once the #784 fields land — the free-text hardware colour. It is
 * untrusted input for escaping purposes, and the seat directory renders it.
 *
 * Two properties, and they are different. The reader must PRESERVE hostile text rather than mangle
 * or sanitise it, because a reader that quietly rewrites values makes every other field a lie. The
 * RENDERER is then what neutralises it, via text nodes. So: preserve here, escape there, and lock
 * the render path so nobody re-opens it.
 */

const HEX = "a".repeat(64);
const PAYLOADS = [
  "<img src=x onerror=alert(1)>",
  "</script><script>alert(1)</script>",
  '" onmouseover="alert(1)',
  "javascript:alert(1)",
  "‮gnp.exe",
  "'; DROP TABLE seats;--",
];

test("hostile operator text in a seat announcement survives to the renderer intact", () => {
  const NOW = 10_000;
  for (const payload of PAYLOADS) {
    const store = createStore();
    store.ingest(
      parseEvent({
        id: "b".repeat(64),
        pubkey: HEX,
        kind: HEARTBEAT,
        created_at: NOW - 10,
        tags: [
          ["d", SELLER_HEARTBEAT_D],
          ["t", "maxplayer"],
          ["v", "1"],
          ["accepting", "y"],
          ["queue_depth", "0"],
          ["agents", payload],
          ["status", payload],
        ],
        content: "",
      }),
    );
    const seat = store.census(NOW).seats[0];
    assert.ok(seat, `payload must not drop the seat: ${payload}`);
    assert.deepEqual(seat.agents, [payload], "roster preserved, for the renderer to escape");
    assert.equal(seat.status, payload, "status preserved, for the renderer to escape");
  }
});

test("a hostile harness id survives to the renderer intact", () => {
  for (const payload of PAYLOADS) {
    const store = createStore();
    store.ingest(
      parseEvent({
        id: "c".repeat(64),
        pubkey: HEX,
        kind: RESULT,
        created_at: 500,
        tags: [
          ["e", "d".repeat(64), "", "root"],
          ["harness", payload],
        ],
        content: "",
      }),
    );
    const row = store.economics().rows.find((r) => r.harness_id === payload);
    assert.ok(row, `harness id preserved verbatim: ${payload}`);
    // An unreadable id yields no family — it must not be laundered into one by a payload that
    // happens to contain a family name as a substring of something else.
    assert.equal(row.harness_family, null);
  }
});

test("a payload cannot forge an economics group boundary", () => {
  // The group key is JSON, not a separator join, so a value containing the old "|" separator must
  // not merge two harnesses into one row or split one across two.
  const store = createStore();
  const mk = (n, harnessId) =>
    parseEvent({
      id: String(n).padStart(2, "e").padEnd(64, "0"),
      pubkey: HEX,
      kind: RESULT,
      created_at: 600 + n,
      tags: [
        ["e", String(n).padStart(2, "f").padEnd(64, "0"), "", "root"],
        ["harness", harnessId],
        ["usage_transport", "side-channel"],
      ],
      content: "",
    });
  store.ingest(mk(1, 'a"|null|"side-channel'));
  store.ingest(mk(2, "a"));
  const groups = store.economics().groups;
  assert.equal(groups.length, 2, "a JSON-ish payload in the id must not collide two harnesses");
});

/**
 * THE RENDER PATH IS LOCKED BY THIS TEST, not by discipline.
 *
 * Every value reaches the DOM through `text()`, which is `document.createTextNode` — escaped by
 * construction. That guarantee holds only while nothing assigns interpolated data to `innerHTML`.
 * Clearing a node with the empty-string literal is fine and is the only form allowed here.
 *
 * A source scan rather than a DOM test, deliberately: this suite has no DOM, so a behavioural test
 * would need a harness that does not exist, and the property is about which API is called. The
 * failure mode of NOT having this test is that someone adds one interpolated `innerHTML` and every
 * escaping guarantee in the app silently stops holding.
 */
test("no module assigns anything but the empty string to innerHTML", () => {
  const ROOT = fileURLToPath(new URL("..", import.meta.url));
  /** @type {string[]} */
  const files = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir)) {
      const p = join(dir, name);
      if (statSync(p).isDirectory()) walk(p);
      else if (p.endsWith(".js") || p.endsWith(".mjs")) files.push(p);
    }
  };
  walk(join(ROOT, "js"));
  walk(join(ROOT, "scripts"));

  const offenders = [];
  let seen = 0;
  for (const file of files) {
    const src = readFileSync(file, "utf8");
    for (const [i, line] of src.split("\n").entries()) {
      if (!line.includes("innerHTML")) continue;
      seen += 1;
      // The only permitted form: `<something>.innerHTML = "";`
      if (!/\.innerHTML\s*=\s*""\s*;?\s*$/.test(line.trim())) {
        offenders.push(`${file}:${i + 1}: ${line.trim()}`);
      }
    }
  }

  // Emit the denominator: an assertion that scanned nothing passes exactly like one that scanned
  // everything and found nothing wrong.
  assert.ok(files.length >= 8, `expected to scan the js/ + scripts/ modules, scanned ${files.length}`);
  assert.ok(seen >= 5, `expected to find the known innerHTML clear sites, found ${seen}`);
  assert.deepEqual(offenders, [], `interpolated innerHTML breaks every escaping guarantee:\n${offenders.join("\n")}`);
});
