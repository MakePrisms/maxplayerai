// I-1 (round-3 review, run 2026-10-19-iso): a post whose offer never reached the relay looped
// forever. post_job answered `status=preparing`, the preparation then failed with "no relay accepted
// event", and every tick re-read the failed preparation (one MCP child per tick) past the deadline,
// the work window and the day stop, and again after a restart. These tests drive the runner against
// a model of the buyer whose texts and get_job shape are the buyer's own (see swarm.mjs QUEUED_OFFER).
// New exports are reached through `m` so the suite loads (and fails at its assertions) on old runners.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import * as m from './swarm.mjs';
const { Journal, Swarm, preparationId, jobBudget } = m;

const RELAY='wss://relay.maxplayer.ai';
const OFFER='e'.repeat(64);
const KEY=JSON.stringify(['b','day']);
// buyer/preparations.rs poll_store: the stored failure (job_lifecycle.rs publish_signed_event_async
// via JobLifecycleError::Relay) plus the note appended once an offer was recorded.
const noRelayText=offer=>`job lifecycle relay: no relay accepted event (${RELAY}: timeout) Offer ${offer} was queued for publication before this failure and may be live; auto-award was NOT armed. Inspect it with get_job job_id=${offer}.`;
// buyer/store/preparations.rs interrupt_preparations (daemon restart).
const restartQueuedText=offer=>`preparation interrupted by daemon restart after offer ${offer} was queued for publication: it may be live, and auto-award was NOT armed for it. Inspect it with get_job job_id=${offer} and award with award_claim. Repeating this post_job returns this message rather than posting again.`;
const restartNothingText='preparation interrupted by daemon restart before any offer was queued; nothing was published. Repeat the identical post_job to start again.';
// mcp-call.mjs turns a tool error (isError) into a failed child; command() reports its stderr.
const toolError=text=>Object.assign(Error(`${process.execPath} exited 1: ${JSON.stringify({content:[{type:'text',text}],isError:true})}\n`),{exitCode:1});
// buyer/mod.rs get_job for an offer the relay never stored: JobView (offer None, empty claims and
// results, read_confirmed from the answered probe) + award_read_confirmed, reservation_state "none"
// (no ledger row), auto_award_state null (put_pending_award runs only after a successful post),
// claim_prices, and mcp.rs with_ok. pending/publish_refused/awarded/awarded_delivery_pending are
// omitted when false/None.
const neverPublishedView=id=>({review_status:{},job_id:id,offer:null,claims:[],results:[],live_claim_id:null,accepted:null,read_confirmed:true,award_read_confirmed:true,reservation_state:'none',auto_award_state:null,claim_prices:[],ok:true});
const analysis={approach:'split',workstreams:[{id:'piece',description:'piece',needs:{skills:[]},estMinutes:1,dependsOn:[]}],verification:'check'};

