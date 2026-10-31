// #1243: since the relay membership gate (PR #1223), wss://relay.maxplayer.ai refuses the reader's
// throwaway-key NIP-42 AUTH with `restricted: not a relay member` and keeps serving the same open
// reads it serves an anonymous connection. The reader fell back to anonymous reads, but the runner's
// readiness required `authenticated`, so it never posted or published. These tests run the real
// ChallengeReader against a fake relay and feed its status to the runner's readiness checks.
import test from 'node:test';
import assert from 'node:assert/strict';
import * as relayModule from '../../web/app/src/challenge/shared/relay.mjs';
import { EventStore } from '../../web/app/src/challenge/shared/model.mjs';
import { relayReady, tradeReady, coordinate } from './coordinator.mjs';
import { readinessBlocker, relayCoverageLine } from './swarm.mjs';
const { ChallengeReader } = relayModule;
// No @noble import here: runner tests resolve packages only through web/app modules. The buyer
// is only a filter value for the reader.
const buyer = '79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798';
const pause = ms => new Promise(r => setTimeout(r, ms));
async function until(predicate, ms = 5000) { const end = Date.now() + ms; while (Date.now() < end) { if (predicate()) return; await pause(5); } assert.ok(predicate(), 'timed out'); }
const MEMBER_GATE = 'restricted: not a relay member';

// Fake relays keyed by URL. Per connection (socket.k = connection number for that URL):
// challenge(s) -> ms until an AUTH challenge (null: never); auth(s) -> true/false (OK) or null
// (never answered); every REQ gets EOSE after 3 ms (history and live alike, no events).
function relayNet(byUrl) {
  const net = { sockets: [] };
  net.Socket = class extends EventTarget {
    constructor(url) {
      super(); this.url = url; this.readyState = 0; this.k = net.sockets.filter(s => s.url === url).length + 1; net.sockets.push(this);
      const o = byUrl[url] ?? {};
      setTimeout(() => {
        if (this.readyState !== 0) return;
        this.readyState = 1; this.dispatchEvent(new Event('open'));
        const delay = (o.challenge ?? (() => null))(this);
        if (delay !== null) setTimeout(() => this.push(['AUTH', `nonce-${url}-${this.k}`]), delay);
      }, 0);
    }
    push(m) { if (this.readyState === 1) this.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(m) })); }
    send(data) {
      const m = JSON.parse(data), o = byUrl[this.url] ?? {};
      if (m[0] === 'AUTH') setTimeout(() => { const ok = (o.auth ?? (() => true))(this); if (ok === null) return; this.push(['OK', m[1].id, ok, ok ? '' : MEMBER_GATE]); }, 20);
      if (m[0] === 'REQ') setTimeout(() => this.push(['EOSE', m[1]]), 3);
    }
    close() { if (this.readyState === 3) return; this.readyState = 3; this.dispatchEvent(new Event('close')); }
  };
  return net;
}
function reader(t, net, options = {}) {
  const r = new ChallengeReader({ url: 'ws://trade', challengeRelays: ['ws://a'], quorum: 1, buyer, store: new EventStore(), WebSocketImpl: net.Socket,
    queueMs: 10, pollMs: 600000, retryMs: 20, stableMs: 600000, authWaitMs: 150, ...options });
  r.start(); t.after(() => r.stop());
  return r;
}
// The runner reads a copy of the status (relay-snapshot.mjs: {...reader.status}).
const snap = r => ({ asOf: 100, relay: { ...r.status } });

test('#1243: trade relay refusing the throwaway AUTH (member gate) is ready on a complete anonymous read', async t => {
  const net = relayNet({ 'ws://trade': { challenge: () => 0, auth: () => false }, 'ws://a': {} });
  const r = reader(t, net);
  await until(() => r.status.historyComplete && r.trade.status.warnings.some(w => w.startsWith('AUTH refused')));
  const trade = r.status.relays.find(x => x.role === 'trade');
  assert.equal(trade.authenticated, false);
  assert.ok(trade.warnings.some(w => w === `AUTH refused (${MEMBER_GATE}); polling`), trade.warnings.join('; '));
  // The old behaviour (main before #1243) fails here: refused AUTH meant "not authenticated".
  assert.equal(readinessBlocker(snap(r), 100), null);
  assert.equal(tradeReady(snap(r)), true, 'trade relay ready');
  assert.equal(relayReady(snap(r)), true, 'relay_not_ready is gone');
  assert.equal(trade.authMode, 'anonymous', 'the reader exposes the anonymous posture explicitly');
  assert.equal(r.status.authMode, 'anonymous');
  assert.equal(relayCoverageLine(r.status), 'All configured relays available');
  // The JSON the runner journals keeps the field too.
  assert.equal(JSON.parse(JSON.stringify(r.trade.status)).authMode, 'anonymous');
});

test('#1243: a relay that never challenges is anonymous and ready; one that accepts AUTH is authenticated and ready', async t => {
  const net = relayNet({ 'ws://trade': {}, 'ws://a': { challenge: () => 0, auth: () => true } });
  const r = reader(t, net);
  await until(() => relayReady(snap(r)) && r.challenges[0].status.authenticated);
  assert.equal(r.trade.status.authMode, 'anonymous');
  assert.equal(r.challenges[0].status.authMode, 'authenticated');
});

