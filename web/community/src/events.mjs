import { createHash } from 'node:crypto';
import { schnorr } from '@noble/curves/secp256k1';
export { npubEncode, decodeNpub } from '../../app/src/trade/nip19.ts';
export const hex = value => typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
export const tag = (e, key) => e.tags.find(t => t[0] === key)?.[1];
export const one = (e, key) => e.tags.filter(t => t[0] === key).length === 1;
export const number = value => typeof value === 'string' && /^\d+$/.test(value) && Number.isSafeInteger(+value) ? +value : null;
export const root = e => { const refs=e.tags.filter(t=>t[0]==='e'&&t[3]==='root'); return refs.length===1&&hex(refs[0][1])?refs[0][1]:undefined; };
export const claimRef = e => { const refs=e.tags.filter(t=>t[0]==='e'&&!t[3]); return refs.length===1&&hex(refs[0][1])?refs[0][1]:undefined; };
export const mentions = (e, pk) => e.tags.some(t => t[0] === 'p' && t[1] === pk);
export const newer = (a,b) => !b || a.created_at > b.created_at || (a.created_at === b.created_at && a.id < b.id);
export function eventId(e) { return createHash('sha256').update(JSON.stringify([0,e.pubkey,e.created_at,e.kind,e.tags,e.content])).digest('hex'); }
export function verify(e) {
  try { return hex(e.id) && hex(e.pubkey) && /^[0-9a-f]{128}$/.test(e.sig) && Number.isSafeInteger(e.kind) && e.kind >= 0 && e.kind <= 65535 && Number.isSafeInteger(e.created_at) && e.created_at >= 0 && typeof e.content === 'string' && Array.isArray(e.tags) && e.tags.every(t => Array.isArray(t) && t.every(v => typeof v === 'string')) && eventId(e) === e.id && schnorr.verify(e.sig,e.id,e.pubkey); } catch { return false; }
}
export function signAuth(key, relay, challenge, now = Math.floor(Date.now()/1000)) {
  const e = {pubkey:Buffer.from(schnorr.getPublicKey(key)).toString('hex'),created_at:now,kind:22242,tags:[['relay',relay],['challenge',challenge]],content:''};
  e.id=eventId(e); e.sig=Buffer.from(schnorr.sign(e.id,key)).toString('hex'); return e;
}
export function validChallenge(e,buyer) {
  if (e.kind !== 30341 || e.pubkey !== buyer || !namespace(e)) return false;
  if (!['d','title','join_opens','join_closes','starts','ends','budget','status'].every(k=>one(e,k))) return false;
  const [opens,closes,starts,ends,budget] = ['join_opens','join_closes','starts','ends','budget'].map(k=>number(tag(e,k)));
  return /^[a-z0-9-]{1,64}$/.test(tag(e,'d')) && !!tag(e,'title')?.trim() && [opens,closes,starts,ends,budget].every(n=>n!==null) && closes===opens+3600 && starts>=closes && ends>starts && e.tags.find(t=>t[0]==='budget')[2]==='sat' && ['scheduled','in_progress','completed','cancelled'].includes(tag(e,'status'));
}
export const namespace = e => e.tags.some(t=>t[0]==='t'&&t[1]==='maxplayer') && one(e,'v') && tag(e,'v')==='1';
export function validApplication(e,c) {
  const buyer=c.pubkey,d=tag(c,'d'),minutes=number(tag(e,'minutes'));
  return e.kind===30342 && namespace(e) && ['d','a','p','minutes','status'].every(k=>one(e,k)) && tag(e,'d')===`${buyer}:${d}` && tag(e,'a')===`30341:${buyer}:${d}` && tag(e,'p')===buyer && e.created_at<=number(tag(c,'join_closes'))+900 && minutes!==null && minutes>=1 && minutes<=480 && ['applied','withdrawn'].includes(tag(e,'status'));
}
export function validDecision(e,c) {
  const seller=tag(e,'p'),d=tag(c,'d');
  return e.kind===30343 && e.pubkey===c.pubkey && namespace(e) && ['d','a','p','decision'].every(k=>one(e,k)) && hex(seller) && tag(e,'d')===`${d}:${seller}` && tag(e,'a')===`30341:${c.pubkey}:${d}` && ['accepted','denied'].includes(tag(e,'decision'));
}
