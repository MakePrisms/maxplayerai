// The /swarm board's reads: our relay only (docs/specs/swarm.md §5.3). No public relays, no quorum,
// no relay denylist and no buyer key built into the page: any buyer's swarms are listed, and a
// swarm page reads the buyer named by its address.
//
// Transport is the RelayReader (src/challenge/shared/relay.mjs: read-only, NIP-42 with a throwaway
// key, anonymous 15 s polling when AUTH is refused, fail-closed history). Two readers:
// - `swarm`: the swarm kinds (index: every buyer's; one swarm: its buyer's swarms and decisions,
//   the swarm's jobs and memberships by `#a`), plus profile names.
// - `trade` (one swarm only): the buyer's job events. Window mode, so today and the shown swarm
//   job are complete before older history backfills: the window opens TRADE_WINDOW_MARGIN_S before
//   the earlier of today's UTC midnight and the shown job's start, once the swarm jobs are read.
import { OFFER, CLAIM, RESULT, AWARD, ACCEPT, FOLLOW_UP, SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION } from '../../model/kinds.ts';
import { RelayReader } from '../../challenge/shared/relay.mjs';
import { dayStart, shownJob } from './model.mjs';
import { Decisions, latestMemberships, latestSwarmJobs, latestSwarms, swarmAddress } from './wire.mjs';

export const TRADE_WINDOW_MARGIN_S = 3600;
const admitAll = () => true;
const noRoster = () => {};

export const indexFilters = () => [SWARM, SWARM_JOB, SWARM_MEMBERSHIP, SWARM_DECISION].map(kind => ({ kinds: [kind] }));
export function swarmFilters(buyer, swarm) {
  const a = swarmAddress(buyer, swarm);
  return [
    { kinds: [SWARM], authors: [buyer] },
    { kinds: [SWARM_JOB], authors: [buyer], '#a': [a] },
    { kinds: [SWARM_MEMBERSHIP], '#a': [a] },
    { kinds: [SWARM_DECISION], authors: [buyer] },
  ];
}
export const taskFilters = buyer => [OFFER, CLAIM, AWARD, RESULT, ACCEPT, FOLLOW_UP].map(kind =>
  [CLAIM, RESULT].includes(kind) ? { kinds: [kind], '#p': [buyer] } : { kinds: [kind], authors: [buyer] });

export class SwarmReader {
  /**
   * swarm: { buyer, swarm } for one swarm's page, null for the index. Other options go to RelayReader.
   * @param {{url: string, store: any, swarm?: {buyer: string, swarm: string} | null, onChange?: () => void, windowMarginS?: number, now?: () => number, [option: string]: any}} options
   */
  constructor({ url, store, swarm = null, onChange = () => {}, windowMarginS = TRADE_WINDOW_MARGIN_S, now = () => Math.floor(Date.now() / 1000), ...options }) {
    this.store = store; this.target = swarm; this.margin = windowMarginS; this.now = now;
    const changed = () => { this.chooseWindow(); this.updateNames(); onChange(); };
    const common = { ...options, url, store, onChange: changed, onRoster: noRoster, admit: admitAll, roster: false, applicants: false };
    this.swarm = new RelayReader({ ...common, buyer: swarm?.buyer ?? '', metadata: true,
      filters: swarm ? swarmFilters(swarm.buyer, swarm.swarm) : indexFilters() });
    this.trade = swarm ? new RelayReader({ ...common, buyer: swarm.buyer, metadata: false, window: true, filters: taskFilters(swarm.buyer) }) : null;
  }
  // Profile names: the buyers on the index; the buyer and the members on a swarm page.
  updateNames() {
    const events = [...this.store.events.values()];
    let keys;
    if (!this.target) keys = latestSwarms(events).map(([, s]) => s.buyer);
    else {
      const { buyer, swarm } = this.target, d = new Decisions(buyer, events);
      keys = [buyer, ...latestMemberships(events).filter(([, m]) => m.buyer === buyer && m.swarm === swarm && d.allows(m.seller, swarm)).map(([, m]) => m.seller)];
    }
    this.swarm.setRoster(keys);
  }
  chooseWindow() {
    if (!this.trade || this.trade.windowSince !== undefined || !this.swarm.read?.has(String(SWARM_JOB))) return;
    const { buyer, swarm } = this.target;
    const job = shownJob(latestSwarmJobs([...this.store.events.values()]).filter(([, j]) => j.buyer === buyer && j.swarm === swarm));
    this.trade.setWindow(Math.max(0, Math.min(dayStart(this.now()), job?.starts ?? Infinity) - this.margin));
  }
  get status() {
    const s = this.swarm.status;
    if (!this.trade) return { ...s, windowComplete: s.historyComplete, windowSince: null, relays: [{ role: 'swarm', url: this.swarm.url, ...s }] };
    const t = this.trade.status;
    return {
      connected: s.connected && t.connected, authenticated: t.authenticated, authMode: t.authMode,
      historyComplete: s.historyComplete && t.historyComplete, windowComplete: s.historyComplete && t.windowComplete,
      windowSince: typeof this.trade.windowSince === 'number' ? this.trade.windowSince : null,
      pollInProgress: s.pollInProgress || t.pollInProgress, error: s.error ?? t.error,
      warnings: [...new Set([...s.warnings, ...t.warnings])],
      relays: [{ role: 'swarm', url: this.swarm.url, ...s }, { role: 'trade', url: this.trade.url, ...t }],
    };
  }
  start() { this.swarm.start(); this.trade?.start(); }
  stop() { this.swarm.stop(); this.trade?.stop(); }
}
