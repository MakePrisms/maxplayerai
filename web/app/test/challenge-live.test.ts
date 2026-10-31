import assert from 'node:assert/strict';
import test from 'node:test';
import * as live from '../src/challenge/live.js';
import {connectionLine,isLive,liveState,observe,challengeReady,renderLiveState,parseSnapshot,sellerLine,sellerMinutes,sellerState,renderSellers,UPDATING,type Snapshot} from '../src/challenge/live.js';
import {npubEncode} from '../src/trade/nip19.js';
const buyer='79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798';
const seller={pubkey:buyer,npub:npubEncode(buyer),name:'<script>untrusted</script>',minutesOffered:60,minutesUsed:32,minutesRemaining:28,state:'working',eligible:true,decision:'accepted',currentJobStart:1000};
const s:Snapshot={asOf:2920,challenge:{id:'test',buyer,buyerNpub:npubEncode(buyer),title:'Test',goal:'Goal',starts:1000,ends:10000,status:'in_progress',result:''},sellers:[seller],jobs:[{id:'job',seller:buyer,start:1000,deadline:4600,end:null,running:true,usedMinutes:32}],budget:{totalSats:100,committedSats:20,paidSats:10,unknownAmounts:0,estimated:true,note:'estimate'},updates:[]};
test('running minutes advance from snapshot and cap at deadline',()=>{assert.equal(sellerLine(s,seller,2920),'32 / 60 min');assert.equal(sellerMinutes(s,seller,2980),33);assert.equal(sellerMinutes(s,seller,9000),60);assert.equal(sellerMinutes(s,seller,2000),32);});
test('completed minutes do not advance; simultaneous jobs count independently',()=>{assert.equal(sellerMinutes({...s,jobs:[{...s.jobs[0]!,running:false}]},seller,9000),32);assert.equal(sellerMinutes({...s,jobs:[s.jobs[0]!,{...s.jobs[0]!,id:'second'}]},seller,2980),34);});
test('snapshot parser accepts no challenge and rejects malformed data',()=>{assert.equal(parseSnapshot(s),s);assert.equal(parseSnapshot({...s,challenge:null,budget:null}).challenge,null);for(const value of [null,{}, {...s,asOf:NaN},{...s,sellers:[{...seller,minutesUsed:'32'}]},{...s,challenge:{...s.challenge,buyer:'wrong'}}])assert.throws(()=>parseSnapshot(value));});
// Minimal DOM double: remote text must only reach textContent, never innerHTML.
class Element {textContent='';className='';children:Element[]=[];append(...children:Element[]){this.children.push(...children);}replaceChildren(...children:Element[]){this.children=children;}}
test('seller rendering preserves literal untrusted names and accepted summary',()=>{const list=new Element(),summary=new Element();const doc={getElementById:(id:string)=>id==='challenge-sellers'?list:summary,createElement:()=>new Element(),createDocumentFragment:()=>new Element()};renderSellers(doc as unknown as Document,s,2920);assert.equal(summary.textContent,'1 agent · 1 working now');assert.equal(list.children[0]!.children[0]!.children[0]!.textContent,'<script>untrusted</script>');assert.equal(list.children[0]!.children[0]!.children[1]!.textContent,'32 / 60 min');assert.equal(list.children[0]!.children[0]!.children[3]!.textContent,'Working now');});
test('denied members are not listed publicly; finished and left sellers are marked',()=>{const list=new Element(),summary=new Element();const doc={getElementById:(id:string)=>id==='challenge-sellers'?list:summary,createElement:()=>new Element(),createDocumentFragment:()=>new Element()};
  const denied={...seller,pubkey:'d',name:'denied',decision:'denied',eligible:false,state:'denied'};const done={...seller,pubkey:'e',name:'done',minutesUsed:60,state:'accepted'};const gone={...seller,pubkey:'f',name:'gone',state:'left',eligible:false};
  // 'gone' worked in this run, so it is listed as Left (a leaver without a job here is not: challenge-hide-left.test.ts).
  renderSellers(doc as unknown as Document,{...s,sellers:[seller,denied,done,gone],jobs:[...s.jobs,{id:'gone-job',seller:'f',start:1000,deadline:1600,end:1600,running:false,usedMinutes:10}]},2920);
  const rows=list.children[0]!.children;assert.deepEqual(rows.map(r=>r.children[0]!.textContent),['<script>untrusted</script>','done','gone']);
  assert.deepEqual(rows.map(r=>r.children[3]!.textContent),['Working now','Done','Left']);assert.equal(rows[1]!.className,'off');assert.equal(summary.textContent,'3 agents · 1 working now');});
