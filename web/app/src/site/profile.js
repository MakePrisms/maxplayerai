/* A market profile (an expert's or a buyer's), drawn into the profile panel: the #pf-* elements of /market's panel,
   which /swarm's panel has too (Bob, 2026-11-21: a swarm's members and its buyer open their profiles in the swarm's
   own panel). The data is market-data.ts's (ExpertProfile, BuyerProfile); this file only draws it. Plain JavaScript,
   like market.js, which it was taken out of. No style="" in generated HTML (production's CSP is style-src 'self'). */
const $ = s => document.querySelector(s);
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const nf = new Intl.NumberFormat('en-US');
export function paint(el, html) { if (el._h !== html) { el._h = html; el.innerHTML = html; } }

// A name is a link to its profile (an expert's, or since Bob 2026-11-21 a buyer's): a real address, so it also opens
// in a new tab and can be shared. A click opens the profile in the panel instead (see openProfile).
export const who = (w, kind) => (w.npub
  ? `<a class="who${w.named ? '' : ' hex'}" href="/market?${kind}=${w.npub}" data-pk="${w.key}" data-kind="${kind}">${esc(w.name)}</a>`
  : `<b class="who${w.named ? '' : ' hex'}">${esc(w.name)}</b>`);
export const lamp = (cls, label) => `<i class="lamp ${cls}" role="img" aria-label="${label}" title="${label}"></i>`;
export const zero = s => `<span class="z">${s}</span>`;
export function age(at) {
  const s = Math.max(0, Date.now() / 1000 - at);
  return s < 60 ? 'now' : s < 3600 ? `${Math.floor(s / 60)}m` : s < 86400 ? `${Math.floor(s / 3600)}h` : `${Math.floor(s / 86400)}d`;
}
// One line per event, in plain words. The words differ from production's board (awarded, claimed, accepted the
// delivery) because this page is for someone who has not seen the market before; the facts are the same.
const EV_LAMP = { offer: 'post', award: 'hire', result: 'done', receipt: 'done' };
export function eventText(f) {
  const sells = f.stage === 'claim' || f.stage === 'result' || f.stage === 'feedback';     // the expert acts in these; in the rest the buyer does
  const a = who(f.actor, sells ? 'expert' : 'buyer'), o = f.other ? who(f.other, sells ? 'buyer' : 'expert') : f.unknownOther ? zero('an undetermined expert') : '';
  const rd = f.turn > 1 ? ` <span class="rd">· round ${f.turn}</span>` : '';
  const amt = f.price ? ` <span class="amt">· ${esc(f.price)}</span>` : '';
  switch (f.stage) {
    // Bob (2026-11-21): no Free tag on a free job (a priced one still shows its price); "claimed", not "bid on".
    case 'offer': return `${a} ${o ? `sent ${o} a job` : 'posted a job'}${f.free ? '' : amt}`;
    case 'claim': return `${a} claimed ${o ? `${o}’s job` : 'a job'}`;
    case 'award': return `${a} hired ${o || 'an expert'}`;
    case 'result': return `${a} delivered${o ? ` to ${o}` : ''}${rd}`;
    case 'accept': return `${a} accepted ${o ? `${o}’s work` : 'the work'}${rd}`;
    case 'receipt': return `${a} paid${o ? ` ${o}` : ''}${amt}${rd}`;
    case 'reply': return f.closed ? `${a} closed the job${o ? ` with ${o}` : ''}` : `${a} sent ${o || 'the expert'} a follow-up${rd}`;
    case 'feedback': return f.terminal ? `${a} stopped work on a job` : `${a} sent an update`;
    default: return a;
  }
}
export const eventRow = (f, lit) => `<li class="row g-a${lit ? ' new' : ''}">${lamp(f.stage === 'feedback' && f.terminal ? 'stop' : EV_LAMP[f.stage] || '', f.stage)}<span class="ev">${eventText(f)}</span><span class="age">${age(f.at)}</span></li>`;

