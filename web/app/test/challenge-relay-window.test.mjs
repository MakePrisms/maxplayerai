// /swarm fast load (fix 2): the page's reader reads the current run's trade events first and
// backfills older history afterwards (relay.mjs window mode, ChallengeReader tradeWindowMarginS).
// The runner and CLI never set the option and read the whole history in one pass as before.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { ChallengeReader, matches } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
import { eventId, challengeTaskPrefix } from '../src/challenge/shared/events.mjs';
const keys=['01','02'].map(k=>k.padStart(64,'0'));
const [buyer,seller]=keys.map(k=>Buffer.from(schnorr.getPublicKey(k)).toString('hex'));
const ev=(kind,who,time,tags,content='')=>{const e={kind,pubkey:[buyer,seller][who],created_at:time,tags:[['t','maxplayer'],['v','1'],...tags],content};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,keys[who])).toString('hex');return e;};
const pause=ms=>new Promise(r=>setTimeout(r,ms));
async function until(p,ms=4000){const end=Date.now()+ms;while(Date.now()<end){if(p())return;await pause(2);}assert.ok(p(),'timed out');}
const MARGIN=3600,OLD=10000,CUR=100000,SINCE=CUR-MARGIN,NOW=CUR+500;
const run=(d,starts,status)=>ev(30341,0,starts-50,[['d',d],['title',d],['starts',String(starts)],['ends',String(starts+10800)],['budget','1000','sat'],['status',status]]);
// One awarded job; the claim can carry a seller clock far behind the offer.
function job(d,at,claimAt=at+1) {
  const offer=ev(3401,0,at,[['p',seller],['i',`${challengeTaskPrefix(buyer,d)}Task ${d}`],['amount','10','sat'],['param','deadline',String(at+3600)]]);
  const claim=ev(3402,1,claimAt,[['e',offer.id,'','root'],['p',buyer],['status','processing']]);
  const award=ev(3405,0,at+2,[['e',claim.id],['e',offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']]);
  return [offer,claim,award];
}
const challengeData=[run('old',OLD,'completed'),run('cur',CUR,'in_progress'),
  ev(30342,1,500,[['d',buyer],['p',buyer],['since','500'],['minutes','480'],['status','joined']]),ev(30343,0,600,[['d',seller],['p',seller],['decision','accepted']])];
// The current run's claim is dated before the window (seller clock 4400 s behind): it must still be read with the window.
const tradeData=[...job('old',OLD+100),...job('cur',CUR+100,SINCE-1000)];
// A fake relay network answering every REQ from its events (limit, since, until, #e), then EOSE.
// hold(frame,socket) keeps a REQ unanswered until release(); refuse(frame,socket) answers CLOSED.
function network({hold=()=>false,refuse=()=>false}={}) {
  const sockets=[];
  class Relay extends EventTarget {
    sent=[];held=[];readyState=1;
    constructor(url){super();this.url=url;this.n=sockets.filter(s=>s.url===url).length;sockets.push(this);queueMicrotask(()=>{if(this.readyState===1)this.dispatchEvent(new Event('open'));});}
    send(d){const m=JSON.parse(d);this.sent.push(m);if(m[0]!=='REQ')return;if(refuse(m,this))return setTimeout(()=>this.message(['CLOSED',m[1],'error: refused']),0);if(hold(m,this))this.held.push(m);else this.answer(m);}
    answer([,id,f]){setTimeout(()=>{if(this.readyState!==1)return;
      const data=this.url==='ws://trade'?tradeData:challengeData;
      for(const e of data.filter(e=>matches(e,f)).sort((a,b)=>b.created_at-a.created_at).slice(0,f.limit??Infinity))this.message(['EVENT',id,e]);
      this.message(['EOSE',id]);},0);}
    release(){const h=this.held;this.held=[];for(const m of h)this.answer(m);}
    message(m){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));}
    close(){if(this.readyState===3)return;this.readyState=3;this.dispatchEvent(new Event('close'));}
  }
  return {Relay,sockets,trade:()=>sockets.filter(s=>s.url==='ws://trade').at(-1)};
}
function reader(t,net,o={}) {
  const store=new EventStore();
  const r=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a','ws://b','ws://c'],buyer,store,WebSocketImpl:net.Relay,queueMs:1,pollMs:600000,retryMs:5,...o});
  r.start();t.after(()=>r.stop());return {r,store};
}
const full=()=>{const s=new EventStore();for(const e of [...challengeData,...tradeData])assert.ok(s.add(e));return s;};
const tradeIds=['3401','3402','3403','3405','3406'];
const historyReqs=sock=>sock.sent.filter(m=>m[0]==='REQ'&&tradeIds.includes(m[1]));

test('window mode: the current run is complete from the window, before the backfill; past jobs fill in after it',async t=>{
  const backfill=m=>m[2].until===SINCE&&!m[2]['#e'];
  const net=network({hold:(m,s)=>s.url==='ws://trade'&&backfill(m)});
  const {r,store}=reader(t,net,{tradeWindowMarginS:MARGIN});
  await until(()=>r.status.windowComplete===true);
  const s=r.status,snap=store.snapshot(buyer,NOW);
  assert.equal(s.historyComplete,false,'the backfill is still held');
  assert.equal(s.windowSince,SINCE);
  // First pass: every trusted trade filter from the window only, then the backfill below it.
  const first=new Map();for(const m of historyReqs(net.trade()))if(!first.has(m[1]))first.set(m[1],m[2]);
  assert.deepEqual([...first.keys()].sort(),tradeIds);
  for(const [id,f] of first)assert.equal(f.since,SINCE,`${id} window`);
  // The backfill goes out last (lowest send priority), and is held unanswered: still not complete.
  assert.ok([...r.trade.pages.values()].filter(p=>p.phase==='backfill').length===5,'one backfill page per trusted trade filter');
  await until(()=>historyReqs(net.trade()).filter(m=>backfill(m)).length===5);
  assert.equal(r.status.historyComplete,false);assert.equal(r.status.windowComplete,true);
  // The run's jobs are exactly what the whole history gives, including the claim dated before the window.
  const whole=full().snapshot(buyer,NOW);
  assert.equal(snap.jobs.length,1);assert.deepEqual(snap.jobs,whole.jobs);assert.deepEqual(snap.budget,whole.budget);
  assert.deepEqual(snap.sellers.map(x=>[x.state,x.minutesUsed]),whole.sellers.map(x=>[x.state,x.minutesUsed]));
  assert.ok(s.windowSince<=snap.challenge.starts,'the window covers the run: the page may show job states');
  // Past jobs (older runs) are not in yet; they come with the backfill.
  assert.equal(snap.sellers[0].pastJobs.length,0);
  assert.equal(whole.sellers[0].pastJobs.length,1);
  net.trade().release();
  await until(()=>r.status.historyComplete===true);
  assert.equal(r.status.windowComplete,true);
  assert.deepEqual(store.snapshot(buyer,NOW),whole,'after the backfill: identical to the whole history');
});

test('default (runner, CLI): no window; trade history is read in full in one pass, as before',async t=>{
  const net=network();
  const {r,store}=reader(t,net);
  await until(()=>r.status.historyComplete===true);
  for(const [,,f] of historyReqs(net.trade()).slice(0,5)){assert.equal(f.since,undefined);assert.equal(f.until,undefined);}
  assert.ok(!net.trade().sent.some(m=>m[0]==='REQ'&&m[1].startsWith('bound-')),'no window reads');
  assert.equal(r.status.windowComplete,true);assert.equal(r.status.windowSince,null);
  assert.deepEqual(store.snapshot(buyer,NOW),full().snapshot(buyer,NOW));
});

test('window mode fails closed: a silent window page times out and reconnects; the next connection reads the same window',async t=>{
  const net=network({hold:(m,s)=>s.url==='ws://trade'&&s.n===0&&m[1]==='3402'&&m[2].since===SINCE});
  const {r}=reader(t,net,{tradeWindowMarginS:MARGIN,pageTimeoutMs:150});
  let early=false;const watch=setInterval(()=>{if(net.trade()?.n===0&&r.status.windowComplete)early=true;},1);t.after(()=>clearInterval(watch));
  await until(()=>net.trade()?.n===1&&r.status.windowComplete===true);
  assert.equal(early,false,'never window-complete with a window page unanswered');
  assert.ok(net.sockets.filter(s=>s.url==='ws://trade')[0].readyState===3,'first trade connection closed');
  assert.equal(historyReqs(net.trade()).find(m=>m[1]==='3402')[2].since,SINCE,'same window after the reconnect');
  await until(()=>r.status.historyComplete===true);
});

test('window mode fails closed: a refused (CLOSED) window page recovers on a fresh connection',async t=>{
  const net=network({refuse:(m,s)=>s.url==='ws://trade'&&s.n===0&&m[1]==='3405'});
  const {r}=reader(t,net,{tradeWindowMarginS:MARGIN});
  const errors=[];const watch=setInterval(()=>{const e=r.trade.status.error;if(e&&!errors.includes(e))errors.push(e);if(net.trade()?.n===0&&r.status.windowComplete)errors.push('EARLY');},1);t.after(()=>clearInterval(watch));
  await until(()=>net.trade()?.n===1&&r.status.historyComplete===true);
  assert.ok(errors.some(e=>/history subscription refused \(3405\)/.test(e)),errors.join('; '));
  assert.ok(!errors.includes('EARLY'));
});

test('window mode: nothing trusted is read on the trade relay before the challenge events pick the run',async t=>{
  // The challenge relays hold their challenge pages: no window, so no trusted trade REQ at all.
  const net=network({hold:(m,s)=>s.url!=='ws://trade'&&m[1]==='30341'});
  const {r}=reader(t,net,{tradeWindowMarginS:MARGIN});
  await pause(60);
  assert.equal(historyReqs(net.trade()).length,0);
  assert.ok(!net.trade().sent.some(m=>m[0]==='REQ'&&/^live-34/.test(m[1])),'no trusted live REQ either');
  assert.equal(r.status.windowComplete,false);assert.equal(r.status.historyComplete,false);
  for(const s of net.sockets)if(s.url!=='ws://trade')s.release();
  await until(()=>r.status.windowComplete===true);
  assert.equal(r.status.windowSince,SINCE);
});