function fixture({sellers=['a'],amountSats=5,durationSeconds=30,ends=1000,maxDuration=90,journal}={}) {
  let now=100;
  const snap={asOf:100,relay:{connected:true,authenticated:true,historyComplete:true},challenge:{buyer:'b',id:'day',starts:100,ends,status:'in_progress',goal:'solve',title:'test'},
    budget:{totalSats:10,unknownAmounts:0,committedSats:0,paidSats:0},jobs:[],
    sellers:sellers.map(pubkey=>({pubkey,eligible:true,decision:'accepted',state:'accepted',minutesRemaining:100,currentJobStart:null}))};
  const calls=[],reports=[],publishes=[],preps=new Map(),views=new Map(),assigned=new Set();
  // One assignment per seller, then wait: every post in these tests is the first one.
  const plan=x=>{
    if(x.mode==='analyze')return structuredClone(analysis);
    if(x.mode==='assess')return {wait:true};
    const free=x.sellers.filter(s=>!assigned.has(s.pubkey));free.forEach(s=>assigned.add(s.pubkey));
    return free.length?{assignments:free.map(s=>({seller:s.pubkey,workstream:'piece',task:'piece',durationSeconds,amountSats}))}:{wait:true,idleReason:'test: one task per seller'};
  };
  const f={snap,calls,reports,publishes,preps,views,fast:new Map(),
    journal:journal??{state:{jobs:[],days:{}},save(){this.saved=structuredClone(this.state);}}};
  f.io={now:()=>now,report:x=>reports.push({at:now,text:x}),snapshot:async()=>({...structuredClone(snap),asOf:now}),plan:async x=>plan(x),answer:async()=>'verified answer',
    publish:async(...a)=>publishes.push(a),latestChallenge:async()=>({...structuredClone(snap.challenge),budgetSats:snap.budget.totalSats}),
    mcp:async(tool,args)=>{
      calls.push({tool,args:structuredClone(args),at:now});
      if(tool==='post_job') {
        const h=preparationId(args.request_id);
        if(f.fast.has(args.seller_pubkey)){const id=f.fast.get(args.seller_pubkey);f.views.set(id,f.views.get(id)??{});return {job_id:id,ok:true};}
        if(!preps.has(h))preps.set(h,{state:'preparing',args});
        return {status:'preparing',preparation_id:h,next:'Call get_job with job_id=preparation_id; do not post another job.',ok:true};
      }
      if(tool==='get_job') {
        if(args.job_id.startsWith('preparation:')) {
          const p=preps.get(args.job_id);
          if(!p)throw toolError('unknown preparation handle');
          if(p.state==='failed')throw toolError(p.text);
          if(p.state==='posted')return {status:'posted',job_id:p.job,preparation_id:args.job_id,ok:true};
          return {status:'preparing',preparation_id:args.job_id,next:'Call get_job with job_id=preparation_id; do not post another job.',ok:true};
        }
        const v=views.get(args.job_id);
        if(v instanceof Error)throw v;
        return {...neverPublishedView(args.job_id),...(typeof v==='function'?v():v??{})};
      }
      if(tool==='collect')return {pay:{state:'Closed { attempt_id: "x" }',amount_sats:5}};
      throw Error(`unexpected ${tool}`);
    }};
  f.c={minDuration:30,maxDuration,duration:30,amount:0,updateEvery:180,awardAfter:5,awardFallback:false,plannerIdleSeconds:0};
  f.runner=new Swarm(f.c,f.io,f.journal);
  f.restart=(journal=f.journal)=>{f.journal=journal;f.runner=new Swarm(f.c,f.io,journal);};
  f.time=t=>{now=t;};
  f.run=async(from,to,step=5)=>{for(let t=from;t<=to;t+=step){now=t;await f.runner.tick();}};
  f.job=seller=>f.journal.state.jobs.find(j=>j.seller===seller);
  f.prepOf=seller=>[...preps.entries()].find(([,p])=>p.args.seller_pubkey===seller)?.[0];
  f.fail=(seller,text=noRelayText(OFFER))=>Object.assign(preps.get(f.prepOf(seller)),{state:'failed',text});
  f.count=(pred,from=-Infinity,to=Infinity)=>calls.filter(c=>c.at>=from&&c.at<=to&&pred(c)).length;
  f.reads=(id,from,to)=>f.count(c=>c.tool==='get_job'&&c.args.job_id===id,from,to);
  f.mcpCalls=(from,to)=>f.count(()=>true,from,to);
  f.posts=(from,to)=>f.count(c=>c.tool==='post_job',from,to);
  return f;
}

test('I-1: the buyer texts are classified (queued offer with/without relay, nothing published, unknown, other)',()=>{
  assert.equal(typeof m.preparationFailure,'function','swarm.mjs exports preparationFailure');
  assert.deepEqual(m.preparationFailure(toolError(noRelayText(OFFER)).message),{kind:'queued',offer:OFFER,relay:`${RELAY}: timeout`,reason:`no relay accepted the offer (${RELAY}: timeout)`});
  const restart=m.preparationFailure(restartQueuedText(OFFER));
  assert.equal(restart.kind,'queued');assert.equal(restart.offer,OFFER);assert.equal(restart.relay,null);
  assert.equal(m.preparationFailure(toolError(restartNothingText).message).kind,'nothing');
  assert.equal(m.preparationFailure(toolError('unknown preparation handle').message).kind,'unknown');
  assert.equal(m.preparationFailure('MCP timeout').kind,'error');
  assert.equal(m.relayUrl(`${RELAY}: timeout`),RELAY);
  assert.equal(m.relayUrl('ws://127.0.0.1:7777: connection refused; wss://b.example/x: timeout'),'ws://127.0.0.1:7777');
});

