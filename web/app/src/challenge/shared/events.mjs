import { CHALLENGE, MEMBERSHIP, DECISION, OFFER, CLAIM, RESULT, AWARD, ACCEPT, RELAY_AUTH } from '../../model/kinds.ts';
import { sha256 } from '@noble/hashes/sha256';
import { bytesToHex } from '@noble/hashes/utils';
import { schnorr } from '@noble/curves/secp256k1';
export { npubEncode, decodeNpub } from '../../trade/nip19.ts';
export const hex = value => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
export const tag = (e, key) => e.tags.find(t => t[0] === key)?.[1];
export const one = (e, key) => e.tags.filter(t => t[0] === key).length === 1;
export const number = value => typeof value === 'string' && /^\d+$/.test(value) && Number.isSafeInteger(+value) ? +value : null;
export const root = e => { const refs=e.tags.filter(t=>t[0]==='e'&&t[3]==='root'); return refs.length===1&&hex(refs[0][1])?refs[0][1]:undefined; };
export const claimRef = e => { const refs=e.tags.filter(t=>t[0]==='e'&&!t[3]); return refs.length===1&&hex(refs[0][1])?refs[0][1]:undefined; };
export const mentions = (e, pk) => e.tags.some(t => t[0] === 'p' && t[1] === pk);
export const newer = (a,b) => !b || a.created_at > b.created_at || (a.created_at === b.created_at && a.id < b.id);
export function eventId(e) { return bytesToHex(sha256(new TextEncoder().encode(JSON.stringify([0,e.pubkey,e.created_at,e.kind,e.tags,e.content])))); }
// Schnorr verification is pure JS (~1-2 ms per event). Readers re-read their poll overlap every
// interval, re-read full history on every reconnect, and verify each frame twice (relay ingest,
// then EventStore.add), so a day of history pinned a runner at 99% CPU (run 2, issue #12).
// Successful verifications are cached:
// - key = id + pubkey + sig (fixed-length lowercase hex, checked before the lookup, so the
//   concatenation is unambiguous) — exactly the inputs schnorr.verify(sig,id,pubkey) checks;
// - the id is ALWAYS recomputed from the event (eventId) before the lookup, so a hit can only
//   accept an event whose canonical serialization hashes to the id whose signature was checked;
//   changed content/tags/kind/created_at/pubkey with a reused id+sig still fails;
// - failures are never cached (a forged signature is re-checked and rejected every time);
// - at most VERIFY_CACHE_LIMIT keys, process-wide (every relay of a reader shares them; the same
//   event on several relays is one key). A re-read costs one hash per event, with no Schnorr
//   check, only while the distinct verified events fit. Measured ~0.4 KB of retained heap per
//   entry (Node 24: 8.2 MB at 20,000, 40.4 MB at 100,000); a relay flooding unique valid events
//   can replace entries but never grow the cache. 100,000 is ~20,000 jobs' lifecycle events
//   (offer/claim/award/result/accept) plus challenge/roster events; past it, see below.
// - RANDOM replacement when full, no reordering on a hit. LRU (or FIFO) turns a stable re-scan
//   of cap+1 events into 100% misses: each miss evicts the key the scan needs next. Random
//   replacement degrades gracefully instead: a repeated scan of cap+1 events pays a mean of 2
//   Schnorr checks per pass, and a repeated scan of 1.5x the cap still hits ~40%.
// - R3-6: the Node runner persists these keys next to its journal (scripts/challenges/
//   verify-cache-file.mjs) so a restart does not re-verify the whole history. primeVerifyCache is
//   the only way in without a Schnorr check; it takes exact keys (the same id+pubkey+sig
//   concatenation, format-checked here), so a primed key can only ever accept that exact triple.
//   A primed key is TRUSTED, not re-checked: the caller vouches that it came from an earlier
//   successful Schnorr check. The runner's file is checksummed against accidents only, so whoever
//   can rewrite it can make an unverified signature count as verified; that writer is already inside
//   the runner home's trust boundary (it can read the buyer key). See verify-cache-file.mjs.
//   The browser never primes or listens.
export const VERIFY_CACHE_LIMIT = 100000;
export const VERIFY_KEY = /^[0-9a-f]{256}$/;
const verified = new Set(), slots = []; // same keys; slots gives O(1) random choice
let onAdd = null;
export const verifyCacheSize = () => slots.length;
export const clearVerifyCache = () => { verified.clear(); slots.length = 0; };
export const verifyCacheKeys = () => [...slots];
// Called with each key newly added after a successful Schnorr check (never for primed keys).
export const setVerifyCacheListener = fn => { onAdd = typeof fn === 'function' ? fn : null; };
function remember(key) {
  let slot = slots.length;
  if (slot >= VERIFY_CACHE_LIMIT) { slot = Math.floor(Math.random() * slots.length); verified.delete(slots[slot]); }
  slots[slot] = key; verified.add(key);
}
// Seeds keys verified by an earlier process. Malformed keys are skipped; returns how many were added.
export function primeVerifyCache(keys) {
  let added = 0;
  for (const key of keys) if (typeof key === 'string' && VERIFY_KEY.test(key) && !verified.has(key)) { remember(key); added++; }
  return added;
}
function signatureValid(e) {
  const key = e.id + e.pubkey + e.sig;
  if (verified.has(key)) return true;
  if (!schnorr.verify(e.sig, e.id, e.pubkey)) return false;
  remember(key);
  try { onAdd?.(key); } catch {}
  return true;
}
export function verify(e) {
  try { return hex(e.id) && hex(e.pubkey) && /^[0-9a-f]{128}$/.test(e.sig) && Number.isSafeInteger(e.kind) && e.kind >= 0 && e.kind <= 65535 && Number.isSafeInteger(e.created_at) && e.created_at >= 0 && typeof e.content === 'string' && Array.isArray(e.tags) && e.tags.every(t => Array.isArray(t) && t.every(v => typeof v === 'string')) && eventId(e) === e.id && signatureValid(e); } catch { return false; }
}
export function signAuth(key, relay, challenge, now = Math.floor(Date.now()/1000)) {
  const e = {pubkey:bytesToHex(schnorr.getPublicKey(key)),created_at:now,kind:RELAY_AUTH,tags:[['relay',relay],['challenge',challenge]],content:''};
  e.id=eventId(e); e.sig=bytesToHex(schnorr.sign(e.id,key)); return e;
}
const scalar = (e,k) => one(e,k) && e.tags.find(t=>t[0]===k).length===2;
export function validChallenge(e,buyer) {
  if (e.kind !== CHALLENGE || e.pubkey !== buyer || !namespace(e)) return false;
  if (!['d','title','starts','ends','status'].every(k=>scalar(e,k)) || !one(e,'budget') || e.tags.find(t=>t[0]==='budget').length!==3 || ['update','result'].some(k=>e.tags.some(t=>t[0]===k)&&!scalar(e,k))) return false;
  const [starts,ends,budget] = ['starts','ends','budget'].map(k=>number(tag(e,k)));
  return /^[a-z0-9-]{1,64}$/.test(tag(e,'d')) && !!tag(e,'title')?.trim() && [starts,ends,budget].every(n=>n!==null) && ends>starts && e.tags.find(t=>t[0]==='budget')[2]==='sat' && ['scheduled','in_progress','completed','cancelled'].includes(tag(e,'status'));
}
// Challenge events remain v1; existing buyer public jobs use the public-v2 envelope.
export const tradeNamespace = e => namespace(e) || ([OFFER,CLAIM,RESULT,AWARD,ACCEPT].includes(e.kind) && e.tags.some(t=>t[0]==='t'&&t[1]==='maxplayer') && one(e,'v') && tag(e,'v')==='2' && one(e,'visibility') && tag(e,'visibility')==='public' && one(e,'job') && hex(tag(e,'job')));
export const namespace = e => scalar(e,'t') && tag(e,'t')==='maxplayer' && scalar(e,'v') && tag(e,'v')==='1';
export function validMembership(e,c) {
  const buyer=c.pubkey,minutes=number(tag(e,'minutes')),since=number(tag(e,'since'));
  return e.kind===MEMBERSHIP && namespace(e) && ['d','p','minutes','status','since'].every(k=>scalar(e,k)) && !e.tags.some(t=>t[0]==='a') && tag(e,'d')===buyer && tag(e,'p')===buyer && since!==null && minutes!==null && minutes>=1 && minutes<=480 && ['joined','left'].includes(tag(e,'status'));
}
export function validDecision(e,c) {
  const seller=tag(e,'p');
  return e.kind===DECISION && e.pubkey===c.pubkey && namespace(e) && ['d','p','decision'].every(k=>scalar(e,k)) && !e.tags.some(t=>t[0]==='a') && hex(seller) && tag(e,'d')===seller && ['accepted','denied'].includes(tag(e,'decision')) && (!e.tags.some(t=>t[0]==='e') || (scalar(e,'e') && hex(tag(e,'e'))));
}

// Buyer-signed task prefix binds an offer without changing the trade protocol.
export const challengeTaskPrefix = (buyer,id) => `[challenge:${buyer}:${id}]\n`;

// Keep authenticated membership state; bound only its display note.
export const membershipNote = e => e.content.length>4096 || JSON.stringify(e.tags).length>16384 ? '' : e.content;
// Stored copy of a verified membership: tags/status/minutes/since/created_at unchanged, but a
// note the projection would drop is not retained either (its signature was checked on ingest).
export const boundMembership = e => membershipNote(e)===e.content ? e : {...e,content:''};
