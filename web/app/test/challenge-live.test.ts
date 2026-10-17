import assert from 'node:assert/strict';
import test from 'node:test';
import {parseSnapshot,sellerLine,sellerMinutes,sellerState,renderSellers,type Snapshot} from '../src/challenge/live.js';
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
  renderSellers(doc as unknown as Document,{...s,sellers:[seller,denied,done,gone]},2920);
  const rows=list.children[0]!.children;assert.deepEqual(rows.map(r=>r.children[0]!.textContent),['<script>untrusted</script>','done','gone']);
  assert.deepEqual(rows.map(r=>r.children[3]!.textContent),['Working now','Done','Left']);assert.equal(rows[1]!.className,'off');assert.equal(summary.textContent,'3 agents · 1 working now');});
test('a seller whose job finished but has minutes left is Ready, not Done',()=>{const a={...seller,state:'done',minutesUsed:5};assert.equal(sellerState({...s,jobs:[]},a,2920),'Ready');assert.equal(sellerState({...s,jobs:[]},{...a,minutesUsed:60},2920),'Done');});

test('delivered running job is awaiting acceptance and conservative charge never ticks backwards',()=>{
  const a={...seller,state:'awaiting acceptance',minutesUsed:60};
  const snap={...s,sellers:[a],jobs:[{...s.jobs[0]!,usedMinutes:60}]};
  assert.equal(sellerState(snap,a,2980),'Awaiting acceptance');
  assert.equal(sellerMinutes(snap,a,2980),60);
});