for(const amountSats of [5,0]) test(`I-1 repro (${amountSats?'priced':'free'} job): a preparation whose offer no relay accepted closes as "never published" at its deadline and stops polling`,async()=>{
  const f=fixture({amountSats});
  await f.run(100,100);
  const prep=f.job('a').id;
  assert.ok(prep.startsWith('preparation:'),'post answered preparing: the job holds the preparation handle');
  f.fail('a');
  // Past the deadline (130), the work window (1000, the day stops) and an hour more.
  await f.run(105,4600);
  const j=f.job('a'),prepReads=f.reads(prep),offerReads=f.reads(OFFER),late=f.mcpCalls(140,4600);
  const counts=`MCP calls: preparation reads ${prepReads}, offer reads ${offerReads}, any call after deadline+10s ${late}`;
  assert.equal(prepReads,1,`the failed preparation is read once, then never again (${counts})`);
  assert.ok(offerReads<=2,`the queued offer is read at most twice (${counts})`);
  assert.equal(late,0,`nothing is called once it closed (${counts})`);
  assert.equal(j.done,true,counts);assert.equal(j.released,true);assert.equal(j.outcome,'never published');
  assert.equal(j.id,OFFER);
  assert.deepEqual(jobBudget(j),{reservedSats:0,paidSats:0});
  assert.equal(f.journal.state.bench?.a,undefined,'the seller never saw the offer: not an unclaimed expiry');
  assert.equal(f.posts(),1,'never re-posted');
  assert.match(Object.values(f.journal.state.days)[0].stop.update,/^Unsolved: (work window ended|all eligible sellers exhausted)/);
  // Its own log lines, each once: no relay accepted (with the relay), then never published.
  assert.equal(f.reports.filter(r=>r.text.includes('NO RELAY ACCEPTED IT')&&r.text.includes(RELAY)).length,1);
  assert.equal(f.reports.filter(r=>r.text.includes('NEVER PUBLISHED')).length,1);
  assert.ok(!f.reports.some(r=>r.text.includes('QUEUED, MAY BE LIVE')),'distinct from "queued, may be live"');
  assert.match(j.answer,/never published/);
});

test('I-1: a restarted runner closes a journal holding the looping job (as in run 2026-10-19-iso) instead of resuming the loop',async()=>{
  const dir=mkdtempSync(join(tmpdir(),'swarm-i1-'));
  try {
    const path=join(dir,'events.jsonl'),rid='swarm-'+'1'.repeat(64);
    const challenge={buyer:'b',id:'day',starts:100,ends:1000,status:'in_progress',goal:'solve',title:'test'};
    // What a runner on 8d03b49c journaled: j.id is the preparation handle, the day has stopped.
    const seed=new Journal(path);
    seed.state={jobs:[{key:KEY,challenge,seller:'a',workstream:'piece',durationSeconds:30,created:100,reportBack:true,id:preparationId(rid),
      args:{task:'t',output:'text/plain',visibility:'public',seller_pubkey:'a',payment:'sat',amount_sats:5,max_sats:5,deadline_unix:130,request_id:rid}}],
      days:{[KEY]:{challenge,budget:10,lastUpdate:0,plan:analysis,stop:{update:'Unsolved: work window ended',result:''},published:true}}};
    seed.save();
    const f=fixture({journal:new Journal(path)});
    f.preps.set(preparationId(rid),{state:'failed',text:noRelayText(OFFER),args:seed.state.jobs[0].args});
    f.restart(new Journal(path));
    await f.run(4000,4000);
    let j=f.journal.state.jobs[0];
    assert.equal(j.done,true,`closed on the first tick after restart (calls ${JSON.stringify(f.calls.map(c=>[c.tool,c.args.job_id]))})`);
    assert.equal(j.outcome,'never published');assert.equal(j.released,true);
    assert.deepEqual(f.calls.map(c=>[c.tool,c.args.job_id]),[['get_job',preparationId(rid)],['get_job',OFFER]]);
    await f.run(4005,4600);
    assert.equal(f.mcpCalls(4005,4600),0,'no further MCP calls');
    // Durable: another restart from disk does nothing for it.
    f.restart(new Journal(path));await f.run(4605,4700);
    j=f.journal.state.jobs[0];
    assert.equal(j.done,true);assert.equal(f.mcpCalls(4605,4700),0);assert.equal(f.posts(),0);
    assert.deepEqual(jobBudget(j),{reservedSats:0,paidSats:0});
  } finally {rmSync(dir,{recursive:true});}
});