test('#1243: a challenge not yet answered is pending, never ready; only the no-answer fallback makes it anonymous', async t => {
  let answered = false, readyWhilePending = 0;
  const net = relayNet({ 'ws://trade': { challenge: () => 0, auth: () => null }, 'ws://a': {} });
  const r = reader(t, net, { authWaitMs: 400, onChange: () => { if (r?.trade.status.authMode === 'pending' && relayReady(snap(r))) readyWhilePending++; } });
  await until(() => r.trade.status.authMode === 'pending' && r.challenges[0].status.historyComplete);
  await pause(150);
  assert.equal(r.trade.status.authMode, 'pending');
  assert.equal(relayReady(snap(r)), false, 'AUTH expected and not done: not ready');
  assert.match(readinessBlocker(snap(r), 100), /trade relay not ready: ws:\/\/trade \((not authenticated|history incomplete)\)/);
  await until(() => r.trade.status.warnings.some(w => w.startsWith('AUTH not answered')), 2000); answered = true;
  await until(() => relayReady(snap(r)));
  assert.equal(r.trade.status.authMode, 'anonymous');
  assert.ok(answered); assert.equal(readyWhilePending, 0);
});

test('#1243: a reconnect to a relay that challenged before is pending until its challenge is answered', async t => {
  let readyWhilePending = 0;
  // Connection 1: challenged at once and refused (member gate). Connection 2: challenge 250 ms late.
  const net = relayNet({ 'ws://trade': { challenge: s => s.k === 1 ? 0 : 250, auth: () => false }, 'ws://a': {} });
  const r = reader(t, net, { authWaitMs: 1000, onChange: () => { if (r?.trade.status.authMode === 'pending' && relayReady(snap(r))) readyWhilePending++; } });
  await until(() => relayReady(snap(r)));
  r.trade.ws.close(); // a plain disconnect, not a relay CLOSED: no recovery round
  await until(() => r.trade.ws.k === 2 && r.trade.ws.readyState === 1);
  await pause(100);
  assert.equal(r.trade.status.authMode, 'pending', 'the relay challenged the earlier connection');
  assert.equal(relayReady(snap(r)), false);
  await until(() => relayReady(snap(r)), 3000);
  assert.equal(r.trade.status.authMode, 'anonymous');
  assert.equal(readyWhilePending, 0);
});

test('#1243: strict recovery on a relay that accepted AUTH still needs AUTH OK: a refused AUTH there is never ready', async t => {
  let closed = false, readyAfterClose = 0, anonymousInRecovery = 0;
  // Connection 1 accepts AUTH; every later connection refuses it.
  const net = relayNet({ 'ws://trade': { challenge: () => 0, auth: s => s.k === 1 }, 'ws://a': {} });
  const r = reader(t, net, {
    onChange: () => {
      if (!closed || !r) return;
      if (relayReady(snap(r))) readyAfterClose++;
      if (r.trade.closedStreak && r.trade.status.authMode === 'anonymous') anonymousInRecovery++;
    } });
  await until(() => relayReady(snap(r)) && r.trade.status.authenticated);
  closed = true;
  r.trade.ws.push(['CLOSED', 'live-3401', 'error: subscription dropped']);
  await pause(800);
  assert.ok(net.sockets.filter(s => s.url === 'ws://trade').length >= 3, 'retries on fresh connections');
  assert.equal(readyAfterClose, 0, 'a refused AUTH never completes a recovery that needs AUTH OK');
  assert.equal(anonymousInRecovery, 0, 'recovery that needs AUTH OK reports pending, never anonymous');
  assert.equal(relayReady(snap(r)), false);
});

test('#1243: an anonymous posture never excuses an error, a disconnect or incomplete history', () => {
  const ok = { role: 'trade', url: 'ws://trade', connected: true, authenticated: false, authMode: 'anonymous', historyComplete: true, error: null };
  const relay = o => ({ connected: true, authenticated: false, authMode: 'anonymous', historyComplete: true, error: null, failedRelays: [], quorum: { required: 1, complete: 1, total: 1 }, relays: [{ ...ok, ...o }] });
  assert.equal(relayReady({ relay: relay({}) }), true);
  for (const bad of [{ error: 'Relay history timed out' }, { connected: false }, { historyComplete: false }, { authMode: 'pending' }, { authMode: undefined }]) {
    assert.equal(tradeReady({ relay: relay(bad) }), false, JSON.stringify(bad));
    assert.equal(relayReady({ relay: relay(bad) }), false, JSON.stringify(bad));
  }
  // The aggregate status too (a reader without per-relay status).
  for (const bad of [{ error: 'x' }, { connected: false }, { historyComplete: false }, { authMode: 'pending' }, { authMode: undefined }])
    assert.equal(relayReady({ relay: { ...relay({}), relays: undefined, ...bad } }), false, JSON.stringify(bad));
  assert.equal(readinessBlocker({ asOf: 1, relay: relay({ authMode: 'pending' }) }, 1), 'trade relay not ready: ws://trade (not authenticated)');
  assert.equal(relayCoverageLine(relay({ historyComplete: false })), 'Relay coverage incomplete; trade relay not ready: ws://trade (required: history incomplete); nothing is posted or published');
  // Through coordinate(): the reason is relay_not_ready exactly when readiness fails.
  const s = r => ({ asOf: 100, relay: r, challenge: { id: 'd', starts: 100, ends: 200, status: 'in_progress' }, budget: { totalSats: 1, committedSats: 0, paidSats: 0, unknownAmounts: 0 }, jobs: [], sellers: [] });
  assert.ok(!coordinate(s(relay({}))).reasons.includes('relay_not_ready'));
  assert.ok(coordinate(s(relay({ authMode: 'pending' }))).reasons.includes('relay_not_ready'));
});
