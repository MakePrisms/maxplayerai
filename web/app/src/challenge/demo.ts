import type { Snapshot } from "./live.js";

// Fabricated snapshot for UI work (`/challenges?demo`). Never shown without the query flag.
// Seller keys are random throwaway identities; the buyer is the temporary test buyer.
export function demoSnapshot(nowSecs: number): Snapshot {
  const now = Math.floor(nowSecs);
  const starts = now - 35 * 60;
  const min = (m: number) => m * 60;
  const seller = (pubkey: string, npub: string, name: string, minutesOffered: number, minutesUsed: number,
    state: string, decision: string, currentJobStart: number | null) => ({
    pubkey, npub, name, minutesOffered, minutesUsed, minutesRemaining: Math.max(0, minutesOffered - minutesUsed),
    state, eligible: decision === "accepted" && state !== "withdrawn", decision, currentJobStart,
  });
  const s = {
    a: ["3e520509a5f2dbc97b56a92f31e29b5178daea9b34bc7b044fd6494263777932", "npub18efq2zd97tduj76k4yhnrc5m29ud465mxj78kpz06ey5ycmh0yeqj6jw67"],
    b: ["f0a9ac673a4a1ddb6e2b5249f3f852418dc38f219749d21be5a183ef033d1fb6", "npub17z56cee6fgwakm3t2fyl87zjgxxu8repjayayxl95xp77qear7mqm2zevl"],
    c: ["7021eeff0195acf6836837a6e5c6ef452e66b29c1b0206fd75f56bc54883743a", "npub1wqs7alcpjkk0dqmgx7nwt3h0g5hxdv5urvpqdlt4744u2jyrwsaq5m2v9q"],
    d: ["1783fbc53f49c70c2c7148cbfb9dfa3204c1403a9aa6d664b739c61b85b6fbf8", "npub1z7plh3flf8rsctr3fr9lh806xgzvzsp6n2ndve9h88rphpdkl0uq8a85d2"],
    e: ["47103a7f7ded2e20a45f753588556028351924c4d230d197c5d7e9ce0e083b15", "npub1gugr5lmaa5hzpfzlw56cs4tq9q63jfxy6gcdr9796l5uursg8v2s22q6me"],
    f: ["979fd8ee7ed21cff4970f5f60ea88fbcd22e07ef8fbf2af49915ce67ed629326", "npub1j70a3mn76gw07jts7hmqa2y0hnfzupl037lj4ayezh8x0mtzjvnqzlj2ct"],
  } as const;
  const job = (id: string, sellerKey: string, start: number, deadline: number, end: number | null) => ({
    id, seller: sellerKey, start, deadline, end, running: end === null,
    usedMinutes: Math.max(0, Math.min(end ?? now, deadline) - start) / 60,
  });
  return {
    asOf: now,
    challenge: {
      id: "demo",
      buyer: "94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5",
      buyerNpub: "npub1jjvf7aj6d8ju8ykkqv2qa4eu0lnpuyelhfm8ee2f2k85alzw2l2sf7haqz",
      title: "Crack the Hutton Cipher, Part 5",
      goal: "Solve MysteryTwister's Hutton Cipher Part 5: recover the plaintext and both keywords from the 4,250-letter ciphertext.",
      starts, ends: starts + min(240), joinOpens: starts - min(60), joinCloses: starts,
      status: "in_progress", result: "",
    },
    sellers: [
      seller(s.a[0], s.a[1], "cipher-crunch", 60, 32, "working", "accepted", now - min(12)),
      seller(s.c[0], s.c[1], "kasiski-bot", 120, 41, "working", "accepted", now - min(3)),
      seller(s.d[0], s.d[1], "playfair-fan", 60, 0, "accepted", "accepted", null),
      seller(s.b[0], s.b[1], "freq-analyst", 30, 30, "done", "accepted", null),
      seller(s.f[0], s.f[1], "quiet-otter", 45, 15, "withdrawn", "accepted", null),
      seller(s.e[0], s.e[1], "", 480, 0, "denied", "denied", null),
    ],
    jobs: [
      job("demo-a1", s.a[0], starts + min(1), starts + min(26), starts + min(21)),
      job("demo-a2", s.a[0], now - min(12), now + min(13), null),
      job("demo-c1", s.c[0], starts + min(1), starts + min(41), starts + min(39)),
      job("demo-c2", s.c[0], now - min(3), now + min(37), null),
      job("demo-b1", s.b[0], starts + min(2), starts + min(20), starts + min(20)),
      job("demo-b2", s.b[0], starts + min(21), starts + min(33), starts + min(33)),
      job("demo-f1", s.f[0], starts + min(2), starts + min(20), starts + min(17)),
    ],
    updates: [
      "Demo data: not a real challenge.",
      "Ciphertext split into four segments. kasiski-bot is testing key lengths 7–12; cipher-crunch is scoring candidate keywords.",
    ],
    budget: { totalSats: 10_000, committedSats: 2_400, paidSats: 3_600, estimated: true, unknownAmounts: 0, note: "Demo figures." },
    relay: { connected: true, authenticated: true, historyComplete: true, error: null },
  };
}
