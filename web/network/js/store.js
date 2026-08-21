import { emptyUsage, percentile } from "./parse.js";
import { aggregateJobs, computePulse } from "./jobs.js";
import {
  buyerMetrics,
  resolveSeatDirectory,
  sellerMetrics,
  verifyAdvertisedHarnesses,
} from "./profiles.js";

/** In-memory aggregator for observatory views. */
export function createStore() {
  /** @type {Map<string, any>} */
  const byId = new Map();
  /** @type {Map<string, any>} */
  const offers = new Map();
  /** @type {Map<string, any[]>} */
  const claimsByOffer = new Map();
  /** @type {Map<string, any[]>} */
  const resultsByOffer = new Map();
  /** @type {Map<string, any[]>} */
  const receiptsByOffer = new Map();
  /** @type {Map<string, {pubkey:string, name:string|null, display_name:string|null, picture:string|null, about:string|null, created_at:number}>} */
  const profiles = new Map();
  /** @type {any[]} */
  const liveTail = [];
  let parseSkips = 0;

  /**
   * @param {any} normalized
   * @returns {{ ingested: boolean, newAuthor: string | null }}
   */
  function ingest(normalized) {
    if (!normalized) {
      parseSkips += 1;
      return { ingested: false, newAuthor: null };
    }

    if (normalized.role === "profile") {
      const prev = profiles.get(normalized.pubkey);
      if (!prev || prev.created_at <= normalized.created_at) {
        profiles.set(normalized.pubkey, {
          pubkey: normalized.pubkey,
          name: normalized.profile?.name ?? null,
          display_name: normalized.profile?.display_name ?? null,
          picture: normalized.profile?.picture ?? null,
          about: normalized.profile?.about ?? null,
          created_at: normalized.created_at,
        });
      }
      // Profiles stay out of the live tail / funnel.
      return { ingested: true, newAuthor: null };
    }

    if (byId.has(normalized.id)) {
      return { ingested: false, newAuthor: null };
    }
    byId.set(normalized.id, normalized);

    // Newest-first: keep sorted by created_at desc (relay may deliver out of order).
    liveTail.push(normalized);
    liveTail.sort((a, b) => b.created_at - a.created_at || (a.id < b.id ? -1 : 1));
    if (liveTail.length > 200) liveTail.length = 200;

    const newAuthor = profiles.has(normalized.pubkey) ? null : normalized.pubkey;

    switch (normalized.role) {
      case "offer":
        if (isPlausibleOffer(normalized)) {
          offers.set(normalized.id, normalized);
        }
        break;
      case "claim": {
        const offerId = normalized.claim?.offerId;
        if (offerId) pushMap(claimsByOffer, offerId, normalized);
        break;
      }
      case "result": {
        const offerId = normalized.result?.offerId;
        if (offerId) pushMap(resultsByOffer, offerId, normalized);
        break;
      }
      case "receipt": {
        const offerId = normalized.receipt?.offerId;
        if (offerId) pushMap(receiptsByOffer, offerId, normalized);
        break;
      }
      // No `handler` case. The NIP-89 kind-31990 announce is still parsed and still shows in the
      // live tail and the kind counts, but nothing derives a seat from it: the seat resolver moved
      // to kind-30340, so a per-author index of that kind only ever held residue.
      default:
        break;
    }
    return { ingested: true, newAuthor };
  }

  function funnel() {
    let unclaimed = 0;
    let unresulted = 0;
    let unpaid = 0;
    let claimed = 0;
    let resulted = 0;
    let receipted = 0;

    for (const offerId of offers.keys()) {
      const claims = claimsByOffer.get(offerId) || [];
      const results = resultsByOffer.get(offerId) || [];
      const receipts = receiptsByOffer.get(offerId) || [];
      if (claims.length === 0) unclaimed += 1;
      else claimed += 1;
      if (claims.length > 0 && results.length === 0) unresulted += 1;
      if (results.length > 0) resulted += 1;
      if (results.length > 0 && receipts.length === 0) unpaid += 1;
      if (receipts.length > 0) receipted += 1;
    }

    return {
      offers: offers.size,
      claimed,
      resulted,
      receipted,
      leaks: { unclaimed, unresulted, unpaid },
      parseSkips,
      events: byId.size,
      profiles: profiles.size,
    };
  }

  function latency() {
    const toClaim = [];
    const toResult = [];

    for (const [offerId, offer] of offers) {
      const claims = claimsByOffer.get(offerId) || [];
      if (!claims.length) continue;
      const firstClaim = minBy(claims, (c) => c.created_at);
      if (firstClaim) {
        const dt = firstClaim.created_at - offer.created_at;
        if (dt >= 0) toClaim.push(dt);
      }
      const results = resultsByOffer.get(offerId) || [];
      if (!results.length || !firstClaim) continue;
      const firstResult = minBy(results, (r) => r.created_at);
      if (firstResult) {
        const dt = firstResult.created_at - firstClaim.created_at;
        if (dt >= 0) toResult.push(dt);
      }
    }

    return {
      toClaim: summarize(toClaim),
      toResult: summarize(toResult),
      samples: { toClaim: toClaim.length, toResult: toResult.length },
    };
  }

  function economics() {
    /** @type {any[]} */
    const rows = [];
    // Resolve the usage a receipt's PAID job actually incurred — it lives on the kind-3403
    // RESULT the receipt settles (usage tags ride the result, not the receipt). Prefer the
    // exact result the receipt binds (its reply-tag id); else the newest result for the same
    // offer. Returns null when no result is visible → the paid row shows usage dashes.
    const resultUsageForReceipt = (receiptEv) => {
      const rid = receiptEv.receipt?.resultId;
      if (rid) {
        const bound = byId.get(rid);
        if (bound?.role === "result" && bound.result?.usage) return bound.result.usage;
      }
      const offerId = receiptEv.receipt?.offerId;
      const list = offerId ? resultsByOffer.get(offerId) : null;
      if (list && list.length) {
        const newest = list.reduce((a, b) => (b.created_at > a.created_at ? b : a));
        if (newest.result?.usage) return newest.result.usage;
      }
      return null;
    };
    for (const list of receiptsByOffer.values()) {
      for (const ev of list) {
        const receiptUsage = ev.receipt?.usage || emptyUsage();
        // JOIN (not replace): a kind-3400 receipt backs this row → PAID, and it keeps STATUS +
        // paid_price_sats. But the USAGE fields come from the settled RESULT (that is where the
        // token/transport/harness tags are) so a paid row shows its real usage, not dashes. No
        // bound result → fall back to the receipt's own usage (echo, if any), else dashes.
        const joined = resultUsageForReceipt(ev) || receiptUsage;
        rows.push({
          id: ev.id,
          created_at: ev.created_at,
          source: "paid",
          paid_price_sats: receiptUsage.paid_price_sats ?? ev.receipt?.amount_sats ?? null,
          paid_price_tokens: receiptUsage.paid_price_tokens,
          measured_cost_tokens: receiptUsage.measured_cost_tokens,
          total_tokens: joined.total_tokens,
          input_tokens: joined.input_tokens,
          output_tokens: joined.output_tokens,
          usage_transport: joined.usage_transport,
          harness_id: joined.harness_id,
          harness_family: joined.harness_family,
        });
      }
    }
    // the seller's kind-3403 RESULT is the AUTHORITATIVE usage source (the kind-3400
    // receipt echo is a convenience copy that may be absent). A settled
    // trade therefore fills the dashboard from its result. Skip offers that already carry a
    // receipt row (echo stands in there) to avoid double counting; an UNTAGGED result
    // contributes only dashes (absent-stays-absent — legacy trades are never backfilled).
    for (const [offerId, list] of resultsByOffer) {
      if (receiptsByOffer.has(offerId)) continue;
      for (const ev of list) {
        const u = ev.result?.usage || emptyUsage();
        rows.push({
          id: ev.id,
          created_at: ev.created_at,
          // Only a kind-3403 result backs this row (no receipt yet) → DELIVERED, not paid.
          // The label MUST stay distinct so "delivered" never reads as "paid".
          source: "delivered",
          paid_price_sats: u.paid_price_sats ?? ev.result?.amount_sats ?? null,
          paid_price_tokens: u.paid_price_tokens,
          measured_cost_tokens: u.measured_cost_tokens,
          total_tokens: u.total_tokens,
          input_tokens: u.input_tokens,
          output_tokens: u.output_tokens,
          usage_transport: u.usage_transport,
          harness_id: u.harness_id,
          harness_family: u.harness_family,
        });
      }
    }
    rows.sort((a, b) => b.created_at - a.created_at);

    /** @type {Map<string, {family:string|null, harness_id:string|null, transport:string|null, n:number, withCost:number, sumCost:number, sumPaidTok:number, sumPaidSat:number}>} */
    const groups = new Map();
    for (const row of rows) {
      const family = row.harness_family ?? null;
      const harness_id = row.harness_id ?? null;
      const transport = row.usage_transport ?? null;
      // JSON, never a "|" join: `harness_id` is seller-supplied text off the wire, so a separator
      // occurring inside it would merge two groups or split one. The parts also ride on the VALUE,
      // so no consumer has to parse the key back apart to render them.
      const key = JSON.stringify([family, harness_id, transport]);
      let g = groups.get(key);
      if (!g) {
        g = { family, harness_id, transport, n: 0, withCost: 0, sumCost: 0, sumPaidTok: 0, sumPaidSat: 0 };
        groups.set(key, g);
      }
      g.n += 1;
      if (row.measured_cost_tokens != null) {
        g.withCost += 1;
        g.sumCost += row.measured_cost_tokens;
      }
      if (row.paid_price_tokens != null) g.sumPaidTok += row.paid_price_tokens;
      if (row.paid_price_sats != null) g.sumPaidSat += row.paid_price_sats;
    }

    return { rows: rows.slice(0, 100), groups: [...groups.entries()] };
  }

  /**
   * The seat census: which seats exist right now.
   *
   * Sourced from the kind-30340 seat announcement at (pubkey, kind, `d`), bounded by freshness.
   * It previously read the NIP-89 kind-31990 handler announce, which the seat resolver no longer
   * uses, so that read answered from residue. Measured against the wire on 2026-08-21 it rendered
   * 21 rows of which 8 were live seats, 13 were 10.1-to-16.7-day-old residue, and one live seat was
   * missing entirely — a seat that came up after the resolver moved never publishes the retired
   * kind, so the old source goes progressively blinder to exactly the seats that matter.
   *
   * The fossil count and the window travel WITH the seats. A caller handed a bare list cannot say
   * how many rows the window removed, and "9 seats" reads identically whether 0 or 12 were cut.
   */
  function census(now) {
    const all = [...byId.values()];
    const dir = resolveSeatDirectory(all, now);
    return {
      freshWindowS: dir.freshWindowS,
      resolvedAt: dir.resolvedAt,
      fossilsExcluded: dir.fossilsExcluded,
      addressesSeen: dir.addressesSeen,
      seats: dir.seats.map((ev) => {
        // Advertised: the seat's own claim about itself, never proof it will serve. `verified` is
        // the separate reading of what it has actually delivered — carried as its own field so a
        // renderer cannot present one as the other by accident.
        const agents = ev.heartbeat?.agents ?? [];
        return {
          id: ev.id,
          pubkey: ev.pubkey,
          created_at: ev.created_at,
          agents,
          verified: verifyAdvertisedHarnesses(
            all,
            ev.pubkey,
            agents,
            ev.heartbeat?.capability?.filterable?.harness_family ?? [],
          ),
          // #784. Carried under its own key, split into `filterable` and `displayOnly` exactly as
          // the emitter splits it, so nothing here can put a display-only field where a filter
          // predicate would read it.
          capability: ev.heartbeat?.capability ?? null,
          version: ev.heartbeat?.version ?? null,
          status: ev.heartbeat?.status ?? null,
          profile: profiles.get(ev.pubkey) || null,
        };
      }),
    };
  }

  function tail(n = 50) {
    return liveTail.slice(0, n).map((ev) => ({
      ...ev,
      profile: profiles.get(ev.pubkey) || null,
    }));
  }

  function getProfile(pubkey) {
    return profiles.get(pubkey) || null;
  }

  /** Job-card feed: one card per offer chain, newest activity first. */
  function jobs(now) {
    return aggregateJobs([...byId.values()], profiles, now);
  }

  /** Pulse-strip numbers: sats settled today · open offers · active sellers. */
  function pulse(now, jobsList) {
    const js = jobsList || jobs(now);
    return computePulse([...byId.values()], js, now);
  }

  /** Seller reputation view for a pubkey. */
  function sellerProfile(pubkey, now) {
    return sellerMetrics([...byId.values()], pubkey, profiles, now);
  }

  /** Buyer reputation view for a pubkey. */
  function buyerProfile(pubkey, now) {
    return buyerMetrics([...byId.values()], pubkey, profiles, now);
  }

  function snapshot(now) {
    const jobsList = jobs(now);
    return {
      jobs: jobsList,
      pulse: pulse(now, jobsList),
      funnel: funnel(),
      latency: latency(),
      economics: economics(),
      census: census(now),
      tail: tail(50),
    };
  }

  return {
    ingest,
    snapshot,
    jobs,
    pulse,
    sellerProfile,
    buyerProfile,
    funnel,
    latency,
    economics,
    census,
    tail,
    getProfile,
  };
}

function pushMap(map, key, value) {
  let arr = map.get(key);
  if (!arr) {
    arr = [];
    map.set(key, arr);
  }
  arr.push(value);
}

function minBy(arr, fn) {
  let best = null;
  let bestV = Infinity;
  for (const item of arr) {
    const v = fn(item);
    if (v < bestV) {
      bestV = v;
      best = item;
    }
  }
  return best;
}

function summarize(values) {
  return {
    n: values.length,
    p50: percentile(values, 50),
    p90: percentile(values, 90),
    p99: percentile(values, 99),
    min: values.length ? Math.min(...values) : null,
    max: values.length ? Math.max(...values) : null,
  };
}

function isPlausibleOffer(ev) {
  const tags = ev.tags || [];
  const hasMaxplayer = tags.some((t) => t[0] === "t" && t[1] === "maxplayer");
  const hasAmount = tags.some((t) => t[0] === "amount" && t[1]);
  return hasMaxplayer || hasAmount;
}
