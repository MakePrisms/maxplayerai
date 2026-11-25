/**
 * protocol-v1 §7.1: the class of a feedback comes from `reason_code` first, then `status`.
 * Every refusal code in the §7.1 table must classify as `refusal` even when a peer sends
 * `status=error`, so it ends the attempt without reading as a work failure.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { feedbackClass, type RawEvent } from "../src/model/events.js";

const feedback = (reason: string, status: string): RawEvent => ({
  id: "f".repeat(64),
  kind: 3404,
  pubkey: "a".repeat(64),
  created_at: 1,
  tags: [
    ["status", status],
    ["reason_code", reason],
  ],
});

test("session refusal codes classify as refusal regardless of status", () => {
  for (const code of ["session_expired", "turn_too_large", "capability_missing"]) {
    assert.equal(feedbackClass(feedback(code, "error")), "refusal", code);
  }
});

test("an unknown reason_code still falls back to status", () => {
  assert.equal(feedbackClass(feedback("not_a_code", "error")), "error");
});
