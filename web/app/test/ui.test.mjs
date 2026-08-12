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
  // The pressed button is bound to the tracked filter rather than hard-coded to
  // All: the panel is rebuilt on every poll tick, so markup that always came
  // back with All pressed would quietly undo the reader's choice.
  assert.match(appSource, /data-activity-filter="all" aria-pressed="\$\{active === "all"\}"/);
  assert.match(appSource, /aria-pressed="\$\{type === active\}"/);
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
  const participantRenderer = appSource.match(/function participantSheet[\s\S]*?function openEvent/);
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
  // #681: the lamp reads `workingJobs`, not `inProgressJobs`. The two differ by
  // exactly the overdue awards, so binding it back to `inProgressJobs` would
  // restore the bug where a seat that delivered nothing lights up as busy.
  assert.match(buyerRenderer[0], /\$\{statusDot\(active, r\.workingJobs, context\)\}/);
});

test("#681: overdue awards render as their own stopped state, not as louder working", () => {
  const detail = appSource.match(/function participantSheet[\s\S]*?const profile = profiles\.get/);
  assert.ok(detail);
  assert.match(detail[0], /job\.state === JOB_OVERDUE/);
  assert.match(detail[0], /Overdue · \$\{nf\.format\(overdue\.length\)\}/);
  assert.match(detail[0], /OVERDUE · \$\{short\(job\.offerId\)\}/);
  // The award stays reachable: an overdue chip still opens its job history.
  assert.match(detail[0], /class="chip overdue-chip" data-open="event" data-id="\$\{job\.awardId\}"/);
  // And it must not claim a payment or cancellation the relay never recorded.
  assert.match(detail[0], /nothing here says it was paid or cancelled/);

  // Styled as stopped rather than as an alternative working state: no reuse of
  // the working chip's red, which is what the lamp means.
  const overdueRule = stylesSource.match(/\.overdue-chip \{[\s\S]*?\}/);
  assert.ok(overdueRule, "overdue-chip needs its own rule");
  assert.doesNotMatch(overdueRule[0], /--neon|rgba\(255, 40, 0/);
});

/* #705: the participant panel was built once, at click time, and never again —
   so watching one agent work, the reason to open it, was the one thing it could
   not do. These pin the panel to the same tick as the columns behind it. */

test("#705: the open participant panel repaints on the poll tick", () => {
  // Building the markup is separate from opening the sheet, so the tick can
  // rebuild without re-opening anything.
  assert.match(appSource, /function participantSheet\(role, pubkey, events, allEvents\)/);
  const open = appSource.match(/function openParticipant\(role, pubkey, events, allEvents\) \{[\s\S]*?\n\}/);
  assert.ok(open);
  assert.match(open[0], /showSheet\(participantSheet\(role, pubkey, events, allEvents\)\);/);
  assert.match(open[0], /openPanel = \{ role, pubkey \};/);
  // The filter reset precedes the build that reads it, or a newly opened panel
  // renders under the previous participant's filter with All shown as pressed.
  assert.ok(
    open[0].indexOf('activityFilter = "all";') < open[0].indexOf("showSheet("),
    "the activity filter must be reset before the sheet markup is built",
  );

  // render() drives it, inside keepScroll, alongside the columns.
  const renderer = appSource.match(/function render\(\) \{[\s\S]*?\n\}/);
  assert.ok(renderer);
  assert.match(renderer[0], /keepScroll\(\(\) => \{[\s\S]*?refreshParticipant\(events, allEvents\);[\s\S]*?\}\);/);

  // No new fetching: the repaint reads the same cache snapshot render() already
  // took for the columns. A relay call on a 3s tick per open panel is a bug.
  const refresh = appSource.match(/function refreshParticipant\([\s\S]*?\n\}/);
  assert.ok(refresh);
  assert.doesNotMatch(refresh[0], /cache\.all\(\)|client\.|fetch\(/);
});

test("#705: the repaint keeps the panel where the reader left it", () => {
  // The sheet scrolls internally, like the three columns, so it is carried
  // across the innerHTML swap by the same mechanism.
  assert.match(appSource, /const SCROLLERS = \["buyers", "feed", "sellers", "detail"\];/);

  const refresh = appSource.match(/function refreshParticipant\([\s\S]*?\n\}/);
  assert.ok(refresh);
  // Not showSheet: that focuses the close button, which on a 3s tick would drag
  // focus off whatever the reader is on.
  assert.doesNotMatch(refresh[0], /showSheet\(/);
  assert.match(refresh[0], /body\.innerHTML = participantSheet\(openPanel\.role, openPanel\.pubkey, events, allEvents\)/);
  assert.match(refresh[0], /focusKey[\s\S]*?body\.querySelector\(focusKey\)\?\.focus\(\)/);

  // And the chosen activity filter is state, not just DOM the repaint discards.
  assert.match(appSource, /let activityFilter = "all";/);
  assert.match(appSource, /activityFilter = selected;/);
  assert.match(appSource, /const active = types\.includes\(activityFilter\) \? activityFilter : "all";/);
});

test("#705: only the participant panel claims the tick, and closing releases it", () => {
  // Every sheet routes through showSheet, so opening an event from inside a
  // panel ends the panel's claim — otherwise the next tick would overwrite the
  // event view the reader just asked for.
  const show = appSource.match(/function showSheet\(html\) \{[\s\S]*?\n\}/);
  assert.ok(show);
  assert.match(show[0], /openPanel = null;/);

  const close = appSource.match(/function closeSheet\(\) \{[\s\S]*?\n\}/);
  assert.ok(close);
  assert.match(close[0], /openPanel = null;/);

  const refresh = appSource.match(/function refreshParticipant\([\s\S]*?\n\}/);
  assert.ok(refresh);
  assert.match(refresh[0], /if \(!openPanel \|\| el\("detail"\)\.hidden\) return;/);
});
