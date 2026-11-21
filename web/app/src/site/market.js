/* Maxplayer Market page (/market): what were the Buy, Sell and Market pages, in one page. Bundled by
   scripts/build.mjs through src/market-page.ts into market.js. Plain JavaScript, not yet typed (tsc does not check
   it); the data layer under it, market-data.ts, is typed.

   - The numbers come from market-data.ts, which drives the same reader the old board used (source/, store/,
     market/engine.ts) and hands this file plain data; this file only draws it. Nothing on the board is made up here.
   - Loading (Bob): placeholders until what is asked for is WHOLE, then all of it at once, and no change after. The
     reader gives this page a view only when it is whole for its period: 24 hours and Week about 1.5 s into a first
     visit, All time and an expert's profile when the whole history is in (about 6 s). Until then the numbers and
     the lists are grey bars of the size the real ones will have, so nothing moves when they arrive. `want` is the
     period the reader asked for; the board draws `view` only if it is for that period (see `cur`).
   - The activity list shows the event only (who posted, bid, hired, delivered, paid, and the price). The task text
     is not passed to this page at all (Bob: the first thing a visitor sees must not be someone's raw task text).
   - The chat in the phone moves exactly as on the Teams page. The functions from `chatEnd` to `cardSays` are teams.js's,
     copied for one phone; when the motion changes there, change it here.
   - No style="" attributes in generated HTML: production's CSP is style-src 'self'. Styles set from script are fine. */
import { startMarket } from './market-data.ts';

const $ = (s, r = document) => r.querySelector(s), $$ = (s, r = document) => [...r.querySelectorAll(s)];
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const root = document.documentElement, body = document.body;
const phoneSize = matchMedia('(max-width:899px)');
const narrow = () => phoneSize.matches;
const nf = new Intl.NumberFormat('en-US');
function paint(el, html) { if (el._h !== html) { el._h = html; el.innerHTML = html; } }

/* ---------- nav: the menu on a phone-size page (as on the Teams page) ---------- */
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
window.addEventListener('scroll', () => { if (nav.classList.contains('open') && Math.abs(scrollY - menuY) > 24) setMenu(false); }, { passive: true });

/* ---------- the board ---------- */
const PERIOD = { '24h': 'last 24 hours', week: 'this week', all: 'all time' };
const board = $('#board');
let view = null, state = 'connecting', handle = null, failed = false;
let want = 'week';                        // the period asked for; the buttons in market.html start on the same one
// The view to draw: the reader's last one, if it is for the period asked for. null: placeholders.
const cur = () => (view && view.windowKey === want ? view : null);
let seen = null;                           // activity rows already drawn once; a row that is not in it arrives lit
let sheetTab = null, sheetFrom = null;
let profileKey = null;                    // the expert whose profile is open: an npub or a 64-hex key

// An expert's name is a link to its profile: a real address, so it also opens in a new tab and can be shared.
// A click opens the profile in the panel instead (see openProfile). Buyers have no profile and are plain text.
const who = (w, expert) => (expert && w.npub
  ? `<a class="who${w.named ? '' : ' hex'}" href="?expert=${w.npub}" data-pk="${w.key}">${esc(w.name)}</a>`
  : `<b class="who${w.named ? '' : ' hex'}">${esc(w.name)}</b>`);
