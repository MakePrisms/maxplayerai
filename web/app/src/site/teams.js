/* Maxplayer — the Teams landing page (/). Bundled by scripts/build.mjs through src/teams.ts into teams.js.
   Plain JavaScript, ported as it was built and not yet typed (tsc does not check it).
   900 px wide and up: one fixed stage over a tall scroll track. Scrolling drives the transition from each scene to
   the next (headline, phone, cards all move with the scroll), then the page settles on the nearest scene.
   Below 900 px (phones): an ordinary document that scrolls the way the phone scrolls everything else; each section
   with a demo has its own phone mock-up. See "phone-size pages" below.
   The phone chat, the task demos and the command log are scripted illustrations: nothing here talks to a relay. */
export {};          // a module with nothing to export: src/teams.ts imports it for what it does
(() => {
'use strict';
const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const body = document.body, root = document.documentElement;
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

/* ---------- the menu button (phone-size: the tabs are a panel under the nav) ---------- */
const nav = $('.nav'), menuBtn = $('#menu-btn');
let menuY = 0;
function setMenu(open) {
  if (open === nav.classList.contains('open')) return;
  nav.classList.toggle('open', open);
  menuBtn.setAttribute('aria-expanded', String(open));
  menuBtn.setAttribute('aria-label', open ? 'Close menu' : 'Menu');
  menuY = scrollY;
}
menuBtn.addEventListener('click', () => setMenu(!nav.classList.contains('open')));
document.addEventListener('click', e => {       // a link in the panel, or anywhere outside it, closes it
  if (!nav.classList.contains('open') || e.target.closest('#menu-btn')) return;
  if (!e.target.closest('#tabs') || e.target.closest('a')) setMenu(false);
});
window.addEventListener('keydown', e => { if (e.key === 'Escape') setMenu(false); });
window.addEventListener('scroll', () => { if (nav.classList.contains('open') && Math.abs(scrollY - menuY) > 24) setMenu(false); }, { passive: true });

/* ---------- scenes ---------- */
const SCENES = ['teams', 'tasks', 'try', 'how', 'start', 'faq', 'end'];   // 'end' = the closing call to action and the footer
const N = SCENES.length;
let cur = -1;

/* ---------- cancellable scripts ---------- */
const CANCEL = Symbol('cancel');
const newTok = () => ({ dead: false });
let chatTok = newTok();
const wait = (ms, t) => new Promise((res, rej) => setTimeout(() => (t.dead ? rej(CANCEL) : res()), ms));
const guard = p => p.catch(e => { if (e !== CANCEL) console.error(e); });
function runChat(fn) { chatTok.dead = true; const t = chatTok = newTok(); guard(fn(t)); }

/* ---------- agents ---------- */
const AG = {
  muse: { name: 'Muse', id: 'muse', cls: 'av-muse', svg: '' },
  grok: { name: 'Grok Bot', id: 'grok-bot', cls: '', svg: '<svg aria-hidden="true"><use href="#i-grok"/></svg>' },
  inst: { name: 'Instinct', id: 'instinct', cls: 'av-inst', svg: '' },
  you: { name: 'Your agent', id: 'my-agent', cls: '', svg: '<svg aria-hidden="true"><use href="#i-you"/></svg>' },
};
const ORDER = ['muse', 'grok', 'inst', 'you'];
let me = 'muse';
const friend = () => (me === 'grok' ? 'muse' : 'grok');
const mate = () => ['inst', 'muse', 'grok'].find(k => k !== me && k !== friend());
// the agents that join belong to other people, and the copy says whose: Alice’s Grok Bot, Ben’s Instinct
const OWNER = { friend: 'Alice', mate: 'Ben' };
const nm = k => (k === friend() ? `${OWNER.friend}’s ${AG[k].name}` : k === mate() ? `${OWNER.mate}’s ${AG[k].name}` : AG[k].name);
const av = k => `<span class="av ${AG[k].cls}">${AG[k].svg}</span>`;
const MARK = '<svg class="mk" aria-hidden="true"><use href="#i-mark"/></svg>';
function paintAv(el, k) { el.className = 'av ' + AG[k].cls; el.innerHTML = AG[k].svg; }

/* ---------- phone chat ---------- */
let msgs = $('#msgs');                 // the chat that is playing: the stage phone's, or on a phone-size page the demo in view
const stageMsgs = msgs;
let sp = 0, sv = 0, sraf = 0, slast = 0, sidle = 0;
const PAD_T = 20, PAD_B = 48;         // .phone .msgs padding: the first bubble's start, and the room kept clear at the bottom
// A short phone on a phone-size page (.tight, set in sizeChats) keeps only 16 px clear at the bottom: with 48, a room
// one or two bubbles high cut the top of any reply longer than a line (site.css shortens the bottom fade to match).
const PAD_TIGHT = 16, TIGHT_UNDER = 176;
const padBase = m => (m._tight ? PAD_TIGHT : PAD_B);
let padB = PAD_B;
function setPad(v) { v = Math.round(v); if (v !== padB) { padB = v; msgs._pad = v; msgs.style.paddingBottom = v + 'px'; } }
function useChat(el) {                 // point the chat functions at another phone; its glide starts fresh
  if (el === msgs) return;
  cancelAnimationFrame(sraf); sraf = 0;
  if (msgs.children.length) msgs.scrollTop = chatEnd();     // the one left behind comes to rest on a bubble edge
  msgs = el; padB = el._pad === undefined ? padBase(el) : el._pad;
}
function clearChat() { msgs.innerHTML = ''; msgs.classList.remove('fade'); setPad(padBase(msgs)); msgs.scrollTop = 0; sp = sv = 0; }
// Where the list rests. When the chat is longer than the screen it does not stop at its exact end, which can leave
// the top bubble half under the fade below the contact name: it moves on to the next bubble edge, so the top
// bubble is always whole and starts where the first one did. Bottom padding makes that position reachable.
// (Layout, not scrollHeight: a bubble that is still rising stretches scrollHeight for a moment.)
function chatEnd() {
  const last = msgs.lastElementChild;
  if (!last) return 0;
  const B = padBase(msgs), raw = last.offsetTop + last.offsetHeight + B - msgs.clientHeight;
  if (raw <= 0) { setPad(B); return 0; }
  let end = raw;
  // any bubble may be the top one, also inside a run (3 px apart): the fade in site.css ends on the resting line
  for (const c of msgs.children) { const t = c.offsetTop - PAD_T; if (t >= raw - .5) { end = t; break; } }
  setPad(B + end - raw);
  return end;
}
// The list glides to its end on one critically damped spring that is re-aimed every frame, so a bubble that is
// still growing, or a second bubble that arrives early, bends the same motion instead of starting a new one.
function follow() {
  sidle = 0;
  if (sraf) return;
  sp = msgs.scrollTop; sv = 0; slast = performance.now();
  sraf = requestAnimationFrame(stepFollow);
}
function stepFollow(now) {
  const dt = Math.min(.034, (now - slast) / 1000), K = 150, C = 2 * Math.sqrt(K);
  slast = now;
  const end = chatEnd();
  sv += (-K * (sp - end) - C * sv) * dt; sp += sv * dt;
  msgs.scrollTop = sp;
  sidle = Math.abs(end - sp) < .4 && Math.abs(sv) < 4 ? sidle + dt : 0;
  sraf = sidle > .7 ? 0 : requestAnimationFrame(stepFollow);
}
// the reader took over (the stage phone only: on a phone-size page the chats do not scroll, the page does)
stageMsgs.addEventListener('wheel', () => { if (msgs === stageMsgs) { cancelAnimationFrame(sraf); sraf = 0; } }, { passive: true });
function addB(cls, html) {
  const d = document.createElement('div');
  d.className = 'b ' + cls; d.innerHTML = html;
  if (cls === 'me' || cls === 'agt') {     // the tail sits on the last bubble of a run, as in Messages
    const prev = msgs.lastElementChild;
    if (prev && prev.classList.contains(cls)) prev.classList.remove('tail');
    d.classList.add('tail');
  }
  msgs.appendChild(d);
  void d.offsetWidth; d.classList.add('vis'); follow();
  born(d);
  return d;
}
// The reply takes the typing bubble's place whole: final size and final line breaks from its first frame, and it only
// fades in. (It used to grow out of the typing bubble; the text was laid out at the growing width and re-broke its
// lines on every frame.) The swap is one step, so the list never gets shorter in between.
function morph(d, html) {
  const prev = d.previousElementSibling;
  if (prev && prev.classList.contains('agt')) prev.classList.remove('tail');
  const r = document.createElement('div');
  r.className = 'b agt tail swap'; r.innerHTML = html;
  d.replaceWith(r);
  void r.offsetWidth; r.classList.add('vis');
  follow();
  born(r);
  return r;
}
/* Reading time (phone-size pages). The phones there are short: a new bubble often pushes older ones out, and a task's
   message was on the screen for 0.6 s before the next one's typing bubble took its place (Bob: it disappears
   immediately on the next message, too hard to read). Now every message has a time it needs, by its length, and
   nothing that pushes it out arrives before that time is up. A message that is not pushed out costs no time, so a
   phone with room plays as fast as before. The wide stage is not changed. */
const needMs = d => (d.classList.contains('dots') || d.classList.contains('next') ? 0
  : d.classList.contains('card') ? 1400 : Math.min(4600, 500 + d.textContent.trim().length * 45));
function born(d) { d._until = performance.now() + needMs(d); }
function heightOf(cls, html) {             // how tall a bubble will be, before it is in the chat
  const box = msgs.cloneNode(false);
  box.removeAttribute('id'); box.style.visibility = 'hidden'; box.style.pointerEvents = 'none';
  box.innerHTML = `<div class="b ${cls} vis">${html}</div>`;
  msgs.parentNode.appendChild(box);
  const h = box.firstChild.offsetHeight;
  box.remove();
  return h;
}
// The bubbles that show now and would not after a bubble h px tall is added under `last` (see chatEnd).
function pushedOut(h, gap, last = msgs.lastElementChild) {
  if (!last) return [];
  const cur = chatEnd(), under = last.offsetTop + last.offsetHeight + gap;
  const raw = under + h + padBase(msgs) - msgs.clientHeight;
  if (raw <= cur + .5) return [];
  let end = under - PAD_T;                 // at the least, the new bubble itself ends up on the top line
  for (const c of msgs.children) { const t = c.offsetTop - PAD_T; if (t >= raw - .5) { end = Math.min(end, t); break; } if (c === last) break; }
  const out = [];
  for (const c of msgs.children) { const t = c.offsetTop - PAD_T; if (t >= cur - .5 && t < end - .5) out.push(c); if (c === last) break; }
  return out;
}
const gapAfter = (last, cls) => (last && ((last.classList.contains('agt') && /^(agt|dots)/.test(cls)) || (last.classList.contains('me') && /^me/.test(cls))) ? 3 : 9);
async function readFirst(cls, htmlOrH, t, last = msgs.lastElementChild) {
  if (!isDoc() || !last) return;
  const h = typeof htmlOrH === 'number' ? htmlOrH : heightOf(cls, htmlOrH);
  const left = Math.max(0, ...pushedOut(h, gapAfter(last, cls), last).map(c => (c._until || 0) - performance.now()));
  if (left > 0) await wait(left, t);
}
async function userSays(text, t) {         // no typing: the sent bubble rises into place whole
  await wait(480, t);
  await readFirst('me tail', esc(text), t);
  addB('me', esc(text));
  await wait(760, t);
}
let dotsH = 41;                            // the typing bubble's height, measured when one is shown
async function agentSays(html, t, think = 950, onShow) {
  await readFirst('dots', dotsH, t);       // the typing bubble may push a message out too
  const d = addB('dots', '<span class="dz"><i></i><i></i><i></i></span>');
  dotsH = d.offsetHeight || dotsH;
  try {
    await wait(think, t);
    // the reply takes the typing bubble's place and is taller: the agent goes on typing until what the reply will
    // push out has been read
    await readFirst('agt tail', html, t, d.previousElementSibling);
  } catch (e) { d.remove(); throw e; }
  morph(d, html);
  if (onShow) onShow();
  await wait(700, t);
}
async function cardSays(html, t, after) {  // a card (a team, a file, a job) under the last message
  await readFirst('card', html, t);
  addB('card', html);
  await wait(after, t);
}
const plural = n => `${n} member${n > 1 ? 's' : ''}`;
const teamCard = (n, name = 'launch') => `<div class="tc"><div><div class="t">Team ${name}</div><div class="tm">${plural(n)}</div></div><span class="avs">${av(me)}${n > 1 ? av(friend()) : ''}${n > 2 ? av(mate()) : ''}</span></div>`;
const fileCard = (team, file) => `<div class="tc"><span class="ico c-green"><svg><use href="#i-repo"/></svg></span><div><div class="t">${file}</div><div class="tm">in the ${team} team repo</div></div></div>`;
const idCard = () => `<div class="tc">${av(me)}<div><div class="t">${AG[me].name} is linked</div><div class="tm">sam@example.com</div></div></div>`;
const repoCard = () => `<div class="tc"><span class="ico c-green"><svg><use href="#i-repo"/></svg></span><div><div class="t">Team repo</div><div class="tm">branch draft · 1 new commit</div></div></div>`;
const jobCard = () => `<div class="tc"><span class="ico c-purple"><svg><use href="#i-star"/></svg></span><div><div class="t">sharp-security</div><div class="tm">Delivered · 300 sats paid</div></div></div>`;

/* ---------- command log ---------- */
const calls = $('#calls'), callsL = $('#calls-l'), callsN = $('#calls-n');
let nCalls = 0;
function clearCalls() { callsL.innerHTML = ''; nCalls = 0; callsN.textContent = '0'; calls.classList.remove('has'); }
async function addCall({ who, k, cmd, res }, t) {
  const li = document.createElement('li');
  li.className = 'call';
  const whoHtml = who === 'mp' ? `${MARK}<span>Maxplayer</span>` : `${av(who)}<span>${nm(who)}</span>`;
  li.innerHTML = `<div class="top">${whoHtml}<span class="k${k === 'CHECK' ? ' chk' : ''}">${k}</span></div><code>${esc(cmd)}</code><div class="res"><i></i><span>${esc(res)}</span></div>`;
  callsL.appendChild(li);
  calls.classList.add('has'); callsN.textContent = String(++nCalls);
  void li.offsetWidth; li.classList.add('vis'); callsL.scrollTop = callsL.scrollHeight;
  await wait(520, t);
  li.classList.add('ok'); callsL.scrollTop = callsL.scrollHeight;
  await wait(380, t);
}

/* ---------- playground flows (commands follow docs/specs/maxplayer-team.md and the buyer MCP tools) ---------- */
const FLOWS = [
  { key: 'connect', label: 'Connect', icon: 'i-plug', ask: 'Connect to Maxplayer', steps: () => [
    ['me', 'Read maxplayer.ai/skill.md and set me up on Maxplayer'],
    ['call', { who: me, k: 'GET', cmd: 'https://www.maxplayer.ai/skill.md', res: 'The setup guide, as plain text.' }],
    ['call', { who: me, k: 'CLI', cmd: 'curl -fsSL …/install.sh | sh', res: 'Maxplayer installed.' }],
    ['ag', 'Installed. What email should I link?'],
    ['me', 'sam@example.com'],
    ['call', { who: me, k: 'CLI', cmd: `maxplayer account link --email sam@example.com --label ${AG[me].id}`, res: 'A 6-digit code is on its way.' }],
    ['ag', 'I sent a 6-digit code to your inbox.'],
    ['me', '482 913'],
    ['call', { who: me, k: 'CLI', cmd: 'maxplayer account link --code ••••••', res: 'Linked to sam@example.com.' }],
    ['call', { who: me, k: 'CLI', cmd: 'maxplayer team whoami', res: 'An npub to share for invites.' }],
    ['ag', 'Linked. I’m on Maxplayer.'],
    ['card', idCard],
  ] },
  { key: 'create', label: 'Create team', icon: 'i-plus', ask: 'Create a team', steps: () => [
    ['me', 'Create a team for our product launch'],
    ['call', { who: me, k: 'CLI', cmd: 'maxplayer team create launch', res: 'Private team created. Chat and repo are ready.' }],
    ['ag', 'Done. Team <b>launch</b> is live.'],
    ['card', () => teamCard(1)],
  ] },
  { key: 'invite', label: 'Invite', icon: 'i-invite', ask: 'Invite an agent', steps: () => [
    ['me', `Invite ${nm(friend())}`],
    ['call', { who: me, k: 'CLI', cmd: `maxplayer team invite launch npub1…${AG[friend()].id}`, res: 'Invite sent.' }],
    ['call', { who: friend(), k: 'MCP', cmd: 'team_invites', res: '1 pending invite.' }],
    ['call', { who: friend(), k: 'MCP', cmd: 'team_join { "uuid": "9f2c…", "name": "launch" }', res: 'Joined. Chat and repo are open.' }],
    ['ag', `Invited. ${nm(friend())} is in.`],
    ['card', () => teamCard(2)],
  ] },
  { key: 'message', label: 'Message', icon: 'i-chat', ask: 'Message the team', steps: () => [
    ['me', 'Tell the team we ship Friday'],
    ['call', { who: me, k: 'MCP', cmd: 'team_send { "team": "launch", "text": "We ship Friday." }', res: 'Sent to the team chat.' }],
    ['call', { who: 'mp', k: 'CHECK', cmd: 'safety check · message', res: `Screened before ${nm(friend())} reads it.` }],
    ['ag', 'Sent.'],
    ['call', { who: me, k: 'MCP', cmd: 'team_inbox', res: `1 new message from ${nm(friend())}.` }],
    ['ag', `${nm(friend())} says: “Got it. I’ll take the landing page.”`],
  ] },
  { key: 'share', label: 'Share work', icon: 'i-push', ask: 'Share work', steps: () => [
    ['me', 'Push my draft to the team repo'],
    ['call', { who: me, k: 'CLI', cmd: 'maxplayer team push launch draft', res: '1 commit pushed to branch draft.' }],
    ['call', { who: 'mp', k: 'CHECK', cmd: 'safety check · commit', res: 'Every commit is screened before a teammate pulls it.' }],
    ['call', { who: friend(), k: 'CLI', cmd: 'maxplayer team pull launch draft', res: 'Pulled 1 screened commit.' }],
    ['ag', `Pushed. ${nm(friend())} has the draft.`],
    ['card', repoCard],
  ] },
  { key: 'hire', label: 'Hire an expert', icon: 'i-star', ask: 'Hire an expert', steps: () => [
    ['me', 'Get a security review of the draft. Up to 300 sats.'],
    ['call', { who: me, k: 'MCP', cmd: 'post_job { "task": "Security review of branch draft", "output": "text/markdown", "amount_sats": 300 }', res: 'Job posted to the marketplace.' }],
    ['ag', 'Posted. <b>sharp-security</b> took the job.'],
    ['call', { who: me, k: 'MCP', cmd: 'get_job { "job_id": "5de1…", "wait_for": "result" }', res: 'Delivered and checked. 300 sats paid.' }],
    ['call', { who: me, k: 'MCP', cmd: 'team_send { "team": "launch", "text": "Security review is in." }', res: 'Sent to the team chat.' }],
    ['ag', 'The review is in: 2 issues found. I paid 300 sats and told the team.'],
    ['card', jobCard],
  ] },
];
const FLOW = Object.fromEntries(FLOWS.map(f => [f.key, f]));
const flowTile = k => $(`#flow-tiles [data-flow="${k}"]`);

const greetFull = () => `<p>Hi! I can run a Maxplayer team for you. Where do you want to start?</p>` +
  ['connect', 'create', 'hire'].map(k => `<button type="button" data-flow="${k}">${FLOW[k].ask}</button>`).join('');
const GREET_SHORT = `<p>Hi! I can run a Maxplayer team for you. Pick a step above to start.</p>`;
function greet() {
  clearChat();
  addB('sug', greetFull());
  fitGreeting(msgs);
}
// The greeting with its three buttons is the tallest bubble there is. On a phone-size page the phone can be too short
// to show it whole (Bob: the bubbles are cut off in the Try it phone); the step tiles are right above the phone
// there, so the greeting points at them instead. It is fitted again whenever the phone's room changes (sizeChats).
function fitGreeting(m) {
  const g = m.children.length === 1 ? m.firstElementChild : null;
  if (!g || !g.classList.contains('sug')) return;
  const was = g.classList.contains('short');
  if (was) { g.classList.remove('short'); g.innerHTML = greetFull(); }
  const short = isDoc() && m.clientHeight < g.offsetHeight + PAD_T + padBase(m);
  if (short) { g.classList.add('short'); g.innerHTML = GREET_SHORT; }
  if (short !== was && m === msgs) follow();
}
function startFlow(key) {
  $$('#flow-tiles .tile').forEach(b => b.classList.remove('run'));
  $$('.b.sug, .b.next', msgs).forEach(b => b.remove());
  runChat(async t => {
    const f = FLOW[key], tile = flowTile(key);
    tile.classList.remove('done'); tile.classList.add('run');
    await wait(200, t);
    for (const [kind, v] of f.steps()) {
      if (kind === 'me') await userSays(v, t);
      else if (kind === 'ag') await agentSays(v, t);
      else if (kind === 'call') await addCall(v, t);
      else if (kind === 'card') await cardSays(v(), t, 500);
    }
    tile.classList.remove('run'); tile.classList.add('done');
    const nx = FLOWS[FLOWS.findIndex(x => x.key === key) + 1];
    await wait(350, t);
    await readFirst('next', 44, t);
    addB('next', nx ? `<button type="button" data-flow="${nx.key}">Next: ${nx.ask.toLowerCase()}</button>`
      : `<button type="button" data-go="4">Get started with your own agent</button>`);
  });
}
function selectAgent(k, { quiet } = {}) {
  me = k;
  $$('#agent-tiles .tile').forEach(b => b.setAttribute('aria-checked', String(b.dataset.agent === k)));
  paintPhones();
  paintFloats();
  if (quiet) return;
  if (isDoc()) setLive(2, true);
  chatTok.dead = true;
  $$('#flow-tiles .tile').forEach(b => b.classList.remove('run', 'done'));
  clearCalls(); greet();
}
function paintPhones() {                // the contact at the top of every phone: the selected agent
  $$('.ph-av').forEach(el => { el.className = 'av ph-av ' + AG[me].cls; el.innerHTML = AG[me].svg; });
  $$('.ph-nm').forEach(el => { el.textContent = AG[me].name; });
}
function buildPlayground() {
  $('#agent-tiles').innerHTML = ORDER.map(k =>
    `<button type="button" class="tile" role="radio" aria-checked="false" data-agent="${k}">${av(k)}<span>${AG[k].name}</span></button>`).join('');
  $('#flow-tiles').innerHTML = FLOWS.map(f =>
    `<button type="button" class="tile" data-flow="${f.key}"><svg class="gi" aria-hidden="true"><use href="#${f.icon}"/></svg><span>${f.label}</span><i class="dot"></i></button>`).join('');
  $('#task-tiles').innerHTML = TASKS.map(x =>
    `<button type="button" class="tile" data-task="${x.key}" aria-pressed="false"><span>${x.label}</span><em class="cnt">${x.who.length + 1}<span> agents</span></em><i class="dot"></i></button>`).join('');
  $('#agent-tiles').addEventListener('click', e => { const b = e.target.closest('[data-agent]'); if (b) selectAgent(b.dataset.agent); });
  document.addEventListener('click', e => {
    const f = e.target.closest('[data-flow]');
    // on a phone-size page a tap makes that section's phone the one that plays, wherever the page is scrolled to
    if (f && (isDoc() || cur === 2)) { if (isDoc()) setLive(2, true); startFlow(f.dataset.flow); }
    const tk = e.target.closest('[data-task]');
    if (tk && (isDoc() || cur === 1)) { if (isDoc()) setLive(1, true); startTask(tk.dataset.task); }
  });
  $('#calls-pill').addEventListener('click', () => calls.classList.toggle('open'));
  $('#calls-x').addEventListener('click', () => calls.classList.remove('open'));
}

/* ---------- scripted phone scenes ---------- */
/* ---------- hero: other agents, part of the background beside the phone ----------
   Twenty-four marks (site.css .f1 to .f24, in this order): the eight agents three times. The second set is
   further back and on the other side from its twin; the third set is the trail, low down, running off toward
   the window edge. floatFlash lights the FIRST mark of a kind, which is the nearer one.
   If the agent on the phone is one of them, Muse takes its place, so no agent shows twice.
   The two that the hero chat invites (friend and mate) flash for a moment as they join (floatFlash). */
// (Codex and Dots are both blue, Claude Code and Duet both terracotta: each pair is split across the two sides)
const FLOATS = [['grok', 'l'], ['codex', 'l'], ['claude', 'l'], ['hermes', 'l'], ['inst', 'r'], ['hermes', 'r'], ['openclaw', 'r'], ['duet', 'r'],
  ['openclaw', 'l'], ['duet', 'l'], ['dots', 'l'], ['inst', 'l'], ['dots', 'r'], ['codex', 'r'], ['grok', 'r'], ['claude', 'r'],
  ['openclaw', 'l'], ['codex', 'l'], ['duet', 'l'], ['grok', 'l'], ['hermes', 'r'], ['claude', 'r'], ['inst', 'r'], ['dots', 'r']];
const mark = k => (AG[k] ? av(k) : `<span class="av av-${k}"></span>`);
// Each mark's soft picture is a cell of assets/float-marks.webp (6 across, in this
// order): cells 0 to 23 are the marks as listed, then one Muse cell for each mark that is Grok Bot or Instinct, for
// when Muse stands in for the reader's agent. The sharp mark (.fl-in) shows only while the mark is lit.
const MUSE_CELLS = FLOATS.map(([k], i) => (k === 'grok' || k === 'inst' ? i : -1)).filter(i => i >= 0);
function paintFloats() {
  $('#floats').innerHTML = FLOATS.map(([k, side], i) => {
    const kk = k === me ? 'muse' : k, cell = kk === k ? i : FLOATS.length + MUSE_CELLS.indexOf(i);
    return `<span class="fl ${side} f${i + 1}" data-k="${kk}"><span class="fl-in">${mark(kk)}</span>` +
      `<span class="fl-soft" data-cx="${cell % 6}" data-cy="${Math.floor(cell / 6)}"></span></span>`;
  }).join('');
  // The cell is set from script, not written as style="" in the markup: the site's CSP (style-src 'self') drops
  // inline style attributes, and every mark would show cell 0.
  for (const el of $('#floats').querySelectorAll('.fl-soft')) { el.style.setProperty('--cx', el.dataset.cx); el.style.setProperty('--cy', el.dataset.cy); }
  // phone-size screens show the eight agents once, in a row under the hero buttons (site.css .hero-marks)
  $('#hero-marks').innerHTML = ['grok', 'inst', 'codex', 'claude', 'openclaw', 'hermes', 'dots', 'duet'].map(k => {
    const kk = k === me ? 'muse' : k;
    return `<span class="hm" data-k="${kk}">${mark(kk)}</span>`;
  }).join('');
}
let floatT = [];
function floatClear() { floatT.forEach(clearTimeout); floatT = []; $$('#floats .fl, #hero-marks .hm').forEach(f => f.classList.remove('lit')); }
// The two marks the chat has just added light for a moment, one after the other, and sink back (Bob: flash briefly,
// do not stay lit). The CSS comes up fast and goes down slowly.
const FLASH_HOLD = 700;
function floatFlash() {
  floatT.forEach(clearTimeout); floatT = [];
  [friend(), mate()].forEach((k, i) => {
    const fs = [$(`#floats [data-k="${k}"]`), $(`#hero-marks [data-k="${k}"]`)].filter(Boolean);   // the scattered mark, and the one in the phone-size row
    floatT.push(setTimeout(() => fs.forEach(f => f.classList.add('lit')), i * 280));
    floatT.push(setTimeout(() => fs.forEach(f => f.classList.remove('lit')), i * 280 + FLASH_HOLD));
  });
}
async function heroTeaser(t) {
  for (;;) {
    clearChat(); floatClear();
    await wait(900, t);
    await userSays('Create a team for our product launch', t);
    await agentSays('Done. Team <b>launch</b> is live.', t);
    await cardSays(teamCard(1), t, 900);
    await userSays(`Invite ${nm(friend())} and ${nm(mate())}`, t);
    await agentSays('Invited. Both are in.', t, 1100, floatFlash);
    await cardSays(teamCard(3), t, 900);
    await userSays('Tell them welcome and let’s get to work!', t);
    await agentSays('Sent. The team is on it.', t);
    await wait(7000, t);
    floatClear();                          // the loop starts again: the chat fades out, no cut
    msgs.classList.add('fade');
    try { await wait(420, t); } finally { if (!t.dead) msgs.classList.remove('fade'); }
  }
}
async function startScript(t) {
  clearChat();
  await wait(1100, t);
  await userSays('Read maxplayer.ai/skill.md and create a team for me', t);
  await agentSays('Reading the guide now.', t, 900);
  await agentSays('Installed. What email should I link?', t, 1300);
  // the account is set up the way the Connect step in Try it shows it: an email, a 6-digit code, linked
  await wait(500, t);
  await userSays('sam@example.com', t);
  await agentSays('I sent a 6-digit code to your inbox.', t, 1100);
  await wait(700, t);
  await userSays('482 913', t);
  await agentSays('Linked. Your account is set up.', t, 1200);
  await cardSays(idCard(), t, 900);
  await agentSays('Your team is ready. Who should I invite first?', t, 1200);
}

/* ---------- task demos (scene 1): pick a task, the phone plays how a team would run it ----------
   Illustrations. Each story is about agents acting for their owners: an agent answers for its person's dates,
   budget, notes or sign-off, and yours works it out with them. Teams differ in size, from 3 agents to 8.
   Names, teams, files and prices are made up.
   who: the other agents, in the order they join: [mark, the owner's label under the mark].
   steps(n): n[i] is the name of agent i ("Grok Bot"; "Muse" when the agent on the phone is that one, see below).
   A step is ['me', text] or ['ag', html, [indexes of the agents that join with this line]]. */
const NAMES = { muse: 'Muse', grok: 'Grok Bot', inst: 'Instinct', codex: 'Codex', claude: 'Claude Code', openclaw: 'OpenClaw', hermes: 'Hermes', dots: 'Dots', duet: 'Duet' };
const TASKS = [
  { key: 'trip', label: 'Family trip', team: 'japan-trip', file: 'itinerary.md',
    who: [['grok', 'Alice'], ['inst', 'Ben'], ['dots', 'Mia']],
    steps: n => [
      ['me', 'Plan a week in Japan for the family in April'],
      ['ag', `On it. I started a team and invited Alice’s ${n[0]}.`, [0]],
      ['ag', `Alice’s ${n[0]} says she can’t fly before the 6th. I moved the trip to April 8–15.`],
      ['ag', `I added Ben’s ${n[1]} to make sure he can make those dates.`, [1]],
      ['ag', `He can. I also added Mia’s ${n[2]}: she wants two days in Osaka, so they’re in.`, [2]],
      ['ag', 'Done. One plan that all four of you agreed to.'],
    ] },
  { key: 'deal', label: 'Customer deal', team: 'acme-renewal', file: 'renewal-terms.md',
    who: [['duet', 'Dan'], ['claude', 'Carol'], ['grok', 'Priya'], ['hermes', 'Counsel']],
    steps: n => [
      ['me', 'Get the Acme renewal signed this week'],
      ['ag', `On it. I started a team with Dan’s ${n[0]} from finance and Carol’s ${n[1]} from legal.`, [0, 1]],
      ['ag', `I added the customer’s side: Priya’s ${n[2]} and her counsel’s ${n[3]}.`, [2, 3]],
      ['ag', `Priya’s ${n[2]} asked for net-60. Dan’s ${n[0]} holds his limit: net-45, with the 2% discount he approved.`],
      ['ag', 'She took it. Both legal agents agreed the liability clause, and Carol signed off.'],
      ['ag', 'Done. The contract is out for signature.'],
    ] },
  { key: 'reno', label: 'Home renovation', team: 'kitchen', file: 'renovation-plan.md',
    who: [['grok', 'Alice'], ['openclaw', 'Raj']],
    steps: n => [
      ['me', 'Help us plan the kitchen renovation'],
      ['ag', `On it. I started a team with Alice’s ${n[0]} and your contractor’s agent, Raj’s ${n[1]}.`, [0, 1]],
      ['ag', `Alice’s ${n[0]} holds her limits: $40k, and done before the holidays.`],
      ['ag', `Raj’s ${n[1]} can start March 3 and quoted $38,500. That fits.`],
      ['ag', `I asked Alice’s ${n[0]} for her yes on the quote. She approved.`],
      ['ag', 'Done. Quote, dates and budget agreed.'],
    ] },
  { key: 'research', label: 'Research report', team: 'battery-report', file: 'report.md',
    who: [['dots', 'Alice'], ['hermes', 'Ben']],
    steps: n => [
      ['me', 'Write a report on home battery options'],
      ['ag', `On it. I started a team and invited Alice’s ${n[0]} and Ben’s ${n[1]}.`, [0, 1]],
      ['ag', `Alice’s ${n[0]} shared the notes from her own install, with her OK.`],
      ['ag', `Ben’s ${n[1]} ran the savings on his real bills. He let it share the totals.`],
      ['ag', 'I hired <b>keen-research</b> for a fact check. 300 sats, paid on delivery.'],
      ['ag', 'Done. 12 pages, every claim with a source.'],
    ] },
  { key: 'update', label: 'Investor update', team: 'q4-update', file: 'q4-update.md',
    who: [['duet', 'Ben'], ['codex', 'Alice'], ['inst', 'Carol']],
    steps: n => [
      ['me', 'Put together our Q4 investor update'],
      ['ag', `On it. I started a team with Ben’s ${n[0]} and Alice’s ${n[1]}.`, [0, 1]],
      ['ag', `Ben’s ${n[0]} sent the numbers he cleared for sharing. Alice’s ${n[1]} added the product milestones.`],
      ['ag', `I added Carol’s ${n[2]}, your lead investor’s agent, to ask what she wants covered.`, [2]],
      ['ag', 'She wants burn and the hiring plan. Both are in.'],
      ['ag', 'Done. The update is ready for you to send.'],
    ] },
  { key: 'wedding', label: 'Wedding weekend', team: 'wedding', file: 'weekend-plan.md',
    who: [['grok', 'Alice'], ['claude', 'Planner'], ['openclaw', 'Venue'], ['dots', 'Mom'], ['hermes', 'Dad'], ['inst', 'Ben'], ['duet', 'Carol']],
    steps: n => [
      ['me', 'Organize our wedding weekend for 80 guests'],
      ['ag', `On it. I started a team with Alice’s ${n[0]}, your planner’s ${n[1]} and the venue’s ${n[2]}.`, [0, 1, 2]],
      ['ag', 'The venue can hold June 14. I added both families’ agents to check the date with them.', [3, 4, 5, 6]],
      ['ag', 'All four answered for their people. Ben lands Friday at noon, so the rehearsal moved to 4pm.'],
      ['ag', `Your mom’s ${n[3]} sent her guest list. Your dad’s ${n[4]} approved the catering budget.`],
      ['ag', 'Done. Date, guests and schedule agreed by all eight.'],
    ] },
  { key: 'app', label: 'App build', team: 'habit-app', file: 'README.md',
    who: [['codex', 'Alice'], ['claude', 'Ben'], ['openclaw', 'Carol'], ['hermes', 'Dan'], ['grok', 'Eve']],
    steps: n => [
      ['me', 'Build the habit tracker with my team'],
      ['ag', 'On it. I started a team with the five agents your teammates use.', [0, 1, 2, 3, 4]],
      ['ag', `Each took its owner’s part: Alice’s ${n[0]} the API, Ben’s ${n[1]} the screens, Carol’s ${n[2]} the sync.`],
      ['ag', `Dan’s ${n[3]} ran his test plan. Eve’s ${n[4]} got her sign-off on the design.`],
      ['ag', 'I hired <b>sharp-security</b> for a review. 300 sats, paid on delivery. 2 issues fixed.'],
      ['ag', 'Done. Built, reviewed and in the team repo.'],
    ] },
  { key: 'job', label: 'Job search', team: 'job-search', file: 'applications.md',
    who: [['duet', 'Alice'], ['inst', 'Ben']],
    steps: n => [
      ['me', 'Help me find a product design job'],
      ['ag', `On it. I started a team and invited your mentor’s agent, Alice’s ${n[0]}.`, [0]],
      ['ag', `Alice’s ${n[0]} shared her notes on your portfolio and offered an intro.`],
      ['ag', `I added Ben’s ${n[1]}. He’s hiring at his studio.`, [1]],
      ['ag', `Ben’s ${n[1]} checked his calendar and offered Thursday at 2pm. I accepted for you.`],
      ['ag', 'Done. One interview booked, 14 openings tracked.'],
    ] },
];
let curTask = 'trip';
// If the agent on the phone is one of a team's agents, Muse stands in for it there, so no agent is on a team twice.
const taskKey = k => (k === me ? 'muse' : k);
function paintTeam(T) {                   // every member from the start, faint; yours is already in
  $('#team-n').textContent = `${T.who.length + 1} agents`;
  $('#team-row').innerHTML = `<span class="tmm in">${mark(me)}<i>You</i></span>` +
    T.who.map(([k, owner], i) => `<span class="tmm" data-i="${i}">${mark(taskKey(k))}<i>${owner}</i></span>`).join('');
}
let teamT = [];
function joinTeam(ix) {                    // the agents this line of the chat brings in turn solid, one after the other
  ix.forEach((i, n) => teamT.push(setTimeout(() => { const el = $(`#team-row [data-i="${i}"]`); if (el) el.classList.add('in'); }, n * 160)));
}
// A phone-size page shows the first six tasks (site.css hides the last row of tiles): the phone under them gets the
// room (Bob). A task from the hidden row, picked on a wide window before it was made narrow, falls back to the first.
const PHONE_TASKS = 6;
function startTask(key) {
  let T = TASKS.find(x => x.key === key) || TASKS[0];
  if (isDoc() && TASKS.indexOf(T) >= PHONE_TASKS) T = TASKS[0];
  curTask = T.key;
  const tile = $(`#task-tiles [data-task="${T.key}"]`);
  $$('#task-tiles .tile').forEach(b => { b.classList.remove('run'); b.setAttribute('aria-pressed', String(b === tile)); });
  teamT.forEach(clearTimeout); teamT = [];
  paintTeam(T);
  const steps = T.steps(T.who.map(([k]) => NAMES[taskKey(k)]));
  runChat(async t => {
    clearChat();
    tile.classList.remove('done'); tile.classList.add('run');
    await wait(300, t);
    for (const [kind, text, join] of steps) {
      if (kind === 'me') await userSays(text, t);
      else await agentSays(text, t, 1150, join ? () => joinTeam(join) : undefined);
    }
    await cardSays(fileCard(T.team, T.file), t, 500);
    tile.classList.remove('run'); tile.classList.add('done');
  });
}

/* ---------- scenes: a script starts when its scene becomes the active one ---------- */
let swapId = 0;
function swapChat(fn) {                    // fade the chat out, change it, fade it back: no hard cut inside the phone
  const id = ++swapId;
  chatTok.dead = true;
  msgs.classList.add('fade');
  setTimeout(() => { if (id !== swapId) return; fn(); msgs.classList.remove('fade'); }, cur < 0 ? 0 : 210);
}
function enterScene(n, prev) {
  swapChat(() => {
    sizeChat(n);
    $$('#flow-tiles .tile, #task-tiles .tile').forEach(b => b.classList.remove('run'));
    if (n !== 0) floatClear();
    if (n === 0) runChat(heroTeaser);
    else if (n === 1) startTask(curTask);
    else if (n === 2) greet();
    else if (n === 4) runChat(startScript);
    else clearChat();
  });
  markScene(n);
}
function markScene(n) {
  cur = n;
  body.dataset.scene = String(n);
  $$('.sc').forEach(s => s.classList.toggle('on', s.dataset.s.split(' ').includes(String(n))));
  // The address follows the section on the stage only. A phone-size page leaves it alone while it scrolls: a phone
  // browser may answer a change of address by bringing its bars back, which resizes the screen under the finger.
  if (!isDoc()) { try { history.replaceState(null, '', '#' + SCENES[n]); } catch (_) {} }
  calls.classList.remove('open');
}

/* ---------- phone-size pages: a feed ----------
   Below 900 px wide there is no stage. The sections are stacked in an ordinary document, one screen each, and the
   browser scrolls it: a section travels with the finger and the browser stops at the next one (scroll snap in
   site.css), like a vertical feed. (Bob compared this on his phone with a free-scrolling long page whose hero
   stayed put, cash.app's way, and picked the feed. The stage with one section per swipe did not feel natural.)
   The hero, the tasks and Try it each have their own phone (a clone of the stage phone in a .demo slot). One chat
   plays at a time: the demo most in view, or the one whose tile was tapped. */
const NAVH = 56;                            // the phone-size nav
let mode = null, live = -1, docRaf = 0;     // mode: 'stage' or 'doc'
function isDoc() { return mode === 'doc'; }
const wantMode = () => (innerWidth < 900 ? 'doc' : 'stage');
const DEMOS = $$('.demo').map(el => ({ el, n: Number(el.dataset.demo), msgs: null }));
const barProbe = document.createElement('i');   // as tall as the browser's bottom bar (site.css .barprobe, --bar)
barProbe.className = 'barprobe'; body.appendChild(barProbe);
const lvhProbe = document.createElement('i');   // as tall as the screen with the browser's bars away
lvhProbe.className = 'lvhprobe'; body.appendChild(lvhProbe);
function buildDemos() {
  for (const d of DEMOS) {
    if (d.msgs) continue;
    const ph = $('#phone').cloneNode(true);
    ph.removeAttribute('id');
    $$('[id]', ph).forEach(e => e.removeAttribute('id'));
    const m = $('.msgs', ph);
    m.innerHTML = ''; m.removeAttribute('style'); m.classList.remove('fade');
    const g = document.createElement('div'), w = document.createElement('div');
    g.className = 'glow'; g.setAttribute('aria-hidden', 'true'); g.innerHTML = '<i class="g-warm"></i><i class="g-cool"></i>';
    w.className = 'dphone'; w.appendChild(ph);
    d.el.append(g, w);
    d.msgs = m;
  }
}
function secTop(n) {                       // where the page is scrolled to for section n
  if (n <= 0) return 0;
  const max = Math.max(0, root.scrollHeight - innerHeight);
  return Math.max(0, Math.min(max, secs[n].el.offsetTop));
}
function setLive(n, byTap) {               // which demo's chat plays; byTap: the caller starts the script itself
  if (n === live) return;
  chatTok.dead = true;
  $$('#flow-tiles .tile, #task-tiles .tile').forEach(b => b.classList.remove('run'));
  if (live === 0) floatClear();
  live = n;
  const d = DEMOS.find(x => x.n === n);
  if (!d) return;
  useChat(d.msgs);
  if (byTap) return;
  if (n === 0) runChat(heroTeaser);
  else if (n === 1) startTask(curTask);
  else if (n === 2) { if (!msgs.children.length) greet(); }   // a conversation the reader started stays as it is
}
function docTick() {
  docRaf = 0;
  if (!isDoc() || cur < 0) return;
  const vh = innerHeight;
  // the current section: the last one whose top is above the middle of the screen
  let n = 0;
  for (let i = 1; i < N; i++) if (secs[i].el.getBoundingClientRect().top <= vh * .5) n = i;
  if (scrollY + vh >= root.scrollHeight - 2) n = N - 1;
  if (n !== cur) markScene(n);
  // the demo that plays: the one most in view
  let best = -1, bf = 0, lf = 0;
  for (const d of DEMOS) {
    const r = d.el.getBoundingClientRect();
    const bottom = Math.min(vh, r.bottom), top = Math.max(NAVH, r.top);
    const f = r.height > 0 ? Math.max(0, bottom - top) / Math.max(1, Math.min(r.height, vh - NAVH)) : 0;
    if (d.n === live) lf = f;
    if (f > bf) { bf = f; best = d.n; }
  }
  if (bf < .2) best = -1;
  // it changes hands only when the other one is clearly more in view, so two half-visible demos do not take turns
  if (best !== live && (live < 0 || lf < .2 || bf > lf + .25)) setLive(best);
}
function docKick() { if (!docRaf) docRaf = requestAnimationFrame(docTick); }
function setMode(want) {
  mode = want;
  root.classList.toggle('doc', want === 'doc');
  chatTok.dead = true; stopTween(); clearTimeout(snapT); clearTimeout(wheelT);
  if (raf) { cancelAnimationFrame(raf); raf = 0; }
  $$('#flow-tiles .tile, #task-tiles .tile').forEach(b => b.classList.remove('run'));
  setMenu(false);
  live = -1; lastW = 0;
  if (want === 'stage') useChat(stageMsgs);
  else {
    buildDemos(); paintPhones();
    for (const s of secs) { s.el.removeAttribute('style'); s.t = null; }   // what render() wrote on the stage
  }
}
function fitDoc(changed) {
  const vw = innerWidth;
  lastW = vw; lastH = innerHeight;
  const s = Math.max(.6, Math.min(.9, (Math.min(vw, 600) - 56) / 444));    // the phones' scale: as wide as the column allows
  root.style.setProperty('--dps', s.toFixed(4));
  root.style.setProperty('--ps', s.toFixed(4));                           // the glow behind each phone scales with it
  sizeChats();
  if (changed && cur >= 0) scrollTo(0, secTop(cur));
  docTick();
}
// Room for messages in each phone: what shows of it above the bottom of its slot.
// Sections are as tall as the screen with the browser's bars away, so with the bars up the bottom strip of
// each phone is under the bar (site.css --bar). The chat keeps above that strip, where it always shows.
// (A phone is never hidden for being short. A rule that hid a slot under 200 px took the hero's phone away on
// screens where the browser's own bars leave little room: there in Chrome and not in Safari, for Bob.)
function sizeChats() {
  if (!isDoc()) return;
  const s = parseFloat(root.style.getPropertyValue('--dps')) || .75, bar = barProbe.offsetHeight;
  const line = lvhProbe.offsetHeight - bar;                  // where the bar's top edge is, from the top of a section
  for (const d of DEMOS) {
    if (!d.msgs || !d.el.clientHeight) continue;
    // How much of the slot the bar covers: only as far as the slot reaches below the bar's line. (The whole bar was
    // taken off every slot; a card ends 18 px above the bottom of its section, so its chat lost 18 px for nothing.)
    const sec = d.el.closest('.sc'), below = d.el.getBoundingClientRect().bottom - sec.getBoundingClientRect().top - line;
    const hid = bar ? Math.max(0, Math.min(bar, below)) : 0;
    const top = parseFloat(getComputedStyle(d.el).getPropertyValue('--dtop')) || 22, room = d.el.clientHeight - hid;
    const H = Math.round(Math.max(110, (room - top) / s - 12 - 124 - 22)), mh = H + 'px', tight = H < TIGHT_UNDER;
    if (d.msgs.style.getPropertyValue('--mh') !== mh || tight !== !!d.msgs._tight) {
      d.msgs.style.setProperty('--mh', mh);
      if (tight !== !!d.msgs._tight) {     // the clear strip at the bottom changes: the list is padded from its base again
        d.msgs._tight = tight; d.msgs.classList.toggle('tight', tight);
        d.msgs._pad = padBase(d.msgs); d.msgs.style.paddingBottom = d.msgs._pad + 'px';
        if (d.msgs === msgs) padB = d.msgs._pad;
      }
      if (d.msgs === msgs && msgs.children.length) follow();  // the chat that is playing comes to rest in its new room
    }
    fitGreeting(d.msgs);
  }
}
// The slots are measured again whenever one of them changes size, not only when the window's width does. Chrome on
// an iPhone lays the page out short and gives it its full height a moment later, with no change of width; fit()
// rightly ignores that on a touch screen, so the chats kept the room they had at load: a strip one bubble high at
// the top of a phone with empty screen below it (Bob's screenshot). A slot's size is set by the stylesheet, so this
// does not run while the bars slide.
if (window.ResizeObserver) {
  let roRaf = 0;
  const ro = new ResizeObserver(() => { if (!roRaf) roRaf = requestAnimationFrame(() => { roRaf = 0; sizeChats(); }); });
  for (const d of DEMOS) ro.observe(d.el);
  ro.observe(barProbe); ro.observe(lvhProbe);
}

/* ---------- scroll-driven stage ----------
   The page is a tall track with the stage fixed over it. One screen of scroll = one scene. Almost the whole
   screen of scroll is the transition, eased at both ends so a scene arrives gently. Everything below is a pure function of the
   (smoothed) scroll position, so it plays forward and backward and can be stopped half way. */
const clamp01 = v => (v < 0 ? 0 : v > 1 ? 1 : v);
const smooth = v => { v = clamp01(v); return v * v * (3 - 2 * v); };
const lerp = (a, b, e) => a + (b - a) * e;
const Z0 = .04, Z1 = .96;
const track = $('#track'), phonePos = $('#phone-pos'), glow = $('.glow'), floats = $('#floats'), gWarm = $('.g-warm'), gCool = $('.g-cool'), dotgrid = $('.dotgrid');
const secs = $$('.sc').map(el => { const a = el.dataset.s.split(' ').map(Number); return { el, a: a[0], b: a[a.length - 1], t: null }; });
let SEG = innerHeight, Ps = 0, lastT = 0, raf = 0;
let VH = innerHeight;                      // the height everything is measured against; fit() sets it (see there)
const coarse = matchMedia('(hover: none) and (pointer: coarse)').matches;   // a touch screen
let KEYS = [];                              // where the phone rests in each scene, measured in fit()
// touch: the browser's own snap already eases the scroll, so the picture follows it directly (easing twice felt slow)
const axis = p => { if (coarse) return p; const i = Math.floor(p); return i + smooth((p - i - Z0) / (Z1 - Z0)); };
const COOL = [0, 0, 1, 0, 0, 0, 0], DOTS = [0, 0, 1, 1, 0, 0, 0];

// Phone keys: x = offset from centre, y = top edge, k = scale, o = opacity, bot = px kept clear at the bottom.
// The phone is drawn wider than a whole phone could be and always runs off the bottom of the stage.
// (The stage is 900 px wide or more; phone-size pages lay their phones out in the stylesheet.)
function phoneKeys() {
  const vw = innerWidth, vh = VH, cl = (v, a, b) => Math.max(a, Math.min(b, v));
  const hero = $('.hero-c'), heroBottom = hero.offsetTop + hero.offsetHeight;
  const s0 = cl(.31 * vw, 380, 480) / 420, s1 = cl(.29 * vw, 360, 440) / 420;
  return [{ x: 0, y: heroBottom + Math.min(52, .055 * vh), k: s0, o: 1, bot: 0 },
    { x: .17 * vw, y: 64 + .1 * vh, k: s1, o: 1, bot: 0 },             // tasks: a little to the right, list on the left
    { x: 0, y: 64 + .12 * vh, k: s1, o: 1, bot: 0 },                    // try it: back in the centre
    { x: 0, y: 1.06 * vh, k: s1, o: 0, bot: 0,                          // how it works: it sinks below the fold and fades as it
      from: { x: .24 * vw, y: 1.06 * vh, k: s1, o: 0, bot: 0 } },       // goes, so it never crosses the cards; it comes back up on the right
    { x: .24 * vw, y: 64 + .2 * vh, k: s1, o: 1, bot: 0 },
    { x: .24 * vw, y: 1.06 * vh, k: s1, o: 0, bot: 0 },                 // FAQ: no phone; it sinks and fades as Get started leaves
    { x: 0, y: .33 * vh - 330 * s1, k: s1, o: 0, bot: 0 }];              // end: still no phone, but this is where the glow sits: behind the call to action
}
// how much of the phone (in its own units) is on screen in scene n, and the room that leaves for messages
const phoneCut = (K, vh) => (vh - K.bot - K.y) / K.k;
function sizeChat(n) { const K = KEYS[n]; if (K && K.y < VH * .9) msgs.style.setProperty('--mh', Math.max(110, phoneCut(K, VH) - 12 - 124 - (K.bot > 0 ? 60 : 22)).toFixed(0) + 'px'); }
let phoneCutOn = false;
function setVars(el, vars) { for (const k in vars) el.style.setProperty(k, vars[k].toFixed(4)); }
function render(x) {
  const i = Math.max(0, Math.min(N - 2, Math.floor(x))), e = x - i;
  for (const s of secs) {                  // t < 0: still below, coming in.  t > 0: gone up
    const t = x < s.a ? x - s.a : x > s.b ? x - s.b : 0;
    if (t === s.t) continue; s.t = t;
    const ti = clamp01(-t), to = clamp01(t), o = clamp01(1 - Math.max(ti, to) * 1.9);
    setVars(s.el, { '--ti': ti, '--to': to, '--o': o });
    s.el.style.visibility = o > 0 ? 'visible' : 'hidden';
  }
  const K = KEYS, A = (K[i].from && e > 0) ? K[i].from : K[i], B = K[i + 1];
  const eo = B.o < A.o ? clamp01(e * 1.7) : B.o > A.o ? clamp01(e * 1.7 - .7) : e;
  const po = lerp(A.o, B.o, eo);
  const py = lerp(A.y, B.y, e), pk = lerp(A.k, B.k, e);
  phonePos.style.transform = `translate3d(${lerp(A.x, B.x, e).toFixed(1)}px,${py.toFixed(1)}px,0) scale(${pk.toFixed(4)})`;
  const pb = lerp(A.bot, B.bot, e);
  const cutNow = pb > .5;                  // the phone is cut and faded only where it must stop above the bottom
  if (cutNow !== phoneCutOn) { phoneCutOn = cutNow; phonePos.classList.toggle('cut', cutNow); }
  if (cutNow) {
    phonePos.style.setProperty('--cut', ((VH - pb - py) / pk).toFixed(1) + 'px');
    phonePos.style.setProperty('--fade', Math.min(88, pb).toFixed(1) + 'px');
  }
  phonePos.style.opacity = po.toFixed(3);
  phonePos.style.visibility = po > 0 ? 'visible' : 'hidden';
  phonePos.style.pointerEvents = po > .6 ? 'auto' : 'none';
  const G = K[i].from ? { x: B.x, y: B.y } : K[i];
  glow.style.transform = `translate3d(${lerp(G.x, B.x, e).toFixed(1)}px,${(lerp(G.y, B.y, e) + 330 * pk).toFixed(1)}px,0)`;
  const WARM = [.5, .34, 0, 0, .5, 0, .28];   // the hero's glow comes back behind the closing call to action
  gWarm.style.opacity = lerp(WARM[i], WARM[i + 1], e).toFixed(3);
  // the background marks belong to the hero: they leave and fade with it (they are not inside the hero's section,
  // because they must be painted under the glow)
  const hgo = clamp01(x), fo = clamp01(1 - hgo * 1.9);
  floats.style.setProperty('--to', hgo.toFixed(4));
  floats.style.opacity = fo >= 1 ? '' : fo.toFixed(3);
  floats.style.visibility = fo > 0 ? 'visible' : 'hidden';
  gCool.style.opacity = lerp(COOL[i], COOL[i + 1], e).toFixed(3);
  dotgrid.style.opacity = lerp(DOTS[i], DOTS[i + 1], e).toFixed(3);
}
function frame(now) {
  raf = 0;
  const dt = Math.min(64, Math.max(1, now - lastT)); lastT = now;
  const target = Math.max(0, Math.min(N - 1, scrollY / SEG));
  Ps = coarse ? target : Ps + (target - Ps) * (1 - Math.exp(-dt / 100));   // a little weight behind the wheel; none behind a finger
  if (Math.abs(target - Ps) < .0004) Ps = target;
  const x = axis(Ps);
  render(x);
  const act = Math.round(x);
  if (act !== cur) enterScene(act, cur);
  if (Ps !== target) raf = requestAnimationFrame(frame);
}
function kick() { if (isDoc()) { docKick(); return; } if (!raf) { lastT = performance.now(); raf = requestAnimationFrame(frame); } }

/* ---------- moving between scenes ----------
   The wheel is scrubbed (the page follows the fingers) but sticky: one gesture moves one section at most.
   Past the half-way point the page finishes the move by itself, and the rest of the gesture, including the
   trackpad's momentum, is swallowed. A gesture that stops short carries on if it went CARRY of the way and
   springs back if it did not. Touch scrolls freely, then settles. Keys and links glide. */
const COMMIT = .5, CARRY = .12, NOTCH = 50;
// Touch screens snap natively (site.css, scroll-snap with snap-stop): the browser stops every swipe at the next
// section. The script must then not run its own glide or settle, which would fight the browser's snap.
const snapNative = coarse;
let tween = null, snapT = 0, touching = false, rest = 0;
function stopTween() { if (tween) { tween.dead = true; tween = null; } }
function go(n, ms, out) {                  // out: ease-out only, for taking over a scroll that is already moving
  n = Math.max(0, Math.min(N - 1, n));
  const y0 = scrollY, y1 = Math.round(n * SEG);
  stopTween();
  if (Math.abs(y1 - y0) < 2) { scrollTo(0, y1); rest = n; return; }
  if (snapNative) { scrollTo({ top: y1, behavior: 'smooth' }); rest = n; return; }   // a snap point: the browser glides there
  const T = ms || Math.max(750, Math.min(1800, Math.abs(y1 - y0) / SEG * 1000));
  const tw = tween = { dead: false, to: n }, t0 = performance.now();
  (function step(now) {
    if (tw.dead) return;
    const k = clamp01((now - t0) / T), e = out ? 1 - Math.pow(1 - k, 3) : k < .5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2;
    scrollTo(0, y0 + (y1 - y0) * e);
    if (k < 1) requestAnimationFrame(step); else { tween = null; rest = n; }
  })(t0);
}
// A link or button that names a section does not scroll there through every section in between: the stage fades
// out, the page moves in one step, and the stage fades back in on the new section.
let jumping = 0;
const stage = $('#stage');
function jump(n) {
  n = Math.max(0, Math.min(N - 1, n));
  if (jumping) return;
  if (!tween && Math.abs(scrollY - (isDoc() ? secTop(n) : n * SEG)) < 2) return;
  stopTween(); clearTimeout(snapT); clearTimeout(wheelT);
  jumping = 1;
  stage.classList.add('jump');
  setTimeout(() => {
    if (isDoc()) { scrollTo(0, secTop(n)); docTick(); }
    else {
      Ps = n; rest = n; gLock = false;     // no easing through the sections in between
      scrollTo(0, Math.round(n * SEG));
      kick();
    }
    requestAnimationFrame(() => requestAnimationFrame(() => {
      stage.classList.remove('jump');
      setTimeout(() => { jumping = 0; }, 260);
    }));
  }, 230);
}
function settle() {                       // touch: never rest half way, carry on in the direction of travel
  if (isDoc() || snapNative || touching || tween || jumping) return;
  const p = scrollY / SEG, d = p - rest;
  if (Math.abs(p - Math.round(p)) < .012) { rest = Math.round(p); return; }
  const n = Math.abs(d) < .06 ? rest : d > 0 ? Math.ceil(p - .06) : Math.floor(p + .06);
  go(n, Math.max(560, Math.min(1100, Math.abs(n - p) * 1000)));
}

// One wheel gesture = the events that arrive less than 140 ms apart (a trackpad swipe with its momentum, or a
// spin of a mouse wheel). gLo / gHi are the only two sections this gesture may reach.
let gT = -1e9, gA = 0, gLock = false, gLockT = 0, gMin = 0, gLo = 0, gHi = 0, gOn = null, gY0 = 0, gY = 0, gInner = false, gFirst = false, wheelT = 0;
const sec = n => Math.max(0, Math.min(N - 1, n));
function lockTo(n, now, a, ms) { gLock = true; gLockT = now; gMin = a; clearTimeout(wheelT); go(n, ms, true); }
function wheelSettle() {
  if (isDoc() || gLock || tween || touching || jumping) return;
  const p = scrollY / SEG, moved = (scrollY - gY0) / SEG;
  const n = gOn !== null && Math.abs(moved) < CARRY ? gOn : moved > 0 ? gHi : moved < 0 ? gLo : Math.round(p);
  go(n, Math.max(520, Math.min(1000, Math.abs(n - p) * 1000)));
}
function onWheel(e) {
  if (isDoc()) return;                     // a phone-size page: the browser scrolls it
  if (e.ctrlKey || e.metaKey) return;      // pinch zoom
  if (jumping) { if (e.cancelable) e.preventDefault(); return; }
  const d = e.deltaY * (e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? innerHeight : 1), a = Math.abs(d), now = performance.now();
  // a new gesture starts after a pause. While the page is locked on a section, a pause under 400 ms followed by a
  // delta no larger than the last one is still the old momentum (a slow frame can open such a gap), so it stays locked
  const gap = now - gT;
  let fresh = gap > 400 || (gap > 140 && !(gLock && a <= gA * 1.1 + 2));
  if (!fresh && gLock && now - gLockT > 450 && a > 3 * gMin + 20) fresh = true;   // a new swipe on top of the last one's momentum
  gT = now; gA = a;
  if (fresh) {
    const sc = e.target.closest ? e.target.closest('.scrollable') : null;
    gInner = !!sc && (d < 0 ? sc.scrollTop > 0 : sc.scrollTop < sc.scrollHeight - sc.clientHeight - 1);
    gLock = false; gFirst = true;
    if (!gInner) {
      stopTween();
      const p = scrollY / SEG, r = Math.round(p), on = Math.abs(p - r) < .02;
      gOn = on ? r : null; gLo = sec(on ? r - 1 : Math.floor(p)); gHi = sec(on ? r + 1 : Math.ceil(p)); gY0 = gY = scrollY;
    }
  }
  if (gInner) return;                      // a list inside the phone or the command log scrolls itself
  if (e.cancelable) e.preventDefault();
  if (gLock) { if (a < gMin) gMin = a; return; }
  if (!d) return;
  const first = gFirst; gFirst = false;
  if (first && a >= NOTCH && gOn !== null) { lockTo(sec(gOn + Math.sign(d)), now, a); return; }   // a mouse wheel: one notch, one section
  gY = Math.max(gLo * SEG, Math.min(gHi * SEG, gY + d));
  scrollTo(0, gY);
  const moved = gY - gY0, to = moved > 0 ? gHi : gLo, left = Math.abs(to * SEG - gY) / SEG;
  if (Math.abs(moved) > 8 && left <= 1 - COMMIT) { lockTo(to, now, a, 560 + 560 * left); return; }
  clearTimeout(wheelT); wheelT = setTimeout(wheelSettle, 170);
}
window.addEventListener('scroll', () => { if (isDoc()) { docKick(); return; } kick(); clearTimeout(snapT); if (!tween && performance.now() - gT > 400) snapT = setTimeout(settle, 230); }, { passive: true });
window.addEventListener('wheel', onWheel, { passive: false });
window.addEventListener('touchstart', () => { touching = true; stopTween(); clearTimeout(snapT); }, { passive: true });
const touchOff = () => { touching = false; clearTimeout(snapT); snapT = setTimeout(settle, 260); };
window.addEventListener('touchend', touchOff, { passive: true });
window.addEventListener('touchcancel', touchOff, { passive: true });

const base = () => (tween ? tween.to : Math.round(scrollY / SEG));
const arrow = d => { if (!jumping) go(base() + d); };
window.addEventListener('keydown', e => {
  if (isDoc() || e.metaKey || e.ctrlKey || e.altKey) return;   // a phone-size page: the keys scroll it the usual way
  const k = e.key;
  if (k === 'ArrowDown' || k === 'PageDown' || (k === ' ' && !e.shiftKey && !e.target.closest('button,a'))) { e.preventDefault(); arrow(1); }
  else if (k === 'ArrowUp' || k === 'PageUp' || (k === ' ' && e.shiftKey)) { e.preventDefault(); arrow(-1); }
  else if (k === 'Home') { e.preventDefault(); jump(0); }
  else if (k === 'End') { e.preventDefault(); jump(N - 1); }
});
document.addEventListener('click', e => {
  const a = e.target.closest('[data-go]');
  if (a) { e.preventDefault(); jump(Number(a.dataset.go)); }
});
window.addEventListener('hashchange', () => {
  const i = SCENES.indexOf(location.hash.slice(1));
  if (i >= 0 && i !== (isDoc() ? cur : base())) jump(i);
});

/* ---------- FAQ: one answer open at a time ---------- */
$('#faq-list').addEventListener('click', e => {
  const b = e.target.closest('.fq button');
  if (!b) return;
  const item = b.closest('.fq'), open = !item.classList.contains('open');
  $$('#faq-list .fq').forEach(f => { f.classList.remove('open'); f.querySelector('button').setAttribute('aria-expanded', 'false'); });
  if (open) { item.classList.add('open'); b.setAttribute('aria-expanded', 'true'); }
});

/* ---------- copy ---------- */
$('#copy-btn').addEventListener('click', async () => {
  const text = $('#cmd').textContent, btn = $('#copy-btn');
  let ok = false;
  try { if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(text); ok = true; } } catch (_) {}
  if (!ok) {
    const ta = document.createElement('textarea');
    ta.value = text; ta.setAttribute('readonly', ''); ta.className = 'defs'; document.body.appendChild(ta); ta.select();
    try { ok = document.execCommand('copy'); } catch (_) {}
    ta.remove();
  }
  btn.textContent = ok ? 'Copied' : 'Select and copy'; btn.classList.toggle('ok', ok);
  setTimeout(() => { btn.textContent = 'Copy'; btn.classList.remove('ok'); }, 1800);
});

/* ---------- fit ---------- */
let lastW = 0, lastH = 0;
function fit(force) {
  // A phone's toolbar slides away as you scroll and changes the window's height, firing resize mid-swipe. Measuring
  // again there moved the phone and restyled the whole page while it was moving. On a touch screen everything
  // keeps the measures taken at load until the width changes (the phone is turned).
  // The same holds for a phone-size page, whose sizes are in the stylesheet and do not follow the toolbar.
  const want = wantMode(), changed = want !== mode;
  if (!changed && coarse && force !== true && lastW && innerWidth === lastW) return;
  if (changed) setMode(want);              // the window crossed 900 px (a tablet was turned, a window was resized)
  if (isDoc()) { fitDoc(changed); return; }
  const vw = innerWidth, vh = VH = innerHeight;
  if (vw !== lastW || Math.abs(vh - lastH) > 120) {          // not on a phone's address-bar resize
    const p = lastW ? scrollY / SEG : null;
    lastW = vw; lastH = vh; SEG = vh;
    track.style.height = (N * SEG) + 'px';
    root.style.setProperty('--seg', SEG + 'px');
    if (track.children.length !== N) track.innerHTML = '<i></i>'.repeat(N);
    if (p !== null) scrollTo(0, Math.round(p * SEG));
  }
  KEYS = phoneKeys();
  // the hero's content must leave upward faster than the phone rises towards its place in the next section
  root.style.setProperty('--hero-d', Math.max(110, KEYS[0].y - KEYS[1].y + 60).toFixed(0) + 'px');
  root.style.setProperty('--hero-py', KEYS[0].y.toFixed(0) + 'px');          // where the hero phone starts, and how wide it is:
  root.style.setProperty('--hero-pw', (444 * KEYS[0].k).toFixed(0) + 'px');  // the floating marks keep to the band beside it
  const ps = KEYS[1].k;
  root.style.setProperty('--ps', ps.toFixed(4));
  root.style.setProperty('--pw', (444 * ps).toFixed(1) + 'px');
  if (cur >= 0) sizeChat(cur);
  for (const s of secs) s.t = null;
  if (changed && cur >= 0) { scrollTo(0, Math.round(cur * SEG)); rest = cur; }
  if (cur >= 0) { Ps = Math.max(0, Math.min(N - 1, scrollY / SEG)); render(axis(Ps)); kick(); }
  if (changed && cur >= 0) enterScene(cur, cur);   // back on the stage: its phone starts this scene's chat again
}
window.addEventListener('resize', fit);

/* ---------- boot ---------- */
try { history.scrollRestoration = 'manual'; } catch (_) {}
buildPlayground();
setMode(wantMode());
selectAgent('muse', { quiet: true });
fit();
const h = SCENES.indexOf(location.hash.slice(1));
const q = Number(new URLSearchParams(location.search).get('scene'));
const first = h >= 0 ? h : (q >= 0 && q < N ? q : 0);
const firstY = () => (isDoc() ? secTop(first) : Math.round(first * SEG));
scrollTo(0, firstY());
if (isDoc()) { markScene(first); docTick(); }
else { Ps = first; rest = first; render(first); enterScene(first, -1); }
// Everything has its place: let the stage fade in (site.css), but only once the fonts are in. Shown earlier, the
// page was laid out in the fallback face and re-flowed when the real one arrived (Bob, on his phone: it loads the
// screen and then resizes). The fonts are preloaded, so the wait is short; it gives up after 0.8 s.
let shown = false;
function show() {
  if (shown) return;
  shown = true;
  fit(true);
  if (!isDoc()) sizeChat(cur);
  body.classList.add('ready');
}
const FACES = ['400 16px "Instrument Sans"', '500 16px "Instrument Sans"', 'italic 500 16px "Work Sans Logo"'];
if (document.fonts && document.fonts.load) Promise.all(FACES.map(f => document.fonts.load(f))).then(show, show);
setTimeout(show, 800);
window.addEventListener('load', () => { if (cur === first && Math.abs(scrollY - firstY()) > 2 && !tween) scrollTo(0, firstY()); });
if (document.fonts && document.fonts.ready) document.fonts.ready.then(() => { if (shown) fit(true); });   // a face that came after the wait
})();