test('I-1: a restart mid-loop with an unconfirmed offer read keeps the sats reserved and backs off (60 s, then 300 s)',async()=>{
  const f=fixture();
  await f.run(100,100);f.fail('a');f.views.set(OFFER,{read_confirmed:false});
  await f.run(105,400);
  const before=f.reads(OFFER,105,400);
  f.restart();
  await f.run(405,1300);
  const j=f.job('a'),after=f.reads(OFFER,405,1300),firstAfter=f.calls.find(c=>c.at>=405&&c.args?.job_id===OFFER)?.at;
  assert.equal(j.done,false,'not proven absent: never released');
  assert.deepEqual(jobBudget(j),{reservedSats:5,paidSats:0});
  assert.ok(before<=5,`backed off before the restart: ${before} offer reads in 105..400`);
  assert.equal(firstAfter,405,'a restarted runner checks once at once');
  assert.ok(after<=5,`backed off after the restart: ${after} offer reads in 405..1300 (every tick would be 180)`);
  assert.equal(j.slotFreed,undefined,'an unconfirmed read never frees the seller slot (existing rule)');
  assert.match(j.outcome,/^unpublished: no relay accepted/);
  // The relay later answers: proof, closed and released, no repost.
  f.views.set(OFFER,{});await f.run(1305,1700);
  assert.equal(f.job('a').outcome,'never published');assert.deepEqual(jobBudget(f.job('a')),{reservedSats:0,paidSats:0});
  assert.equal(f.posts(),1);
});

test('I-1 (b): no release without the full proof; uncertain views stay reserved and only quiet ones back off',async()=>{
  const cases={
    'read not confirmed':{read_confirmed:false},
    'award read failed':{award_read_confirmed:false},
    'claim present':{claims:[{claim_id:'c',seller_pubkey:'a',live:false,status:'expired'}]},
    'reservation held':{reservation_state:'reserved'},
    'reservation unreadable':{reservation_state:null},
    'auto-award intent':{auto_award_state:'pending'},
    'accept present':{accepted:{}},
    'claims missing':{claims:undefined},
    'awarded':{awarded:{claim_id:'c',seller_pubkey:'a'}},
  };
  for(const [name,extra] of Object.entries(cases)) {
    const f=fixture();await f.run(100,100);f.fail('a');f.views.set(OFFER,extra);
    await f.run(105,1300);
    const j=f.job('a'),reads=f.reads(OFFER);
    assert.equal(j.done,false,name);assert.deepEqual(jobBudget(j),{reservedSats:5,paidSats:0},name);
    assert.notEqual(j.outcome,'never published',name);
    if(name==='awarded'||name==='accept present')assert.ok(reads>200,`${name}: possibly payable, read every tick (${reads})`);
    else assert.ok(reads<=8,`${name}: backed off (${reads} reads in 105..1300)`);
    assert.equal(f.journal.state.bench?.a,undefined,`${name}: not an unclaimed expiry`);
    assert.equal(f.posts(),1,name);
  }
});

for(const amountSats of [5,0]) test(`I-1 day stop while a preparation is still pending (${amountSats?'priced':'free'}): nothing new is posted, the preparation is polled slowly and closes at its deadline`,async()=>{
  const f=fixture({sellers:['a','b'],amountSats,durationSeconds:90});
  // a's post is slow (preparing); b's post publishes at once.
  f.fast.set('b','job-b');
  await f.run(100,100);
  assert.equal(f.posts(),2);
  f.snap.challenge.status='completed';  // external stop
  await f.run(105,300);
  const day=Object.values(f.journal.state.days)[0],prep=f.prepOf('a');
  assert.ok(day.stop,'stopped');
  assert.equal(f.posts(),2,'nothing new posted after the stop');
  const polls=f.reads(prep,115,300);
  assert.ok(polls<=3,`pending preparation polled ${polls} times in 115..300 (every tick would be 38)`);
  const a=f.job('a');
  if(amountSats) {
    assert.ok(!a.done,'still preparing past its deadline: no proof, stays reserved');
    assert.deepEqual(jobBudget(a),{reservedSats:5,paidSats:0});
    // The buyer then reports that no relay accepted the offer: closed at the next due check.
    f.fail('a');await f.run(305,800);
    assert.equal(f.job('a').outcome,'never published');assert.deepEqual(jobBudget(f.job('a')),{reservedSats:0,paidSats:0});
    assert.ok(f.mcpCalls(305,800)<=4,`at most a handful of calls in 305..800 (${f.mcpCalls(305,800)})`);
  } else {
    assert.equal(a.done,true,'a free preparation closes at its deadline');assert.equal(a.outcome,'expired/unpublished');
    assert.ok(f.reads(prep,195,300)<=1);
  }
  assert.equal(f.posts(),2);
});