const lamp = (cls, label) => `<i class="lamp ${cls}" role="img" aria-label="${label}" title="${label}"></i>`;
const zero = s => `<span class="z">${s}</span>`;
const num = n => (n ? nf.format(n) : zero('0'));
const money = v => (v === '$0' ? zero('$0') : esc(v));
const rate = v => (v == null ? zero('–') : v === '$0' ? 'Free' : esc(v));
function age(at) {
  const s = Math.max(0, Date.now() / 1000 - at);
  return s < 60 ? 'now' : s < 3600 ? `${Math.floor(s / 60)}m` : s < 86400 ? `${Math.floor(s / 3600)}h` : `${Math.floor(s / 86400)}d`;
}
const expertRow = r => `<li class="row g-x go" data-pk="${r.key}"><span class="pos">${r.rank}</span>${r.working ? lamp('busy', 'Working now') : r.online ? lamp('on', 'Online') : lamp('', 'Offline')}<span class="nm">${who(r, true)}</span><span class="num">${num(r.delivered)}</span><span class="num">${rate(r.ask)}</span><span class="num">${money(r.earned)}</span></li>`;
const buyerRow = r => `<li class="row g-b"><span class="pos">${r.rank}</span>${r.working ? lamp('busy', 'Has a job running') : r.active ? lamp('on', 'Active in the last 24 hours') : lamp('', 'Not active in the last 24 hours')}<span class="nm">${who(r)}</span><span class="num">${num(r.posted)}</span></li>`;

// One line per event, in plain words. The words differ from production's board (awarded, claimed, accepted the
// delivery) because this page is for someone who has not seen the market before; the facts are the same.
const EV_LAMP = { offer: 'post', award: 'hire', result: 'done', receipt: 'done' };
function eventText(f) {
  const sells = f.stage === 'claim' || f.stage === 'result' || f.stage === 'feedback';     // the expert acts in these; in the rest the buyer does
  const a = who(f.actor, sells), o = f.other ? who(f.other, !sells) : f.unknownOther ? zero('an undetermined expert') : '';
  const rd = f.turn > 1 ? ` <span class="rd">· round ${f.turn}</span>` : '';
  const amt = f.price ? ` <span class="amt">· ${esc(f.price)}</span>` : '';
  switch (f.stage) {
    case 'offer': return `${a} ${o ? `sent ${o} a job` : 'posted a job'}${f.free ? '<span class="tag">Free</span>' : amt}`;
    case 'claim': return `${a} bid on ${o ? `${o}’s job` : 'a job'}`;
    case 'award': return `${a} hired ${o || 'an expert'}`;
    case 'result': return `${a} delivered${o ? ` to ${o}` : ''}${rd}`;
    case 'accept': return `${a} accepted ${o ? `${o}’s work` : 'the work'}${rd}`;
    case 'receipt': return `${a} paid${o ? ` ${o}` : ''}${amt}${rd}`;
    case 'reply': return f.closed ? `${a} closed the job${o ? ` with ${o}` : ''}` : `${a} sent ${o || 'the expert'} a follow-up${rd}`;
    case 'feedback': return f.terminal ? `${a} stopped work on a job` : `${a} sent an update`;
    default: return a;
  }
}
const eventRow = (f, lit) => `<li class="row g-a${lit ? ' new' : ''}">${lamp(f.stage === 'feedback' && f.terminal ? 'stop' : EV_LAMP[f.stage] || '', f.stage)}<span class="ev">${eventText(f)}</span><span class="age">${age(f.at)}</span></li>`;

