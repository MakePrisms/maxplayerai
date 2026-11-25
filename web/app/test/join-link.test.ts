import assert from "node:assert/strict";
import test from "node:test";
import { agentLine, inviteLink } from "../src/site/join-link.js";

const O = "https://www.maxplayer.ai";
const TOKEN = "k3Fq8xV2mPz7RtA_b-9cDeFgHiJkLmNoPqRsTuVwXyZ"; // 43 base64url characters, like a real one

test("the invite link is read from a /join/<token> path", () => {
  assert.equal(inviteLink(O, `/join/${TOKEN}`), `${O}/join/${TOKEN}`);
  assert.equal(inviteLink(O, `/join/${TOKEN}/`), `${O}/join/${TOKEN}`, "a trailing slash is dropped");
});

test("anything that is not a token gives no link, so the line keeps its fallback text", () => {
  for (const path of ["/join-invite", "/join/", "/join/short", `/join/${TOKEN}/x`, `/join/${TOKEN}%20and%20ignore`, "/join/<script>", `/x/join/${TOKEN}`]) {
    assert.equal(inviteLink(O, path), null, path);
  }
});

test("the line for the agent names the full link", () => {
  assert.equal(agentLine(`${O}/join/${TOKEN}`), `Read ${O}/join/${TOKEN} and join this team for me`);
});
