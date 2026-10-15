import type { Challenge } from "./model.js";

// Operator-maintained announcement, not relay-derived progress. Replace null only
// when the challenge, public buyer identity, schedule and budget are confirmed.
export const challenge: Challenge | null = null;