const LISTS = {
  experts: { title: 'Experts', rows: v => v.experts, row: expertRow, list: $('#x-rows'), meta: $('#x-meta'), empty: 'No experts seen in this period.',
    all: n => `See all ${n} experts`, say: v => `${v.stats.expertsOnline} online of ${v.stats.expertsSeen}` },
  activity: { title: 'Activity', rows: v => v.feed, row: eventRow, list: $('#a-rows'), meta: $('#a-meta'), empty: 'No activity yet.',
    all: n => `See the latest ${n}`, say: () => 'Newest first' },
  buyers: { title: 'Buyers', rows: v => v.buyers, row: buyerRow, list: $('#b-rows'), meta: $('#b-meta'), empty: 'No buyers in this period.',
    all: n => `See all ${n} buyers`, say: v => `${v.buyers.length} ${PERIOD[v.windowKey] || ''}` },
};
// How many rows a list shows. A wide page: the whole rows that fit its column (styles: 5 to 10 rows). A phone-size
// page scrolls freely and the list is as long as its rows: eight, and "See all" opens the rest.
function fits(el) {
  if (narrow()) return 8;
  const rh = parseFloat(getComputedStyle(el).getPropertyValue('--rh')) || 44;
  return Math.max(1, Math.floor((el.clientHeight + .5) / rh));
}
const skeleton = n => Array.from({ length: n }, (_, i) => `<li class="row sk" aria-hidden="true"><i class="sk${i % 3}"></i></li>`).join('');
// One light for the whole page (market.css `skmove`): each grey bar is told how far it is from the window's left
// edge, so the band is over all of them at the same place at the same time. Called whenever placeholders are drawn
// or may have moved (a resize, another list on a phone-size page, the panel).
function alignSk() {
  for (const e of $$('.row.sk i, .lead dd.sk, .pf-wait i')) {
    if (e.offsetParent === null) continue;
    const x = Math.round(e.getBoundingClientRect().left) + 'px';
    if (e.style.getPropertyValue('--skx') !== x) e.style.setProperty('--skx', x);
  }
}

// The page shows no status lamp (Bob). The state is kept on the board (data-state) for checks; a reader sees
// placeholders until the first rows, and a note in the lists if the market cannot be reached.
function drawStatus() { board.dataset.state = failed ? 'failed' : state; }
const STATS = { 'st-online': s => s.expertsOnline, 'st-delivered': s => s.delivered, 'st-posted': s => s.posted, 'st-buyers': s => s.buyers };
function draw() {
  drawStatus();
  const v = cur(), down = failed || state === 'failed';
  const per = PERIOD[want] || '';
  $$('.per').forEach(e => { if (e.textContent !== per) e.textContent = per; });
  $$('button[data-win]').forEach(b => b.setAttribute('aria-pressed', String(b.dataset.win === want)));
  for (const id in STATS) {               // a grey bar until the number is known; a dash if it cannot be
    const dd = $('#' + id), text = v ? nf.format(STATS[id](v.stats)) : down ? '–' : '';
    if (dd.textContent !== text) dd.textContent = text;
    dd.classList.toggle('sk', !text);
  }
  if (!v) {
    for (const k in LISTS) {
      const L = LISTS[k];
      paint(L.list, down
        ? '<li class="note">The live market is not reachable right now. Try again in a moment.</li>'
        : skeleton(Math.min(fits(L.list), 10)));
      L.meta.textContent = '';
      $(`.more[data-more="${k}"]`).hidden = true;
    }
    if (sheetTab || profileKey) drawSheet();
    alignSk();
    return;
  }
  const fresh = seen ? new Set(v.feed.filter(f => !seen.has(f.id)).map(f => f.id)) : new Set();
  for (const k in LISTS) {
    const L = LISTS[k], rows = L.rows(v), n = fits(L.list);
    paint(L.list, rows.length ? rows.slice(0, n).map(r => L.row(r, fresh.has(r.id))).join('') : `<li class="note">${L.empty}</li>`);
    L.meta.textContent = L.say(v);
    const more = $(`.more[data-more="${k}"]`);
    more.hidden = rows.length <= n;
    more.textContent = L.all(rows.length);
  }
  seen = new Set(v.feed.map(f => f.id));
  if (sheetTab || profileKey) drawSheet();
}
// A period button. The reader answers at once if that period is whole; if it is not (All time in the first seconds
// of a first visit), the board shows placeholders and the reader calls back when it is.
function pickWindow(key) {
  want = key;
  if (handle && handle.setWindow(key) && handle.view()) view = handle.view();
  draw();
}

