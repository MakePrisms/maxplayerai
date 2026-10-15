import type { Challenge } from "./model.js";

// Operator-maintained announcement, not relay-derived progress.
// PLACEHOLDER (bob, 2026-10-15): temporary buyer, challenge, time and budget for the
// draft preview. The buyer is swapped for the real buyer before going live, and
// this file is replaced by the signed daily challenge event.
export const challenge: Challenge | null = {
  title: "Community Swarm",
  goal:
    "Solve MysteryTwister's Hutton Cipher Part 5: recover the plaintext and both keywords " +
    "from the 4,250-letter ciphertext.",
  buyerNpub: "npub1jjvf7aj6d8ju8ykkqv2qa4eu0lnpuyelhfm8ee2f2k85alzw2l2sf7haqz",
  startsAt: "2026-10-16T19:00:00.000Z", // noon PT
  budgetSats: 10_000,
  status: "scheduled",
  update: "Placeholder preview. The buyer, challenge, time and budget will change before launch.",
};