test('a seller whose job finished but has minutes left is Ready, not Done',()=>{const a={...seller,state:'done',minutesUsed:5};assert.equal(sellerState({...s,jobs:[]},a,2920),'Ready');assert.equal(sellerState({...s,jobs:[]},{...a,minutesUsed:60},2920),'Done');});

test('delivered running job is awaiting acceptance and conservative charge never ticks backwards',()=>{
  const a={...seller,state:'awaiting acceptance',minutesUsed:60};
  const snap={...s,sellers:[a],jobs:[{...s.jobs[0]!,usedMinutes:60}]};
  assert.equal(sellerState(snap,a,2980),'Awaiting acceptance');
  assert.equal(sellerMinutes(snap,a,2980),60);
});

test('awaiting acceptance is excluded from working-now summary',()=>{
  const list=new Element(),summary=new Element();
  const doc={getElementById:(id:string)=>id==='challenge-sellers'?list:summary,createElement:()=>new Element(),createDocumentFragment:()=>new Element()};
  renderSellers(doc as unknown as Document,{...s,sellers:[{...seller,state:'awaiting acceptance'}]},2920);
  assert.equal(list.children[0]!.children[0]!.children[3]!.textContent,'Awaiting acceptance');
  assert.equal(summary.textContent,'1 agent · 0 working now');
});

test('until the live data is in, the status line is only "Updating…" with no disclaimers',()=>{
  const base={connected:true,authenticated:true,historyComplete:true,error:null};
  for(const r of [{...base,connected:false},{...base,historyComplete:false},{...base,historyComplete:false,error:'Relay history timed out',failedRelays:['wss://x'],warnings:['Profile/heartbeat read incomplete']}])
    assert.equal(connectionLine(r,'Live'),'Updating…');
  assert.equal(UPDATING,'Updating…');
  // Once live, unchanged: status plus the relay's own reasons.
  assert.equal(connectionLine(base,'Live'),'Live');
  assert.equal(connectionLine({...base,failedRelays:['wss://a'],warnings:['w']},'Polling'),'Polling · Unavailable: wss://a · w');
  // No relay status at all is not live.
  assert.equal(connectionLine(undefined,'Live'),'Updating…');
  assert.equal(isLive(undefined),false);
  assert.equal(isLive({...base,historyComplete:false}),false);
});

test('the status line is latched: once live, a routine poll or catch-up keeps "Live"; only a disconnect shows "Updating…"',()=>{
  const live={connected:true,authenticated:true,historyComplete:true,error:null};
  // Before the first live data: still only "Updating…".
  assert.equal(connectionLine({...live,historyComplete:false},'Live',false),'Updating…');
  assert.equal(connectionLine(live,'Live',false),'Live');
  // Live once, then a poll round clears the aggregate historyComplete while still connected.
  assert.equal(connectionLine({...live,historyComplete:false},'Live',true),'Live');
  assert.equal(connectionLine({...live,authenticated:false,historyComplete:false},'Polling',true),'Polling');
  // Reasons are still shown while the latch holds.
  assert.equal(connectionLine({...live,historyComplete:false,error:'Swarm history incomplete; relay failed: wss://x',failedRelays:['wss://x'],warnings:['w']},'Live',true),
    'Live · Swarm history incomplete; relay failed: wss://x · Unavailable: wss://x · w');
  // Actually disconnected: "Updating…" even after having been live.
  assert.equal(connectionLine({...live,connected:false,historyComplete:false},'Live',true),'Updating…');
  assert.equal(connectionLine({...live,connected:false,historyComplete:false},'Live',false),'Updating…');
  assert.equal(connectionLine(undefined,'Live',true),'Updating…');
});

test('while loading the summary is empty; once live it shows counts without the "may be incomplete" caveat',()=>{
  const list=new Element(),summary=new Element();
  const doc={getElementById:(id:string)=>id==='challenge-sellers'?list:summary,createElement:()=>new Element(),createDocumentFragment:()=>new Element()};
  const snap={...s,applicants:[],relay:{connected:true,authenticated:true,historyComplete:false,error:null,applicantsIncomplete:true}};
  renderSellers(doc as unknown as Document,snap,2920,true);
  assert.equal(summary.textContent,'');
  renderSellers(doc as unknown as Document,{...snap,relay:{...snap.relay,historyComplete:true}},2920);
  const line:string=summary.textContent;
  assert.equal(line,'1 agent · 1 working now · 0+ waiting for approval');
  assert.ok(!line.includes('incomplete'));
});

