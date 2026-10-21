/**
 * The swarm buyer's public key, written into the static /swarm page and the swarm-join skill at
 * build time (CHALLENGE_BUYER_PUBKEY, same value the bundle gets).
 *
 * Both files carry the default key in their source, so they read correctly on GitHub too; the
 * build replaces those slots with the configured key, so an override reaches the shipped bytes and
 * the shipped copy can never drift from the build's key. Each slot must match exactly once, or the
 * build fails rather than shipping a page or skill without the key. The page script still fills
 * the same elements from the relay once it loads.
 */
import { npubEncode } from "../src/challenge/shared/events.mjs";

const HEX = "[0-9a-f]{64}";
const NPUB = "npub1[02-9ac-hj-np-z]{58}";
const SLOTS = {
  html: [
    [new RegExp(`(<code id="buyer-hex">)${HEX}(</code>)`, "g"), (k) => `$1${k.hex}$2`],
    [new RegExp(`(<code id="buyer-npub">)${NPUB}(</code>)`, "g"), (k) => `$1${k.npub}$2`],
    [new RegExp(`(<code id="apply-command">maxplayer-swarm join --buyer )${NPUB}( --minutes 60</code>)`, "g"), (k) => `$1${k.npub}$2`],
  ],
  skill: [
    [new RegExp(`(The swarm buyer's public key is \`)${HEX}(\` \\(npub \`)${NPUB}(\`\\))`, "g"), (k) => `$1${k.hex}$2${k.npub}$3`],
  ],
};

export function withSwarmBuyer(text, kind, hex) {
  if (kind === null) return text;
  if (!/^[0-9a-f]{64}$/.test(hex)) throw Error("CHALLENGE_BUYER_PUBKEY must be lowercase hex");
  const key = { hex, npub: npubEncode(hex) };
  for (const [re, to] of SLOTS[kind]) {
    const hits = text.match(re)?.length ?? 0;
    if (hits !== 1) throw Error(`swarm buyer key slot ${re.source} matched ${hits} times in the ${kind}, expected 1`);
    text = text.replace(re, to(key));
  }
  return text;
}