/* the panel: the full list of a column, or one expert's profile. A dialog on a wide page, a sheet on a phone-size page */
const sheet = $('#sheet'), main = $('.mk-main');
function drawSheet() {
  sheet.dataset.mode = profileKey ? 'expert' : 'list';
  $('#sheet-back').hidden = !(profileKey && sheetTab);
  if (profileKey) { drawProfile(); return; }
  const L = LISTS[sheetTab];
  $('#sheet-t').textContent = L.title;
  const head = L.list.previousElementSibling, ch = $('#sheet-ch');
  ch.className = head.className; paint(ch, head.innerHTML);
  const v = cur();
  if (!v) { paint($('#sheet-rows'), skeleton(10)); $('#sheet-meta').textContent = ''; alignSk(); return; }
  const rows = L.rows(v);
  paint($('#sheet-rows'), rows.length ? rows.map(r => L.row(r, false)).join('') : `<li class="note">${L.empty}</li>`);
  $('#sheet-meta').textContent = L.say(v);
}
function showSheet() {
  root.classList.add('sheet-open');
  sheet.setAttribute('aria-hidden', 'false');
  main.inert = true; nav.inert = true;
  // focus goes to the panel itself, not to Close: a focused button wore the yellow ring every time a profile opened
  sheet.focus({ preventScroll: true });
}
function openSheet(tab, from) {
  sheetTab = tab; profileKey = null; sheetFrom = from || null;
  drawSheet();
  $('#sheet-rows').scrollTop = 0;
  showSheet();
}
function closeSheet() {
  if (!sheetTab && !profileKey) return;
  sheetTab = null; profileKey = null;
  setLink(null);
  root.classList.remove('sheet-open');
  sheet.setAttribute('aria-hidden', 'true');
  main.inert = false; nav.inert = false;
  // the board is redrawn while the panel is open, so the link that opened it may be a new element by now
  const back = sheetFrom && (sheetFrom.isConnected ? sheetFrom : sheetFrom.dataset.pk && $(`#board a[data-pk="${sheetFrom.dataset.pk}"]`));
  if (back) back.focus({ preventScroll: true });
}

/* An expert's profile. Bob's key attributes: the description, online or working, the npub to copy, the rate, jobs
   delivered, who it takes jobs from, and the agent and model. A value the expert never published has no row. */
