// #1243 follow-up: the FULL /swarm status line, not just its first word. The member-gated launch
// relay refuses the reader's throwaway AUTH key and serves the same open reads (authMode
// 'anonymous', word "Live"), but the reader's "AUTH refused (…); polling" warning was still
// appended, so the live page read "Live · AUTH refused (restricted: not a relay member); polling".
// The page leaves that one expected warning out; status.warnings keeps it for the runner and CLI.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
import { ChallengeReader } from '../src/challenge/shared/relay.mjs';
import { EventStore } from '../src/challenge/shared/model.mjs';
import { connectionLine, connectionText, relayStatusWord } from '../src/challenge/live.ts';
import { paint } from '../src/challenge/page.ts';
import { npubEncode } from '../src/trade/nip19.ts';

const buyer=Buffer.from(schnorr.getPublicKey('01'.padStart(64,'0'))).toString('hex');
const GATE='restricted: not a relay member';
const REFUSED=`AUTH refused (${GATE}); polling`;
class Socket extends EventTarget {
  sent=[];readyState=1;
  constructor(url){super();this.url=url;}
  send(d){this.sent.push(JSON.parse(d));}
  message(m){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(m)}));}
  close(){if(this.readyState===3)return;this.readyState=3;this.dispatchEvent(new Event('close'));}
}
const finish=r=>{while(r.pages.size)for(const id of [...r.pages.keys()])r.ws.message(['EOSE',id]);};
// The launch relay today: the trade relay challenges, refuses the throwaway key, serves open reads.
function gatedReader(t,{complete=3}={}) {
  const reader=new ChallengeReader({url:'ws://trade',challengeRelays:['ws://a','ws://b','ws://c'],buyer,store:new EventStore(),WebSocketImpl:Socket,queueMs:1,pollMs:600000});
  reader.start();t.after(()=>reader.stop());
  for(const r of [reader.trade,...reader.challenges])r.ws.dispatchEvent(new Event('open'));
  const trade=reader.trade.ws;
  trade.message(['AUTH','nonce-1']);
  const auth=trade.sent.find(m=>m[0]==='AUTH');
  assert.ok(auth,'the reader answered the challenge');
  trade.message(['OK',auth[1].id,false,GATE]);
  finish(reader.trade);
  reader.challenges.slice(0,complete).forEach(finish);
  return reader;
}
// The page's paint(): the text the user actually sees in #connection-status.
const seller={pubkey:buyer,npub:npubEncode(buyer),name:'a',minutesOffered:60,minutesUsed:0,minutesRemaining:60,state:'working',eligible:true,decision:'accepted',currentJobStart:1000};
const snap={asOf:1500,challenge:{id:'t',buyer,buyerNpub:npubEncode(buyer),title:'T',goal:'G',starts:1000,ends:2000,status:'in_progress',result:''},sellers:[seller],jobs:[],budget:{totalSats:1,committedSats:0,paidSats:0,unknownAmounts:0,estimated:true,note:''},updates:[]};
class El { textContent='';children=[];attrs={};className='';constructor(tag='#text'){this.tag=tag;}
  getAttribute(k){return this.attrs[k]??null;} setAttribute(k,v){this.attrs[k]=v;}
  querySelector(sel){return this.children.find(c=>c.tag===sel)??null;}
  classList={toggle:()=>{}}; append(...c){this.children.push(...c);}
  replaceChildren(...c){this.children=c;this.textContent=c.map(n=>n.textContent).join('');} }
function shown(relay) {
  const ids={};
  const doc={getElementById:id=>(ids[id]??=new El(id)),createElement:t=>new El(t),createTextNode:t=>Object.assign(new El(),{textContent:t}),createDocumentFragment:()=>new El()};
  paint(doc,snap,relay,{wasLive:false,readyAt:null},{nowMs:0,now:1500,when:x=>`t=${x}`,status:relayStatusWord(relay),budget:()=>'$1.00'});
  return ids['connection-status'].textContent;
}

test('member-gated relay (AUTH refused, anonymous reads): the page line is exactly "Live"; the runner still sees the warning',t=>{
  const s=gatedReader(t).status;
  assert.equal(s.historyComplete,true);assert.equal(s.authMode,'anonymous');assert.equal(s.authenticated,false);
  // Unchanged for the runner and CLI: the reader's status still carries the refusal.
  assert.ok(s.warnings.includes(REFUSED),s.warnings.join('; '));
  assert.equal(relayStatusWord(s),'Live');
  assert.equal(connectionLine(s,relayStatusWord(s)),'Live');
  assert.equal(connectionText(s,relayStatusWord(s)),'Live');
  const line=shown(s);
  assert.equal(line,'Live');
  assert.ok(!line.includes('AUTH refused')&&!line.includes('polling'),line);
});

test('anonymous + AUTH refused + another warning: the other warning still shows after "Live"',t=>{
  // ws://c never completes: quorum met, degraded, with the reader's own warning.
  const s=gatedReader(t,{complete:2}).status;
  assert.equal(s.authMode,'anonymous');assert.equal(s.degraded,true);
  assert.ok(s.warnings.includes(REFUSED),s.warnings.join('; '));
  const degraded='Degraded: 2 of 3 challenge relays complete (quorum 2); missing: ws://c';
  assert.equal(connectionLine(s,'Live'),`Live · ${degraded}`);
  assert.equal(connectionText(s,'Live'),`Live · ${degraded}`);
  assert.equal(shown(s),`Live · ${degraded}`);
  // Synthetic: every other warning kind is kept, in order, with only the refusal dropped.
  const base={connected:true,authenticated:false,authMode:'anonymous',historyComplete:true,error:null};
  const others=['Recovering after relay CLOSED (x); reconnect 2','AUTH not answered (Relay did not answer AUTH (AUTH did not complete within 15s)); polling','Profile/heartbeat read incomplete','Live subscription k closed by relay (y); polling'];
  assert.equal(connectionLine({...base,warnings:[others[0],REFUSED,...others.slice(1)]},'Live'),`Live · ${others.join('; ')}`);
});

test('pending, authenticated or unknown AUTH posture: an AUTH refusal still shows, unchanged',()=>{
  const base={connected:true,authenticated:false,historyComplete:true,error:null,warnings:[REFUSED]};
  assert.equal(connectionLine({...base,authMode:'pending'},'Polling'),`Polling · ${REFUSED}`);
  assert.equal(connectionText({...base,authMode:'pending'},'Polling'),`Polling · ${REFUSED}`);
  assert.equal(connectionLine({...base,authenticated:true,authMode:'authenticated'},'Live'),`Live · ${REFUSED}`);
  // An older reader or demo status without authMode keeps the old line.
  assert.equal(connectionLine(base,'Polling'),`Polling · ${REFUSED}`);
});

test('anonymous: an error and unavailable relays still show',()=>{
  const r={connected:true,authenticated:false,authMode:'anonymous',historyComplete:true,windowComplete:true,
    error:'Swarm history incomplete; relay failed: wss://x',failedRelays:['wss://x'],warnings:[REFUSED,'w']};
  assert.equal(connectionLine(r,'Live'),'Live · Swarm history incomplete; relay failed: wss://x · Unavailable: wss://x · w');
  assert.equal(connectionText(r,'Live'),'Live · Swarm history incomplete; relay failed: wss://x · Unavailable: wss://x · w');
  assert.equal(connectionLine({...r,error:null,warnings:[REFUSED]},'Live'),'Live · Unavailable: wss://x');
});