test('I-1 day stop: an awarded job is still collected and paid exactly once after the stop, beside a never-published one',async()=>{
  const f=fixture({sellers:['a','b']});
  f.fast.set('a','job-a');
  await f.run(100,100);
  const deadline=f.job('a').args.deadline_unix;
  let delivered=false;
  f.views.set('job-a',()=>({offer:{event_id:'job-a',deadline_unix:deadline},claims:[{claim_id:'c',seller_pubkey:'a',live:!delivered,status:delivered?'delivered':'processing'}],
    results:delivered?[{seller_pubkey:'a',commit_oid:'0'.repeat(40)}]:[],awarded:{claim_id:'c',seller_pubkey:'a'},reservation_state:'reserved',auto_award_state:'awarded',...(delivered?{awarded_delivery_pending:true}:{})}));
  f.fail('b');
  await f.run(105,1100);                                  // window ends at 1000: the day stops
  assert.match(Object.values(f.journal.state.days)[0].stop.update,/^Unsolved: (work window ended|all eligible sellers exhausted)/);
  assert.equal(f.job('a').done,false,'awarded, undelivered: still open after the stop');
  assert.equal(f.job('b').outcome,'never published');
  delivered=true;await f.run(1105,1400);                  // late delivery, well past the deadline
  assert.equal(f.count(c=>c.tool==='collect'),1,'collected exactly once');
  assert.equal(f.job('a').outcome,'collected');assert.deepEqual(jobBudget(f.job('a')),{reservedSats:0,paidSats:5});
  assert.deepEqual(jobBudget(f.job('b')),{reservedSats:0,paidSats:0});
  assert.equal(f.posts(),2);
});

test('I-1: one WARNING naming the relay after 5 minutes without a published offer, no line repeated per tick',async()=>{
  const f=fixture({durationSeconds:1200,maxDuration:1800,ends:5000});
  await f.run(100,100);f.fail('a');
  await f.run(105,800);
  const warnings=f.reports.filter(r=>/^WARNING: Job .*offer never reached the relay: wss:\/\/relay\.maxplayer\.ai accepted no event/.test(r.text));
  assert.equal(warnings.length,1,f.reports.map(r=>r.text).join('\n'));
  assert.ok(warnings[0].at>=105+300&&warnings[0].at<=105+310,`warned at ${warnings[0].at}`);
  assert.ok(f.job('a').unpublished.warnedAt,'journaled: a restart does not warn again');
  const about=f.reports.filter(r=>r.text.includes(OFFER)||r.text.includes(f.prepOf('a'))).length;
  assert.ok(about<=6,`${about} log lines about the job in 105..800 (one per tick would be 140)`);
  assert.ok(f.reads(OFFER,105,800)<=6,`${f.reads(OFFER,105,800)} offer reads in 105..800`);
  assert.ok(!f.job('a').done,'before its deadline it stays open');
  f.restart();await f.run(805,1000);
  assert.equal(f.reports.filter(r=>r.text.startsWith('WARNING: Job')).length,1,'not repeated after a restart');
});

test('I-1: a preparation the buyer says failed before any offer was queued closes at once as never published; never re-posted',async()=>{
  const f=fixture({durationSeconds:90});
  await f.run(100,100);f.fail('a',restartNothingText);
  await f.run(105,400);
  const j=f.job('a');
  assert.equal(j.outcome,'never published');assert.equal(j.ts,105);assert.deepEqual(jobBudget(j),{reservedSats:0,paidSats:0});
  assert.equal(f.reads(f.prepOf('a')),1);assert.equal(f.posts(),1);
});

test('I-1: a restart-interrupted preparation that queued an offer is logged as "queued, may be live", not as no relay accepted',async()=>{
  const f=fixture();
  await f.run(100,100);f.fail('a',restartQueuedText(OFFER));
  f.views.set(OFFER,{offer:{event_id:OFFER,deadline_unix:130},claims:[]});
  await f.run(105,200);
  assert.equal(f.reports.filter(r=>r.text.includes('QUEUED, MAY BE LIVE')).length,1);
  assert.ok(!f.reports.some(r=>r.text.includes('NO RELAY ACCEPTED')));
  assert.equal(f.reports.filter(r=>r.text.includes('IS on the relay after all')).length,1);
  // Seen on the relay: the ordinary expiry rules apply (an expired offer with no claim, no award, no
  // reservation and no auto-award intent is released as expired/undelivered).
  assert.equal(f.job('a').outcome,'expired/undelivered');assert.equal(f.job('a').done,true);
});
