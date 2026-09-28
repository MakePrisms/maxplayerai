import { CLAIM, RESULT, FEEDBACK, AWARD, ACCEPT } from "../model/kinds.js";
import {
  auth,
  claim,
  Event,
  Evidence,
  NEMO,
  now,
  one,
  result,
  selectionTags,
  sign,
  trade,
  verify,
} from "./wire.js";
import { RecordState, Store } from "./store.js";
export interface Transport {
  read(filter: object): Promise<Event[]>;
  publish(e: Event, secret: string, evidence: Evidence): Promise<void>;
}
export async function step(
  store: Store,
  transport: Transport,
  time = now(),
  seller = NEMO,
  notify: (state: RecordState) => void = () => {},
): Promise<RecordState | undefined> {
  let s = await store.read();
  if (!s) return;
  const save = async (patch: Partial<RecordState>) => {
    s = (await store.update((old) => ({ ...old, ...patch })))!;
    notify(s);
    return s;
  };
  const send = async (
    e: Event,
    patch: Partial<RecordState>,
    evidence: Evidence = {},
  ) => {
    const found = await transport.read({ ids: [e.id], limit: 1 });
    if (
      !found.some((x) => {
        try {
          verify(x);
          return x.id === e.id;
        } catch {
          return false;
        }
      })
    )
      await transport.publish(e, s!.secret, evidence);
    await save(patch);
  };
  // Read before offer forwarding: the next poll replays history, including early deliveries.
  const history = await transport.read({
    "#e": [s.offer.id],
    authors: [seller, s.offer.pubkey],
    kinds: [CLAIM, RESULT, FEEDBACK, AWARD, ACCEPT],
    limit: 100,
  });
  // A lost acknowledgement may survive past the deadline. Reconcile once, but
  // never attempt to forward an old, unobserved offer or replace its identity.
  if (!s.offerAck && time > s.offer.created_at + 330) {
    const found = await transport.read({ ids: [s.offer.id], limit: 1 });
    const exists = found.some((e) => {
      try {
        verify(e);
        return e.id === s!.offer.id;
      } catch {
        return false;
      }
    });
    if (!exists) return save({ phase: "timeout" });
    await save({ profileAck: true, offerAck: true });
  }
  if (!s.profileAck) await send(s.profile, { profileAck: true });
  if (!s.offerAck) await send(s.offer, { offerAck: true, phase: "waiting" });
  const claims = history.filter((e) => {
    try {
      claim(e, s!.offer, seller);
      return true;
    } catch {
      return false;
    }
  });
  const unique = [...new Map(claims.map((c) => [c.id, c])).values()];
  if (
    unique.length > 1 ||
    (s.claim && unique.some((c) => c.id !== s!.claim!.id))
  )
    return save({ phase: "conflict" });
  if (!s.claim && unique[0] && time <= s.offer.created_at + 300) {
    const c = unique[0];
    await save({
      claim: c,
      award: sign(s.secret, AWARD, selectionTags(s.offer, c), "", time),
      phase: "starting",
    });
  }
  if (s.award && !s.awardAck) {
    // A persisted award may already be on the relay; never newly award after expiry.
    if (time > s.offer.created_at + 300) {
      const found = await transport.read({ ids: [s.award.id], limit: 1 });
      if (
        !found.some((e) => {
          try {
            verify(e);
            return e.id === s!.award!.id;
          } catch {
            return false;
          }
        })
      )
        return save({ phase: "timeout" });
      await save({ awardAck: true, phase: "working" });
    } else
      await send(
        s.award,
        { awardAck: true, phase: "working" },
        { offer: s.offer, claim: s.claim },
      );
  }
  if (s.claim && s.awardAck) {
    const candidates = history.filter((e) => {
      try {
        verify(e);
        return (
          e.kind === RESULT &&
          e.pubkey === seller &&
          one(e, "e")[1] === s!.offer.id
        );
      } catch {
        return false;
      }
    });
    const verified: Event[] = [];
    for (const e of candidates) {
      try {
        result(e, s.offer, s.claim, seller);
        verified.push(e);
      } catch {
        return save({ phase: "invalid" });
      }
    }
    const rs = [...new Map(verified.map((r) => [r.id, r])).values()];
    if (rs.length > 1 || (s.result && rs.some((r) => r.id !== s!.result!.id)))
      return save({ phase: "conflict" });
    if (!s.result && rs[0]) {
      const r = rs[0];
      await save({
        result: r,
        binding: result(r, s.offer, s.claim, seller),
        accept: sign(
          s.secret,
          ACCEPT,
          selectionTags(s.offer, s.claim),
          "",
          time,
        ),
        phase: "accept-pending",
      });
    }
    if (s.accept && !s.acceptAck)
      await send(
        s.accept,
        { acceptAck: true, phase: "done" },
        { offer: s.offer, claim: s.claim, result: s.result },
      );
  }
  if (s.acceptAck) return s;
  for (const e of history) {
    try {
      trade(e);
      if (
        e.kind === FEEDBACK &&
        e.pubkey === seller &&
        one(e, "e")[1] === s.offer.id &&
        one(e, "p")[1] === s.offer.pubkey &&
        ["error", "refused"].includes(one(e, "status")[1]!)
      )
        return save({ phase: "refused" });
    } catch {
      /* unrelated */
    }
  }
  if (time > s.offer.created_at + 330) return save({ phase: "timeout" });
  if (!s.claim && time > s.offer.created_at + 30)
    return save({ phase: "delayed" });
  return s;
}
export function envelope(e: Event, secret: string, evidence: Evidence = {}) {
  const eventBody = JSON.stringify(e);
  return { eventBody, relayAuth: auth(secret, eventBody), evidence };
}
