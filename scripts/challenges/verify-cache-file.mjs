// R3-6: the verified-signature cache (#1167, events.mjs) persisted next to the runner journal, so a
// restart does not re-run a Schnorr check for every event of its history (round 3: 4.5 minutes at
// 95-120% CPU after a restart, while both public relays timed out their first read).
//
// What is stored: the cache KEYS only, each id + pubkey + sig (256 lowercase hex), exactly the triple
// schnorr.verify(sig,id,pubkey) checked. Event ids, pubkeys and signatures are public relay data. A
// key never vouches for anything but that exact triple: verify() still recomputes the event id from
// the event's own fields before the lookup, so changed content/tags/kind/time/pubkey miss, and a
// different sig is a different key (a forged or foreign sig is Schnorr-checked and rejected as before).
//
// File: <home>/swarm/verified-sigs, mode 0600 in the runner's 0700 directory.
// TRUST BOUNDARY (advisor r1 L3): the SHA-256 in the header is a checksum, NOT a MAC. It only makes
// ACCIDENTAL damage (a torn or truncated write, a stray edit that leaves the header stale) fail safe.
// Anyone who rewrites the file with a matching checksum can plant any id+pubkey+sig key, and verify()
// then accepts that exact triple WITHOUT a Schnorr check, i.e. an unverified signature counts as
// verified. That is accepted on purpose: whoever can write $MAXPLAYER_HOME/swarm is already inside
// the home's trust boundary (the same user can read the buyer key and rewrite the journal), so a key
// over this file would add nothing. Never point the runner at a home another user can write.
//   line 1: "maxplayer-verified-sigs v1 <count> <sha256 hex of the body>"
//   body:   <count> lines, one key each
// Bounded: at most VERIFY_CACHE_LIMIT keys (the in-memory bound), so at most ~25.7 MB; a larger file
// is not even read. Loading checks the header, count, checksum and every key; ANY mismatch (a torn
// or truncated write, a foreign or edited file) loads NOTHING: the cache starts empty and every
// signature is verified again, exactly as before this file existed. A missing file is empty.
// Writes are crash-safe: temp file, fsync, rename over the file, fsync the directory, so a crash
// leaves the previous complete file or the new one. A write failure is logged and never fatal (the
// cache is an optimisation; the journal is what must be durable).
import { openSync, writeSync, fsyncSync, closeSync, renameSync, unlinkSync, readFileSync, statSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { dirname } from 'node:path';
import { VERIFY_CACHE_LIMIT, VERIFY_KEY, primeVerifyCache, verifyCacheKeys, setVerifyCacheListener } from '../../web/app/src/challenge/shared/events.mjs';
export const VERIFY_CACHE_FILE = 'verified-sigs';
export const VERIFY_CACHE_HEADER = 'maxplayer-verified-sigs v1';
// Flush cadence while new keys arrive; the runner also flushes once on a clean stop.
export const VERIFY_CACHE_FLUSH_MS = 30000;
const KEY_LINE = 257, HEADER_MAX = 200;
export const verifyCacheMaxBytes = (limit=VERIFY_CACHE_LIMIT) => HEADER_MAX+limit*KEY_LINE;
const sha256 = text => createHash('sha256').update(text).digest('hex');
export function encodeVerifyCache(keys) {
  const body=keys.map(k=>`${k}\n`).join('');
  return `${VERIFY_CACHE_HEADER} ${keys.length} ${sha256(body)}\n${body}`;
}
// {keys} or {error}; never partial.
export function decodeVerifyCache(text,{limit=VERIFY_CACHE_LIMIT}={}) {
  const nl=text.indexOf('\n');if(nl<0)return {error:'no header'};
  const m=text.slice(0,nl).match(/^maxplayer-verified-sigs v1 (\d+) ([0-9a-f]{64})$/);
  if(!m)return {error:'bad header'};
  const count=+m[1],body=text.slice(nl+1);
  if(!Number.isSafeInteger(count)||count>limit)return {error:`count ${m[1]} above the limit ${limit}`};
  // A torn, truncated or edited body fails the checksum; the count and per-key checks follow.
  // Integrity against accidents only: a rewrite that updates the checksum passes (see the header).
  if(sha256(body)!==m[2])return {error:'checksum mismatch (torn, truncated or edited file)'};
  const keys=count?body.slice(0,-1).split('\n'):[];
  if(keys.length!==count||keys.some(k=>!VERIFY_KEY.test(k)))return {error:'malformed key'};
  return {keys};
}
// {status:'missing'|'loaded'|'corrupt', keys, reason?}
export function loadVerifyCache(path,{limit=VERIFY_CACHE_LIMIT,read=p=>readFileSync(p,'latin1'),stat=statSync}={}) {
  let size;
  try {size=stat(path).size;} catch(e) {if(e.code==='ENOENT')return {status:'missing',keys:[]};return {status:'corrupt',keys:[],reason:e.message};}
  if(size>verifyCacheMaxBytes(limit))return {status:'corrupt',keys:[],reason:`file is ${size} bytes, above the ${verifyCacheMaxBytes(limit)}-byte bound`};
  let text;try {text=read(path);} catch(e) {return {status:'corrupt',keys:[],reason:e.message};}
  const r=decodeVerifyCache(text,{limit});
  return r.error?{status:'corrupt',keys:[],reason:r.error}:{status:'loaded',keys:r.keys};
}
export function saveVerifyCache(path,keys,{fs={}}={}) {
  fs={openSync,writeSync,fsyncSync,closeSync,renameSync,unlinkSync,...fs};
  const tmp=`${path}.tmp`,data=Buffer.from(encodeVerifyCache(keys),'latin1');let fd,renamed=false;
  try {
    fd=fs.openSync(tmp,'w',0o600);
    for(let o=0;o<data.length;){const n=fs.writeSync(fd,data,o,data.length-o);if(!n)throw Error('Short verify-cache write');o+=n;}
    fs.fsyncSync(fd);fs.closeSync(fd);fd=undefined;
    fs.renameSync(tmp,path);renamed=true;
    let dir;
    try{dir=fs.openSync(dirname(path),'r');fs.fsyncSync(dir);}
    catch(e){if(!['EINVAL','EPERM','EISDIR','ENOTSUP'].includes(e.code))throw e;}
    finally{if(dir!==undefined)fs.closeSync(dir);}
  } finally {
    if(fd!==undefined)try{fs.closeSync(fd);}catch{}
    if(!renamed)try{fs.unlinkSync(tmp);}catch{}
  }
}
// Loads and primes the process cache, then flushes it every flushMs while new keys were verified.
// Returns {loaded, flush(), stop()}; stop() flushes once more and detaches.
export function persistVerifyCache(path,{report=()=>{},flushMs=VERIFY_CACHE_FLUSH_MS,fs,load=loadVerifyCache}={}) {
  const started=Date.now(),loaded=load(path);
  const primed=primeVerifyCache(loaded.keys);
  report(loaded.status==='loaded'?`Verified-signature cache: loaded ${primed} keys from ${path} in ${Date.now()-started} ms`
    :loaded.status==='missing'?`Verified-signature cache: no ${path} yet; starting empty`
    :`WARNING: verified-signature cache ${path} unusable (${loaded.reason}); starting empty, every signature is verified again`);
  let dirty=loaded.status==='corrupt',stopped=false;
  setVerifyCacheListener(()=>{dirty=true;});
  const flush=()=>{
    if(!dirty)return false;dirty=false;
    try{saveVerifyCache(path,verifyCacheKeys(),{fs});return true;}
    catch(e){dirty=true;report(`WARNING: verified-signature cache not saved (${e.message}); will retry`);return false;}
  };
  const timer=setInterval(flush,flushMs);timer.unref?.();
  return {loaded:{...loaded,keys:undefined,count:primed},flush,stop:()=>{if(stopped)return;stopped=true;clearInterval(timer);setVerifyCacheListener(null);flush();}};
}
