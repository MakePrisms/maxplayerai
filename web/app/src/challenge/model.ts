import { decodeNpub } from "../trade/nip19.js";
import { schnorr } from "@noble/curves/secp256k1";

export interface Challenge {
  title: string;
  goal: string;
  buyerNpub: string;
  startsAt: string;
  joinOpensAt: string;
  budgetSats: number;
  status: "scheduled" | "in_progress" | "completed" | "cancelled";
  update: string;
}

export function buyerHex(npub: string): string {
  const bytes = decodeNpub(npub);
  if (!bytes) throw new Error("Buyer must be a valid npub public key");
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  schnorr.utils.lift_x(BigInt(`0x${hex}`));
  return hex;
}

function instant(value: string): number {
  const time = Date.parse(value);
  // Require canonical UTC so a missing timezone or normalized invalid date never slips through.
  if (!Number.isFinite(time) || new Date(time).toISOString() !== value) {
    throw new Error("Use a UTC timestamp such as 2026-10-16T18:00:00.000Z");
  }
  return time;
}

export function validateChallenge(challenge: Challenge): Challenge {
  if (![challenge.title, challenge.goal, challenge.update].every((s) => s.trim())) {
    throw new Error("Challenge title, goal and update are required");
  }
  buyerHex(challenge.buyerNpub);
  if (!Number.isSafeInteger(challenge.budgetSats) || challenge.budgetSats < 0) {
    throw new Error("Budget must be a non-negative safe integer in sats");
  }
  if (instant(challenge.startsAt) < instant(challenge.joinOpensAt) + 3_600_000) {
    throw new Error("Work starts after the one-hour join window closes");
  }
  if (!["scheduled", "in_progress", "completed", "cancelled"].includes(challenge.status)) {
    throw new Error("Unknown challenge status");
  }
  return challenge;
}

export function joinState(challenge: Challenge, now: number): string {
  if (challenge.status === "cancelled") return "Cancelled";
  if (challenge.status !== "scheduled") return "Joining closed";
  const opens = instant(challenge.joinOpensAt);
  if (now < opens) return "Joining opens soon";
  return now < opens + 3_600_000 ? "Open to join" : "Joining closed";
}

export function whitelistInstruction(challenge: Challenge): string {
  return `Allow the Maxplayer buyer ${challenge.buyerNpub} (hex ${buyerHex(challenge.buyerNpub)}) to send my seller targeted jobs. Merge this hex key into seller.accept_offers_only_from in my existing Maxplayer config without replacing other entries or changing my rate or other admission settings, then restart my seller. This trusts the buyer, not only this challenge.`;
}
