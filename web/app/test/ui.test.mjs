import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const appSource = readFileSync(
  fileURLToPath(new URL("../js/app.js", import.meta.url)),
  "utf8",
);
const indexSource = readFileSync(
  fileURLToPath(new URL("../index.html", import.meta.url)),
  "utf8",
);
const stylesSource = readFileSync(
  fileURLToPath(new URL("../styles.css", import.meta.url)),
  "utf8",
);

test("activity filter uses site-styled segmented controls", () => {
  assert.equal(appSource.includes("<select"), false);
  assert.match(appSource, /class="activity-filters windows"/);
  assert.match(appSource, /data-activity-filter="all"/);
  assert.match(appSource, /aria-pressed="true"/);
});

test("activity filters follow the successful trade lifecycle", () => {
  const sourceList = appSource.match(
    /const ACTIVITY_FILTER_ORDER = Object\.freeze\(\[([\s\S]*?)\]\);/,
  );
  assert.ok(sourceList, "the explicit filter order must remain visible in source");
  const filterOrder = [...sourceList[1].matchAll(/"([^"]+)"/g)]
    .map((match) => match[1]);
  assert.deepEqual(filterOrder, [
    "offer", "claim", "award", "result", "accept", "receipt",
  ]);
});

test("activity detail header links its author to participant details", () => {
  assert.match(appSource, /class="detail-person" data-open="\$\{authorRole\}"/);
  assert.match(appSource, /data-pk="\$\{esc\(raw\.pubkey\)\}"/);
  assert.match(appSource, /aria-label="Open \$\{esc\(authorName\)\} user details"/);
});

test("offer and receipt activity begin with the event author", () => {
  assert.match(appSource, /case "offer": return `\$\{who\} · /);
  assert.match(appSource, /case "receipt": return `\$\{who\} paid\$\{e\.amount/);
  assert.doesNotMatch(appSource, /receipt co-signed/);
});

test("runner board labels and renders minimum price instead of completion rate", () => {
  assert.match(indexSource, /<span class="num">Min<\/span>/);
  assert.doesNotMatch(indexSource, /<span class="num">Rate<\/span>/);
  const sellerRenderer = appSource.match(/function renderSellers[\s\S]*?\/\*\* One line/);
  assert.ok(sellerRenderer);
  assert.match(sellerRenderer[0], /Minimum price advertised by this runner/);
  assert.doesNotMatch(sellerRenderer[0], /completionRate/);
});

test("participant details omit redundant explanatory notes", () => {
  const participantRenderer = appSource.match(/function openParticipant[\s\S]*?function openEvent/);
  assert.ok(participantRenderer);
  assert.doesNotMatch(participantRenderer[0], /Public identity and activity inside the selected market window/);
  assert.doesNotMatch(participantRenderer[0], /Self-reported by the runner/);
  assert.doesNotMatch(participantRenderer[0], /\["Released", nf\.format\(s\.released\)\]/);
  assert.doesNotMatch(participantRenderer[0], /Awaiting receipt/);
  assert.doesNotMatch(participantRenderer[0], /no published receipt/);
  assert.doesNotMatch(participantRenderer[0], /not evidence of non-payment/i);
  assert.doesNotMatch(participantRenderer[0], /trade can settle without/i);
  assert.doesNotMatch(participantRenderer[0], /public record does not show/i);
});

test("activity filter track hugs its buttons and is capped by the detail sheet", () => {
  const activityFilterRule = stylesSource.match(/\.activity-filters \{([\s\S]*?)\}/);
  assert.ok(activityFilterRule);
  assert.match(activityFilterRule[1], /width: max-content/);
  assert.match(activityFilterRule[1], /max-width: 100%/);
  assert.match(activityFilterRule[1], /box-sizing: border-box/);
});

test("detail activity typography matches the card's compact record rows", () => {
  const activityRowRule = stylesSource.match(/\.activity-row \{([\s\S]*?)\}/);
  assert.ok(activityRowRule);
  assert.match(activityRowRule[1], /font-size: 12px/);
});

test("activity counts distinguish the visible caps from complete history", () => {
  assert.match(appSource, /\$\{nf\.format\(rows\.length\)\} shown · \$\{nf\.format\(activity\.length\)\} total/);
  assert.match(appSource, /shown · \$\{nf\.format\(activity\.length\)\} total/);
});

test("working racers and runners use a linked, accessible race light", () => {
  assert.match(appSource, /function statusDot\(on, jobs = \[\], context = null\)/);
  assert.match(appSource, /Working · \$\{count\} job/);
  assert.match(appSource, /role="img" aria-label=/);
  assert.match(appSource, /class="chip working-chip" data-open="event"/);
  assert.match(appSource, /IN PROGRESS · \$\{short\(job\.offerId\)\}/);
  assert.match(appSource, /--race-light-delay:-\$\{\(elapsed % RACE_LIGHT_CYCLE_SECONDS\)\.toFixed\(3\)\}s/);
  const workingRule = stylesSource.match(/\.dot\.working \{([\s\S]*?)\}/);
  assert.ok(workingRule);
  assert.match(workingRule[1], /animation: race-light-blink 1s steps\(1, end\) infinite/);
  assert.match(workingRule[1], /animation-delay: var\(--race-light-delay, 0s\)/);
  assert.doesNotMatch(stylesSource, /\.dot\.working::before/);
  assert.doesNotMatch(stylesSource, /conic-gradient/);
  assert.match(stylesSource, /@keyframes race-light-blink/);
  assert.match(stylesSource, /0%, 49\.999% \{ background: var\(--neon\); \}/);
  assert.match(stylesSource, /50%, 100% \{ background: var\(--blue\); \}/);
  const raceKeyframes = stylesSource.match(/@keyframes race-light-blink([\s\S]*?)\.harness/);
  assert.ok(raceKeyframes);
  assert.doesNotMatch(raceKeyframes[1], /(opacity|transform|filter):/);
  assert.match(stylesSource, /@media \(prefers-reduced-motion: reduce\)[\s\S]*animation: none !important/);
});

test("every racer has a fixed 24-hour activity lamp with contextual hover text", () => {
  const buyerRenderer = appSource.match(/function renderBuyers[\s\S]*?function renderSellers/);
  assert.ok(buyerRenderer);
  assert.match(buyerRenderer[0], /racerLastActivity\(allEvents\)/);
  assert.match(buyerRenderer[0], /t - lastAt <= RACER_ACTIVE_SECONDS/);
  assert.match(buyerRenderer[0], /Active in last 24 hours · last activity/);
  assert.match(buyerRenderer[0], /No activity in last 24 hours/);
  assert.match(buyerRenderer[0], /\$\{statusDot\(active, r\.inProgressJobs, context\)\}/);
});