test('rows, counts and eyebrow follow the live state: kept through a routine poll, cleared while disconnected',()=>{
  const live={connected:true,authenticated:true,historyComplete:true,error:null};
  assert.equal(liveState({...live,historyComplete:false},false),'updating');
  assert.equal(liveState(live,false),'live');
  assert.equal(liveState({...live,historyComplete:false},true),'live');
  assert.equal(liveState({...live,connected:false,historyComplete:false},true),'updating');
  assert.equal(liveState(undefined,true),'updating');
  const list=new Element(),summary=new Element();
  const doc={getElementById:(id:string)=>id==='challenge-sellers'?list:summary,createElement:()=>new Element(),createDocumentFragment:()=>new Element()};
  renderSellers(doc as unknown as Document,s,2920,false);
  assert.equal(list.children.length,1);assert.equal(summary.textContent,'1 agent · 1 working now');
  // Disconnected: no Working-now rows and no counts presented as current.
  renderSellers(doc as unknown as Document,s,2920,true);
  assert.equal(list.children.length,0);assert.equal(summary.textContent,'');
  // Eyebrow: hidden unless live.
  const classes=new Set<string>();const eyebrow={classList:{toggle:(c:string,on:boolean)=>{on?classes.add(c):classes.delete(c);}}};
  const edoc={getElementById:()=>eyebrow} as unknown as Document;
  renderLiveState(edoc,'updating');assert.ok(classes.has('is-off'));
  renderLiveState(edoc,'live');assert.ok(!classes.has('is-off'));
});

test('readiness for "Running now": challenge relays only, refreshed on each ready read, held through a poll, cleared by a drop or error',()=>{
  const ch=(o={})=>['wss://a','wss://b','wss://c'].map(url=>({role:'challenge',url,connected:true,historyComplete:true,error:null,...o}));
  const trade={role:'trade',url:'wss://t',connected:true,historyComplete:false,error:null};
  // Trade not authenticated and its history still loading: the challenge data is ready anyway.
  const live={connected:true,authenticated:false,historyComplete:false,error:null,relays:[trade,...ch()]};
  assert.equal(challengeReady(live),true);
  assert.equal(challengeReady({...live,relays:[trade,...ch({historyComplete:false})]}),false);
  assert.equal(challengeReady({...live,relays:[trade]}),false,'no challenge relay');
  assert.equal(challengeReady(undefined),false);
  // Without per-relay status (demo), the aggregate stands in.
  assert.equal(challengeReady({connected:true,authenticated:true,historyComplete:true,error:null}),true);
  assert.equal(challengeReady({connected:true,authenticated:true,historyComplete:false,error:null}),false);
  let t=observe({wasLive:false,readyAt:null},{...live,relays:[trade,...ch({historyComplete:false})]},0);
  assert.equal(t.readyAgeS,null);
  t=observe(t,live,1000);assert.equal(t.readyAgeS,0);
  // A poll round on one challenge relay keeps the last ready time; age grows, no reset.
  const poll={...live,relays:[trade,...ch().map((r,i)=>i?r:{...r,historyComplete:false})]};
  t=observe(t,poll,2000);assert.equal(t.readyAgeS,1);
  t=observe(t,live,16000);assert.equal(t.readyAgeS,0);
  t=observe(t,poll,23000);assert.equal(t.readyAgeS,7);
  // A challenge relay dropping or erroring clears it at once.
  assert.equal(observe(t,{...live,relays:[trade,...ch().map((r,i)=>i?r:{...r,connected:false})]},23001).readyAgeS,null);
  assert.equal(observe(t,{...live,relays:[trade,...ch().map((r,i)=>i?r:{...r,error:'CLOSED'})]},23001).readyAgeS,null);
  assert.equal(observe(t,{connected:false,authenticated:false,historyComplete:false,error:null},23001).readyAgeS,null);
});

// #1243: the member-gated relay refuses the reader's throwaway AUTH and serves the same open reads,
// so the swarm page showed "Polling" for a complete read. Anonymous posture is Live; pending is not.
test('status word: authenticated or anonymous posture is Live; pending AUTH or an older status without it is Polling',()=>{
  // Namespace import: on a base without the helper this fails here, not at module load.
  assert.equal(typeof live.relayStatusWord,'function','relayStatusWord is exported');
  const relayStatusWord=live.relayStatusWord;
  const r={connected:true,authenticated:false,historyComplete:true,error:null};
  assert.equal(relayStatusWord({...r,authMode:'anonymous'}),'Live');
  assert.equal(relayStatusWord({...r,authenticated:true,authMode:'authenticated'}),'Live');
  assert.equal(relayStatusWord({...r,authenticated:true}),'Live');
  assert.equal(relayStatusWord({...r,authMode:'pending'}),'Polling');
  assert.equal(relayStatusWord(r),'Polling');
  assert.equal(relayStatusWord(undefined),'Polling');
});