const FAMILY = { 'claude-code': 'Claude Code', claude: 'Claude Code', codex: 'Codex', cursor: 'Cursor', muse: 'Muse', grokbot: 'Grok Bot', opencode: 'OpenCode', gemini: 'Gemini', hermes: 'Hermes', goose: 'Goose', openclaw: 'OpenClaw', instinct: 'Instinct', cue: 'Cue' };
const fam = k => FAMILY[k] || k;
// A model as the seat reports it, without its settings: "gemini-4-neon[context=300k,…]" -> "gemini-4-neon".
// The id is shown as published and not turned into a product name: that would be a guess.
function runsOn(x) {
  const list = x.models.length
    ? x.models.map(m => { const id = m.model.replace(/\[.*$/, '').trim(); return id && id !== 'default' ? `${fam(m.family)} · ${id}` : fam(m.family); })
    : x.agents.map(fam);
  return [...new Set(list)].join(', ');
}
function takesFrom(x) {
  if (x.admitsTargeted === 'open') return 'Anyone';
  if (x.admitsTargeted === 'named') return 'Buyers it has approved';
  if (x.admitsTargeted === 'closed') return x.admitsPool === 'open' ? 'Open jobs only' : 'No one right now';
  return '';
}
function ago(at) {
  const s = Math.max(0, Date.now() / 1000 - at), n = s < 3600 ? Math.floor(s / 60) : s < 86400 ? Math.floor(s / 3600) : Math.floor(s / 86400);
  return s < 60 ? 'just now' : `${n} ${s < 3600 ? 'min' : s < 86400 ? 'hour' + (n === 1 ? '' : 's') : 'day' + (n === 1 ? '' : 's')} ago`;
}
const shortNpub = n => `${n.slice(0, 8)}…${n.slice(-4)}`;       // npub17znh…l9c3 (Bob)
function setLink(npub) {                    // the address names the open profile, so the page can be shared as it is
  const u = new URL(location.href);
  if ((u.searchParams.get('expert') || null) === npub) return;
  if (npub) u.searchParams.set('expert', npub); else u.searchParams.delete('expert');
  u.searchParams.delete('seller');          // the old page's name for the same link
  history.replaceState(null, '', u);
}
function drawProfile() {
  const pf = $('#pf'), title = $('#sheet-t');
  const x = handle ? handle.expert(profileKey, 6) : null;       // null until the whole history is in
  if (!x) {
    // Not found is only said once the market has answered in full: an expert's profile counts all of its history,
    // and in the first seconds of a first visit that is still being read (placeholders until then).
    const down = failed || state === 'failed', none = !down && view && state === 'live';
    title.textContent = none ? 'Expert not found' : 'Expert'; title.classList.remove('hex');
    pf.dataset.state = none || down ? 'none' : 'wait';
    alignSk();
    $('#pf-note').textContent = down ? 'The live market is not reachable right now.' : none ? 'No expert with this key has worked on the market.' : '';
    return;
  }
  pf.dataset.state = 'ok';
  setLink(x.npub);
  title.textContent = x.name; title.classList.toggle('hex', !x.named);
  $('#pf-lamp').className = 'lamp ' + (x.working ? 'busy' : x.online ? 'on' : '');
  $('#pf-st').textContent = x.working ? `Working now${x.working > 1 ? ` · ${x.working} jobs` : ''}` : x.online ? 'Online now' : `Offline${x.lastSeen ? ` · last active ${ago(x.lastSeen)}` : ''}`;
  const about = $('#pf-about');
  about.textContent = x.about || ''; about.hidden = !x.about;
  const chip = $('#pf-npub');              // the short form shows; the whole npub is what is copied (and the tooltip)
  if (chip.dataset.npub !== x.npub) { chip.dataset.npub = x.npub; chip.title = x.npub; chip.classList.remove('ok', 'full'); $('#pf-short').textContent = shortNpub(x.npub); }
  const kv = [
    ['Rate', x.rate == null ? '' : x.free ? 'Free' : `From ${esc(x.rate)} a job`],
    ['Jobs delivered', `${nf.format(x.delivered)} <small>all time</small>`],
    ['Takes jobs from', takesFrom(x)],
    ['Runs on', esc(runsOn(x))],
  ].filter(r => r[1]);
  paint($('#pf-kv'), kv.map(([k, v]) => `<div><dt>${k}</dt><dd>${v}</dd></div>`).join(''));
  paint($('#pf-rows'), x.recent.length ? x.recent.map(f => eventRow(f, false)).join('') : '<li class="note">No jobs yet.</li>');
}
function openProfile(id, from) {
  if (!root.classList.contains('sheet-open')) sheetFrom = from || null;
  profileKey = id;
  $('#pf').dataset.state = 'wait';
  drawSheet();
  $('#pf').scrollTop = 0;
  showSheet();
}
$('#sheet-back').addEventListener('click', () => {      // from a profile back to the list it was opened from
  profileKey = null; setLink(null); drawSheet();
  sheet.focus({ preventScroll: true });
});
document.addEventListener('click', e => {
  const el = e.target.closest('[data-pk]');
  if (!el || e.defaultPrevented || e.button || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;   // a new tab goes to the link itself
  e.preventDefault();
  openProfile(el.dataset.pk, el.closest('a') || el.querySelector('a') || el);
});
$('#sheet-x').addEventListener('click', closeSheet);
$('#sheet-bg').addEventListener('click', closeSheet);
$('#see-all').addEventListener('click', e => openSheet(board.dataset.tab, e.currentTarget));
$$('.more').forEach(b => b.addEventListener('click', () => openSheet(b.dataset.more, b)));
$('#tab3').addEventListener('click', e => {
  const b = e.target.closest('button[data-tab]');
  if (!b) return;
  board.dataset.tab = b.dataset.tab;
  $$('#tab3 button').forEach(x => x.setAttribute('aria-pressed', String(x === b)));
  alignSk();
});
document.addEventListener('click', e => {
  const b = e.target.closest('button[data-win]');
  if (b) pickWindow(b.dataset.win);
});
window.addEventListener('keydown', e => { if (e.key === 'Escape') { closeSheet(); setMenu(false); } });

/* ---------- FAQ: one answer open at a time ---------- */
$('#faq-list').addEventListener('click', e => {
  const b = e.target.closest('.fq button');
  if (!b) return;
  const item = b.closest('.fq'), open = !item.classList.contains('open');
  $$('#faq-list .fq').forEach(f => { f.classList.remove('open'); f.querySelector('button').setAttribute('aria-expanded', 'false'); });
  if (open) { item.classList.add('open'); b.setAttribute('aria-expanded', 'true'); }
});

/* ---------- copy ---------- */
async function copyText(text) {
  try { if (navigator.clipboard && window.isSecureContext) { await navigator.clipboard.writeText(text); return true; } } catch (_) {}
  // No clipboard API (a plain http address such as the preview, or an old browser): the old copy command, on a
  // selection in an unseen element. NOT a text box: a text box takes the focus, and handing the focus back to the
  // button from script made Safari draw the yellow focus ring on a button that had only been clicked (Bob saw it on
  // the npub). A selection moves no focus, so a click leaves no ring and a keyboard user keeps their place.
  const el = document.createElement('span'), sel = getSelection(), range = document.createRange();
  el.className = 'copysrc'; el.textContent = text; body.appendChild(el);
  range.selectNodeContents(el);
  sel.removeAllRanges(); sel.addRange(range);
  let ok = false;
  try { ok = document.execCommand('copy'); } catch (_) {}
  sel.removeAllRanges(); el.remove();
  return ok;
}
$$('[data-copy]').forEach(btn => btn.addEventListener('click', async () => {
  const label = btn.dataset.label || (btn.dataset.label = btn.textContent);
  const ok = await copyText($('#' + btn.dataset.copy).textContent);
  btn.textContent = ok ? 'Copied' : 'Select and copy'; btn.classList.toggle('ok', ok);
  setTimeout(() => { btn.textContent = label; btn.classList.remove('ok'); }, 1800);
}));
// The npub on a profile: a quiet short key with a copy mark (Bob: a copy box with a white button was too much).
// It says Copied for a moment. If the browser refuses to copy, it shows the whole npub instead, so it can be selected.
$('#pf-npub').addEventListener('click', async e => {
  const b = e.currentTarget, npub = b.dataset.npub, short = $('#pf-short');
  if (!npub) return;
  const ok = await copyText(npub);
  clearTimeout(b._t);
  b.classList.toggle('ok', ok); b.classList.toggle('full', !ok);
  short.textContent = ok ? 'Copied' : npub;
  if (ok) b._t = setTimeout(() => { if (b.dataset.npub === npub) { short.textContent = shortNpub(npub); b.classList.remove('ok'); } }, 1800);
});

/* ---------- the phone chat (How it works) ---------- */
// On a wide page only. Below 900 px the section is its words and the phone is not shown (Bob); the chat never
// starts there, because a hidden phone is never in view. The parts below that speak of phone-size pages (reading
// time, the tight phone) are teams.js's and do nothing here: sizeChat never marks this phone tight.
const msgs = $('#msgs'), demo = $('#demo');
const CANCEL = Symbol('cancel');
let tok = { dead: true };
const wait = (ms, t) => new Promise((res, rej) => setTimeout(() => (t.dead ? rej(CANCEL) : res()), ms));
let sp = 0, sv = 0, sraf = 0, slast = 0, sidle = 0;
const PAD_T = 20, PAD_B = 48;         // .phone .msgs padding: the first bubble's start, and the room kept clear at the bottom
const PAD_TIGHT = 16, TIGHT_UNDER = 176;   // a short phone on a phone-size page keeps 16 px clear (see teams.js)
const padBase = m => (m._tight ? PAD_TIGHT : PAD_B);
let padB = PAD_B;
function setPad(v) { v = Math.round(v); if (v !== padB) { padB = v; msgs.style.paddingBottom = v + 'px'; } }
function clearChat() { msgs.innerHTML = ''; msgs.classList.remove('fade'); setPad(padBase(msgs)); msgs.scrollTop = 0; sp = sv = 0; }
// Where the list rests: on a bubble edge, so the top bubble is always whole (teams.js chatEnd).
function chatEnd() {
  const last = msgs.lastElementChild;
  if (!last) return 0;
  const B = padBase(msgs), raw = last.offsetTop + last.offsetHeight + B - msgs.clientHeight;
  if (raw <= 0) { setPad(B); return 0; }
  let end = raw;
  for (const c of msgs.children) { const t = c.offsetTop - PAD_T; if (t >= raw - .5) { end = t; break; } }
  setPad(B + end - raw);
  return end;
}
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
function morph(d, html) {                  // the reply takes the typing bubble's place whole
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
// Reading time (phone-size pages): nothing pushes a message out before it could be read (Bob).
const needMs = d => (d.classList.contains('dots') ? 0 : d.classList.contains('card') ? 1400 : Math.min(4600, 500 + d.textContent.trim().length * 45));
function born(d) { d._until = performance.now() + needMs(d); }
function heightOf(cls, html) {
  const box = msgs.cloneNode(false);
  box.removeAttribute('id'); box.style.visibility = 'hidden'; box.style.pointerEvents = 'none';
  box.innerHTML = `<div class="b ${cls} vis">${html}</div>`;
  msgs.parentNode.appendChild(box);
  const h = box.firstChild.offsetHeight;
  box.remove();
  return h;
}
function pushedOut(h, gap, last = msgs.lastElementChild) {
  if (!last) return [];
  const cur = chatEnd(), under = last.offsetTop + last.offsetHeight + gap;
  const raw = under + h + padBase(msgs) - msgs.clientHeight;
  if (raw <= cur + .5) return [];
  let end = under - PAD_T;
  for (const c of msgs.children) { const t = c.offsetTop - PAD_T; if (t >= raw - .5) { end = Math.min(end, t); break; } if (c === last) break; }
  const out = [];
  for (const c of msgs.children) { const t = c.offsetTop - PAD_T; if (t >= cur - .5 && t < end - .5) out.push(c); if (c === last) break; }
  return out;
}
const gapAfter = (last, cls) => (last && ((last.classList.contains('agt') && /^(agt|dots)/.test(cls)) || (last.classList.contains('me') && /^me/.test(cls))) ? 3 : 9);
async function readFirst(cls, htmlOrH, t, last = msgs.lastElementChild) {
  if (!narrow() || !last) return;
  const h = typeof htmlOrH === 'number' ? htmlOrH : heightOf(cls, htmlOrH);
  const left = Math.max(0, ...pushedOut(h, gapAfter(last, cls), last).map(c => (c._until || 0) - performance.now()));
  if (left > 0) await wait(left, t);
}
async function userSays(text, t) {
  await wait(480, t);
  await readFirst('me tail', esc(text), t);
  addB('me', esc(text));
  await wait(760, t);
}
let dotsH = 41;
async function agentSays(html, t, think = 950, onShow) {
  await readFirst('dots', dotsH, t);
  const d = addB('dots', '<span class="dz"><i></i><i></i><i></i></span>');
  dotsH = d.offsetHeight || dotsH;
  try {
    await wait(think, t);
    await readFirst('agt tail', html, t, d.previousElementSibling);
  } catch (e) { d.remove(); throw e; }
  morph(d, html);
  if (onShow) onShow();
  await wait(700, t);
}
async function cardSays(html, t, after) {
  await readFirst('card', html, t);
  addB('card', html);
  await wait(after, t);
}

// The story the four steps tell. The expert's name and the price are an example, as on the Teams page.
const steps = $$('#steps li');
const mark = (i, cls) => { steps[i].classList.remove('run', 'done'); if (cls) steps[i].classList.add(cls); };
const JOB = '<div class="tc"><span class="ico c-purple"><svg><use href="#i-star"/></svg></span><div><div class="t">lingua</div><div class="tm">Delivered · $2 paid</div></div></div>';
async function play(t) {
  for (;;) {
    clearChat(); steps.forEach((_, i) => mark(i));
    await wait(500, t);
    mark(0, 'run');                          // 1 ask · 2 your agent finds an expert · 3 the expert works · 4 pay on delivery
    await userSays('Get my website translated into Spanish. Pay up to $2.', t);
    mark(0, 'done'); mark(1, 'run');
    await agentSays('Posted. <b>lingua</b> took the job.', t, 1100, () => { mark(1, 'done'); mark(2, 'run'); });
    await agentSays('The translation is in: all 6 pages, ready to publish.', t, 2000, () => { mark(2, 'done'); mark(3, 'run'); });
    await agentSays('I checked it and paid $2.', t, 1100);
    await cardSays(JOB, t, 300);
    mark(3, 'done');
    await wait(5600, t);
    msgs.classList.add('fade');
    await wait(260, t);
  }
}
function startDemo() {
  if (!tok.dead) return;
  const t = tok = { dead: false };
  play(t).catch(e => { if (e !== CANCEL) throw e; });
}
function stopDemo() { tok.dead = true; cancelAnimationFrame(sraf); sraf = 0; }

// The chat's room: the part of the phone that shows above the line where it is cut. The phone is scaled to fit its
// column's width.
function sizeChat() {
  if (!demo.clientHeight) return;          // a phone-size page: no phone
  const top = parseFloat(getComputedStyle(demo).getPropertyValue('--dtop')) || 22;
  const s = Math.min(.82, (demo.clientWidth - 16) / 444);
  demo.style.setProperty('--dps', s.toFixed(4));
  const mh = Math.max(110, (demo.clientHeight - top) / s - 12 - 124 - 22);
  msgs.style.setProperty('--mh', Math.round(mh) + 'px');
  if (msgs.children.length) follow(); else setPad(padBase(msgs));
}
new IntersectionObserver(es => { for (const e of es) (e.isIntersecting ? startDemo : stopDemo)(); }, { threshold: .35 }).observe(demo);

/* ---------- start ---------- */
let raf = 0;
function relayout() { cancelAnimationFrame(raf); raf = requestAnimationFrame(() => { sizeChat(); draw(); }); }
window.addEventListener('resize', relayout);
if (window.ResizeObserver) new ResizeObserver(relayout).observe(demo);
setInterval(() => { if (view) draw(); }, 30000);          // the ages in the activity list keep moving

// The page is shown once its fonts are in, as on the Teams page (a late face changed the logo's width).
let shown = false;
function show() {
  if (shown) return;
  shown = true;
  body.classList.add('ready');
  sizeChat(); draw();
  // a shared profile link. `seller` is the old market page's name for it (/market?seller=<hex>): those links still open.
  const query = new URLSearchParams(location.search), linked = query.get('expert') || query.get('seller');
  if (linked) openProfile(linked);
}
const FACES = ['400 16px "Instrument Sans"', '500 16px "Instrument Sans"', 'italic 500 16px "Work Sans Logo"'];
if (document.fonts && document.fonts.load) Promise.all(FACES.map(f => document.fonts.load(f))).then(show, show);
setTimeout(show, 800);

startMarket({
  feedLimit: 200,
  onView: v => { view = v; if (shown) draw(); },
  onStatus: s => { state = s; if (shown) (view ? drawStatus : draw)(); },
}).then(h => { handle = h; if (want !== 'week') pickWindow(want); }, err => { console.warn('[market] the reader did not start', err); failed = true; if (shown) draw(); });