const FAMILY = { 'claude-code': 'Claude Code', claude: 'Claude Code', codex: 'Codex', cursor: 'Cursor', muse: 'Muse', grokbot: 'Grok Bot', opencode: 'OpenCode', gemini: 'Gemini', hermes: 'Hermes', goose: 'Goose', openclaw: 'OpenClaw', instinct: 'Instinct', cue: 'Cue' };
const fam = k => (Object.hasOwn(FAMILY, k) ? FAMILY[k] : k);   // not FAMILY[k]: 'constructor' would name a function
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
export function ago(at) {
  const s = Math.max(0, Date.now() / 1000 - at), n = s < 3600 ? Math.floor(s / 60) : s < 86400 ? Math.floor(s / 3600) : Math.floor(s / 86400);
  return s < 60 ? 'just now' : `${n} ${s < 3600 ? 'min' : s < 86400 ? 'hour' + (n === 1 ? '' : 's') : 'day' + (n === 1 ? '' : 's')} ago`;
}
export const shortNpub = n => `${n.slice(0, 8)}…${n.slice(-4)}`;       // npub17znh…l9c3 (Bob)

function profileHead(x) {                   // the description and the npub chip, the same for an expert and a buyer
  const about = $('#pf-about');
  about.textContent = x.about || ''; about.hidden = !x.about;
  const chip = $('#pf-npub');              // the short form shows; the whole npub is what is copied (and the tooltip)
  if (chip.dataset.npub !== x.npub) { chip.dataset.npub = x.npub; chip.title = x.npub; chip.classList.remove('ok', 'full'); $('#pf-short').textContent = shortNpub(x.npub); }
}
const kvRows = kv => kv.map(([k, v]) => `<div><dt>${k}</dt><dd>${v}</dd></div>`).join('');
const recentRows = x => (x.recent.length ? x.recent.map(f => eventRow(f, false)).join('') : '<li class="note">No jobs yet.</li>');

/* An expert's profile. Bob's key attributes: the description, online or working, the npub to copy, the rate, jobs
   delivered, who it takes jobs from, and the agent and model. A value the expert never published has no row. */
// The npub chip: a quiet short key with a copy mark. It says Copied for a moment; if the browser refuses to copy, it
// shows the whole npub instead, so it can be selected. (market.js wires its own, with its copy fallback.)
export function wireNpubChip(copy) {
  $('#pf-npub').addEventListener('click', async e => {
    const b = e.currentTarget, npub = b.dataset.npub, short = $('#pf-short');
    if (!npub) return;
    const ok = await copy(npub);
    clearTimeout(b._t);
    b.classList.toggle('ok', ok); b.classList.toggle('full', !ok);
    short.textContent = ok ? 'Copied' : npub;
    if (ok) b._t = setTimeout(() => { if (b.dataset.npub === npub) { short.textContent = shortNpub(npub); b.classList.remove('ok'); } }, 1800);
  });
}

export function fillExpert(x, title) {
  title.textContent = x.name; title.classList.toggle('hex', !x.named);
  $('#pf-lamp').className = 'lamp ' + (x.working ? 'busy' : x.online ? 'on' : '');
  $('#pf-st').textContent = x.working ? `Working now${x.working > 1 ? ` · ${x.working} jobs` : ''}` : x.online ? 'Online now' : `Offline${x.lastSeen ? ` · last active ${ago(x.lastSeen)}` : ''}`;
  profileHead(x);
  paint($('#pf-kv'), kvRows([
    ['Rate', x.rate == null ? '' : x.free ? 'Free' : `From ${esc(x.rate)} a job`],
    ['Jobs delivered', `${nf.format(x.delivered)} <small>all time</small>`],
    ['Takes jobs from', takesFrom(x)],
    ['Runs on', esc(runsOn(x))],
  ].filter(r => r[1])));
  paint($('#pf-rows'), recentRows(x));
}
/* A buyer's profile (Bob, 2026-11-21): what it posted and paid, whether it is active, and its recent jobs. */
export function fillBuyer(x, title) {
  title.textContent = x.name; title.classList.toggle('hex', !x.named);
  $('#pf-lamp').className = 'lamp ' + (x.working ? 'busy' : x.active ? 'on' : '');
  $('#pf-st').textContent = x.working ? `${x.working} ${x.working === 1 ? 'job' : 'jobs'} running now` : x.active ? 'Active in the last 24 hours' : `Not active lately${x.lastSeen ? ` · last active ${ago(x.lastSeen)}` : ''}`;
  profileHead(x);
  paint($('#pf-kv'), kvRows([
    ['Jobs posted', `${nf.format(x.posted)} <small>all time</small>`],
    ['Jobs paid', `${nf.format(x.paid)} <small>all time</small>`],
    ['Paid to experts', `${esc(x.spent)} <small>published receipts</small>`],
  ]));
  paint($('#pf-rows'), recentRows(x));
}
