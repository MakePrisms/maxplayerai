import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, appendFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Journal, Swarm, childEnv, validatePlan, requestId, preparationId, config, reservedBudget, jobBudget } from './swarm.mjs';
const analysis={approach:'split',workstreams:[{id:'piece',description:'piece',needs:{skills:[]},estMinutes:1,dependsOn:[]}],verification:'check'};
const assignments=input=>({assignments:input.sellers.map(s=>({seller:s.pubkey,workstream:'piece',task:'piece',durationSeconds:30}))});
function fixture() {
  let now=100;
  const snap={asOf:100,relay:{connected:true,authenticated:true,historyComplete:true},challenge:{buyer:'b',id:'day',starts:100,ends:1000,status:'in_progress',goal:'solve',title:'test'},budget:{totalSats:10,unknownAmounts:0,committedSats:0,paidSats:0},jobs:[],sellers:['a','b'].map(pubkey=>({pubkey,eligible:true,decision:'accepted',state:'accepted',minutesRemaining:1,currentJobStart:null}))};
  const calls=[],publishes=[],inputs=[],remote=new Map();let planner=assignments;
  const journal={state:{jobs:[],days:{}},save(){this.saved=structuredClone(this.state);}};
  const io={now:()=>now,snapshot:async()=>({...structuredClone(snap),asOf:now}),plan:async x=>{inputs.push(x);return x.mode==='analyze'?structuredClone(analysis):x.mode==='assess'&&planner===assignments?{wait:true}:planner(x);},answer:async()=> 'verified answer',publish:async(...a)=>publishes.push(a),mcp:async(tool,args)=>{
    calls.push([tool,structuredClone(args)]);
    if(tool==='post_job') {if(!remote.has(args.request_id))remote.set(args.request_id,{id:`job-${remote.size}`,results:[]});return {job_id:remote.get(args.request_id).id};}
    if(tool==='get_job') {
      if(args.job_id.startsWith('preparation:')) {const entry=[...remote.entries()].find(([key])=>preparationId(key)===args.job_id);if(!entry)throw Error('unknown preparation handle');return {status:'posted',job_id:entry[1].id};}
      return [...remote.values()].find(j=>j.id===args.job_id)??{results:[]};
    }
    if(tool==='collect')return {pay:{state:'none',amount_sats:0}};
  }};
  const c={minDuration:30,maxDuration:90,duration:30,amount:0,updateEvery:180,awardAfter:5,awardFallback:false};
  const runner=new Swarm(c,io,journal);
  return {snap,c,io,runner,journal,calls,publishes,inputs,remote,setPlan:p=>{planner=p;},time:n=>{now=n;}};
}
test('both sellers get work automatically, never concurrently; every result reaches planner',async()=>{
  const f=fixture();await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='post_job').length,2);
  await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='post_job').length,2);
  for(const job of f.remote.values())job.results=[{}];
  f.setPlan(input=> input.results.length===2?{solved:true,result:'answer',summary:'verified'}:{wait:true});
  await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='collect').length,2);assert.equal(f.inputs.at(-1).results.length,2);
  assert.equal(f.publishes.at(-1)[2],'completed');assert.equal(f.publishes.at(-1)[4],'answer');
  await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='post_job').length,2);
});
test('ambiguous post retries exact durable intent after restart, never new request ID',async()=>{
  const f=fixture(),original=f.io.mcp;let fail=true;
  f.io.mcp=async(t,a)=>{const r=await original(t,a);if(t==='post_job'&&fail){fail=false;throw Error('lost reply');}return r;};
  await assert.rejects(f.runner.tick(),/lost reply/);
  const intent=structuredClone(f.journal.state.jobs[0].args);
  await new Swarm(f.c,f.io,f.journal).tick();
  assert.deepEqual(f.calls.filter(x=>x[0]==='post_job')[0][1],intent);assert.equal(f.calls.filter(x=>x[0]==='post_job').length,2);assert.equal(f.remote.size,2);
});
test('stop is durable before publication; restart retries completion and settles outstanding job',async()=>{
  const f=fixture();await f.runner.tick();f.setPlan(()=>({solved:true,result:'answer'}));
  // Free a seller while the other seller still has work.
  [...f.remote.values()][0].results=[{}];
  const publish=f.io.publish;f.io.publish=async()=>{throw Error('offline');};
  await assert.rejects(f.runner.tick(),/offline/);assert.ok(Object.values(f.journal.state.days)[0].stop);
  f.io.publish=publish;[...f.remote.values()][1].results=[{}];
  await new Swarm(f.c,f.io,f.journal).tick();assert.equal(f.calls.filter(x=>x[0]==='collect').length,2);assert.equal(f.remote.size,2);
});
test('exhaustion completes unsolved, but busy sellers and wait do not mean exhausted',async()=>{
  const f=fixture();f.setPlan(()=>({wait:true}));await f.runner.tick();assert.equal(f.publishes.length,0);
  f.snap.sellers.forEach(s=>s.minutesRemaining=.1);await f.runner.tick();assert.match(f.publishes.at(-1)[3],/Unsolved: all eligible sellers exhausted/);
});
test('work window ending stops new work but continues collecting',async()=>{
  const f=fixture();await f.runner.tick();f.time(1001);[...f.remote.values()][0].results=[{}];await f.runner.tick();assert.equal(f.remote.size,2);assert.match(f.publishes.at(-1)[3],/window ended/);assert.equal(f.calls.filter(x=>x[0]==='collect').length,1);
});
test('priced pending intents reserve budget serially, payment/max_sats are explicit',async()=>{
  const f=fixture();f.setPlan(input=>({assignments:assignments(input).assignments.map(a=>({...a,amountSats:6}))}));await f.runner.tick();assert.equal(f.remote.size,1);
  const args=f.calls.find(x=>x[0]==='post_job')[1];assert.equal(args.payment,'sat');assert.equal(args.max_sats,6);
  await f.runner.tick();assert.equal(f.remote.size,1);assert.equal(f.publishes.length,0);
});
test('fallback award is disabled by default and bounded when explicitly enabled',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.claims=[{live:true,seller_pubkey:j.id==='job-0'?'a':'b',claim_id:'claim'}];
  f.time(110);await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='award_claim').length,0);
  f.c.awardFallback=true;await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='award_claim').length,2);
});
test('planner latency rechecks deadline and changed acceptance immediately before post',async()=>{
  for(const change of [f=>f.time(999),f=>{f.snap.challenge.ends=110;},f=>f.snap.sellers.forEach(s=>s.decision='denied'),f=>f.snap.challenge.status='completed']){
    const f=fixture();f.setPlan(input=>{change(f);return assignments(input);});await f.runner.tick();assert.equal(f.remote.size,0);
  }
});
test('unready projection never posts or declares exhaustion',async()=>{
  const f=fixture();f.snap.relay.historyComplete=false;f.snap.sellers=[];await f.runner.tick();assert.equal(f.publishes.length,0);assert.equal(f.remote.size,0);
});
test('expired jobs are recorded and passed to planner',async()=>{
  const f=fixture();await f.runner.tick();f.time(131);f.setPlan(()=>({wait:true}));await f.runner.tick();assert.equal(f.inputs.at(-1).results.length,2);assert.match(f.inputs.at(-1).results[0].answer,/expired/);
});
test('preparation IDs are polled until posted without a second posting intent',async()=>{
  const f=fixture();let preparing=true;const orig=f.io.mcp;
  f.io.mcp=async(t,a)=>t==='post_job'?{preparation_id:'prep'}:t==='get_job'&&a.job_id==='prep'?(preparing?{status:'preparing'}:{status:'posted',job_id:'real'}):orig(t,a);
  await f.runner.tick();await f.runner.tick();assert.equal(f.journal.state.jobs.length,2);preparing=false;await f.runner.tick();assert.ok(f.journal.state.jobs.every(j=>j.id==='real'));
});
test('JSONL survives torn final writes and rejects corrupt committed data',()=>{
  const dir=mkdtempSync(join(tmpdir(),'swarm-test-')),path=join(dir,'events.jsonl');
  try{const j=new Journal(path);j.state.jobs.push({id:'one'});j.save();appendFileSync(path,'{"broken":');const recovered=new Journal(path);assert.equal(recovered.state.jobs[0].id,'one');assert.ok(readFileSync(path,'utf8').endsWith('\n'));appendFileSync(path,'bad\n');assert.throws(()=>new Journal(path));}finally{rmSync(dir,{recursive:true});}
});
test('planner responses, configuration and idempotency key validation',()=>{
  for(const p of [{},{task:'x',solved:true,result:'x'},{solved:true},{task:4},{task:' '}])assert.throws(()=>validatePlan(p));
  assert.deepEqual(validatePlan({wait:true}),{wait:true});
  const c=config([],{MAXPLAYER_HOME:'/tmp/buyer',RELAY_URL:'ws://localhost'});assert.equal(c.awardFallback,false);assert.equal(c.amount,undefined);
  assert.throws(()=>config([],{MAXPLAYER_HOME:'/tmp/b',RELAY_URL:'ws://localhost',SWARM_AMOUNT_SATS:'0'}));
  assert.throws(()=>config(['--amount-sats','-1'],{MAXPLAYER_HOME:'/tmp/b',RELAY_URL:'ws://localhost'}));
  assert.equal(requestId({buyer:'a',id:'b'},'c',1),requestId({buyer:'a',id:'b'},'c',1));assert.notEqual(requestId({buyer:'a',id:'b'},'c',1),requestId({buyer:'a',id:'b'},'c',2));
});
test('lost reply is recovered read-only after deadline and collected without another post',async()=>{
  const f=fixture(),original=f.io.mcp;let fail=true;
  f.io.mcp=async(t,a)=>{const r=await original(t,a);if(t==='post_job'&&fail){fail=false;throw Error('lost reply');}return r;};
  await assert.rejects(f.runner.tick());f.time(1001);[...f.remote.values()][0].results=[{}];
  await new Swarm(f.c,f.io,f.journal).tick();assert.equal(f.calls.filter(x=>x[0]==='post_job').length,1);assert.equal(f.calls.filter(x=>x[0]==='collect').length,1);
});
test('collected work waits for projection tally before receiving successive jobs',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.results=[{}];
  await f.runner.tick();assert.equal(f.remote.size,2);assert.equal(f.publishes.filter(x=>x[2]==='completed').length,0);
  f.snap.jobs=[...f.remote.values()].map((j,i)=>({id:j.id,result:{},accepted:true,seller:i?'b':'a',running:false,amountSats:0}));
  await f.runner.tick();assert.equal(f.remote.size,4);assert.ok(f.inputs.at(-1).results.every(r=>r.answer==='verified answer'));
});
test('progress is throttled independently from immediate completion publication',async()=>{
  const f=fixture();f.setPlan(()=>({wait:true}));f.time(200);await f.runner.tick();assert.equal(f.publishes.length,1);
  f.time(210);await f.runner.tick();assert.equal(f.publishes.length,1);f.time(381);await f.runner.tick();assert.equal(f.publishes.length,2);
  f.setPlan(()=>({solved:true,result:'verified'}));f.time(382);await f.runner.tick();assert.equal(f.publishes.length,3);assert.equal(f.publishes[2][2],'completed');
});
test('last paid result can solve even when budget is fully reserved',async()=>{
  const f=fixture();f.setPlan(input=>({assignments:assignments(input).assignments.map(a=>({...a,amountSats:10}))}));await f.runner.tick();assert.equal(f.remote.size,1);
  [...f.remote.values()][0].results=[{}];f.setPlan(()=>({solved:true,result:'last answer'}));await f.runner.tick();
  assert.equal(f.publishes.at(-1)[4],'last answer');assert.equal(f.remote.size,1);
});
test('one failed collection does not block settlement for another seller',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.results=[{}];
  const orig=f.io.mcp;f.io.mcp=async(t,a)=>{if(t==='collect'&&a.job_id==='job-0')throw Error('temporary failure');return orig(t,a);};
  await f.runner.tick();assert.equal(f.journal.state.jobs.filter(j=>j.done).length,1);assert.equal(f.journal.state.jobs.find(j=>j.done).id,'job-1');
});
test('settlement continues when relay is unavailable',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.results=[{}];
  f.io.snapshot=async()=>{throw Error('HTTP unavailable');};await assert.rejects(f.runner.tick(),/HTTP unavailable/);
  assert.equal(f.journal.state.jobs.filter(j=>j.done).length,2);
});
test('new public reservations during planning are included in the final budget gate',async()=>{
  const f=fixture();f.setPlan(input=>{f.snap.jobs.push({id:'foreign',amountSats:5});return {assignments:assignments(input).assignments.map(a=>({...a,amountSats:6}))};});
  await f.runner.tick();assert.equal(f.remote.size,0);
});
test('stub planner solves only after N successful results',async()=>{
  const {command}=await import('./swarm.mjs');
  const input={results:[{answer:'ok'},{answer:'[expired/undelivered]'}],mode:'assign',sellers:[{pubkey:'a',capabilities:[]}]};
  const invoke=async data=>JSON.parse(await command(process.execPath,['scripts/challenges/planner-stub.mjs'],{input:JSON.stringify(data),env:{...process.env,SWARM_STUB_SOLVE_AFTER:'2'}}));
  assert.equal((await invoke(input)).assignments.length,1);input.results.push({answer:'second'});assert.equal((await invoke(input)).solved,true);
});
test('real planner adapter passes noninteractive/tool-free arguments and verification prompt',async()=>{
  const {writeFileSync}=await import('node:fs'),{command}=await import('./swarm.mjs');
  const dir=mkdtempSync(join(tmpdir(),'swarm-planner-')),bin=join(dir,'fake-claude');
  try {
    writeFileSync(bin,`#!/usr/bin/env node\nconst fs=require('fs'),assert=require('assert/strict');const text=fs.readFileSync(0,'utf8');assert.deepEqual(process.argv.slice(2),['-p','--output-format','text','--tools','','--no-session-persistence']);assert.match(text,/BOTH keywords/);assert.match(text,/prior-answer/);console.log(JSON.stringify({wait:true}));\n`,{mode:0o700});
    const output=await command(process.execPath,['scripts/challenges/planner-llm.mjs'],{input:JSON.stringify({results:[{answer:'prior-answer'}]}),env:{...process.env,SWARM_LLM_BINARY:bin}});
    assert.deepEqual(JSON.parse(output),{wait:true});
  } finally {rmSync(dir,{recursive:true});}
});
test('journal I/O failure is fatal, never retried from unpersisted in-memory intent',()=>{
  const dir=mkdtempSync(join(tmpdir(),'swarm-journal-'));
  try{const j=new Journal(join(dir,'missing','events.jsonl'));assert.throws(()=>j.save(),e=>e.journalFailure===true);}finally{rmSync(dir,{recursive:true});}
});
test('sticky failed preparation recovers its queued offer and settles after stop',async()=>{
  const f=fixture(),id='d'.repeat(64);let posted=0,collected=0;
  const error=()=>Error(`rate-limited: quota exceeded. Offer ${id} was queued for publication before this failure and may be live; auto-award was NOT armed.`);
  f.io.mcp=async(t,a)=>{
    if(t==='post_job'){posted++;throw error();}
    if(t==='get_job'&&a.job_id.startsWith('preparation:'))throw error();
    if(t==='get_job'){assert.equal(a.job_id,id);return {results:[{}]};}
    if(t==='collect'){collected++;return {pay:{state:'none'}};}
  };
  await assert.rejects(f.runner.tick(),/quota/);
  f.snap.challenge.status='completed';f.time(150);await f.runner.tick();
  assert.equal(posted,1);assert.equal(collected,1);assert.equal(f.journal.state.jobs[0].outcome,'collected');
});
test('permanently refused publication is recorded and does not hold seller forever',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.publish_refused='invalid event';
  f.setPlan(()=>({wait:true}));await f.runner.tick();
  assert.ok(f.journal.state.jobs.every(j=>j.done&&j.outcome.includes('publication refused')));
  assert.ok(f.inputs.at(-1).results.every(r=>r.answer.includes('publication refused')));
});

test('analysis is durable across disk journal restart; revisions reach every later call',async()=>{
  const f=fixture(),dir=mkdtempSync(join(tmpdir(),'swarm-analysis-')),path=join(dir,'events.jsonl');
  try {
    const journal=new Journal(path);await new Swarm(f.c,f.io,journal).tick();
    assert.deepEqual(new Journal(path).state.days[JSON.stringify(['b','day'])].plan,analysis);
    const revised={...analysis,approach:'revised'};
    f.setPlan(()=>({wait:true,plan:revised}));await new Swarm(f.c,f.io,new Journal(path)).tick();
    await new Swarm(f.c,f.io,new Journal(path)).tick();
    assert.equal(f.inputs.filter(i=>i.mode==='analyze').length,1);
    assert.equal(f.inputs.at(-1).plan.approach,'revised');
    assert.equal(f.inputs.at(-1).running.length,2);
  } finally {rmSync(dir,{recursive:true});}
});
test('batch rejects invalid duration, busy/duplicate, ineligible and oversized minute assignments with reasons',async()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);
  f.snap.sellers.push({pubkey:'denied',eligible:false,decision:'denied',minutesRemaining:100});
  f.setPlan(()=>({assignments:[
    {seller:'a',workstream:'piece',task:'valid',durationSeconds:30},
    {seller:'a',workstream:'piece',task:'duplicate',durationSeconds:30},
    {seller:'b',workstream:'piece',task:'long',durationSeconds:91},
    {seller:'b',workstream:'piece',task:'minutes',durationSeconds:61},
    {seller:'b',workstream:'piece',task:'short',durationSeconds:29},
    {seller:'denied',workstream:'piece',task:'ineligible',durationSeconds:30}
  ]}));
  await f.runner.tick();assert.equal(f.remote.size,1);assert.equal(reports.length,5);
  assert.ok(reports.some(r=>r.includes('insufficient_minutes')));
  assert.ok(reports.some(r=>r.includes('busy')));
  await f.runner.tick();assert.equal(f.remote.size,1);
});
test('stub routes Rust work only to advertised Rust seller with distinct durations and full profiles',async()=>{
  const {command}=await import('./swarm.mjs');const f=fixture();
  f.snap.sellers[0].capabilities=[['capabilities','rust','python'],['harness_family','codex'],['harness_model','model'],['harness_variant','fast'],['hardware','cpu']];
  f.snap.sellers[0].npub='npub-a';f.snap.sellers[0].note='hello';
  f.io.plan=async input=>{f.inputs.push(input);return JSON.parse(await command(process.execPath,['scripts/challenges/planner-stub.mjs'],{input:JSON.stringify(input)}));};
  await f.runner.tick();
  assert.equal(f.journal.state.jobs.find(j=>j.seller==='a').workstream,'rust');
  assert.equal(f.journal.state.jobs.find(j=>j.seller==='b').workstream,'research');
  assert.deepEqual(f.journal.state.jobs.map(j=>j.durationSeconds),[40,30]);
  assert.deepEqual(f.journal.state.jobs.map(j=>j.args.amount_sats),[5,0]);
  const profile=f.inputs[0].sellers[0];assert.equal(profile.npub,'npub-a');assert.deepEqual(profile.harness.family,['codex']);assert.equal(profile.note,'hello');
  for(const j of f.remote.values())j.results=[{}];
  f.snap.jobs=[...f.remote.values()].map(j=>({id:j.id,result:{},running:false,accepted:true}));
  f.time(110);await f.runner.tick();
  assert.equal(f.inputs.at(-1).sellers[0].trackRecord.delivered,1);
  assert.equal(f.inputs.at(-1).sellers[0].trackRecord.tasks[0].turnaroundSeconds,10);
});
test('analyze includes busy eligible sellers but assignment roster excludes them; idle choice is not exhaustion',async()=>{
  const f=fixture();f.snap.sellers[0].currentJobStart=95;f.snap.jobs=[{id:'external',seller:'a',running:true,start:95,task:'ongoing'}];
  f.setPlan(()=>({assignments:[]}));await f.runner.tick();await f.runner.tick();
  assert.equal(f.inputs[0].sellers.length,2);assert.deepEqual(f.inputs.at(-1).sellers.map(s=>s.pubkey),['b']);
  assert.equal(f.inputs.at(-1).running[0].since,95);assert.equal(f.publishes.length,0);
});

test('a current-job marker prevents exhaustion even before the job is projected',async()=>{
  const f=fixture();f.snap.sellers.forEach(s=>{s.currentJobStart=95;});
  f.setPlan(()=>({wait:true}));await f.runner.tick();assert.equal(f.publishes.length,0);
});
test('pending work from a previous challenge still blocks a seller',async()=>{
  const f=fixture();await f.runner.tick();f.snap.challenge.id='next';await f.runner.tick();
  assert.equal(f.remote.size,2);assert.equal(f.journal.state.days[JSON.stringify(['b','next'])].stop,undefined);
});

test('maxplayer child env carries only MAXPLAYER_HOME from the MAXPLAYER_* namespace',()=>{
  const e=childEnv({PATH:'/bin',MAXPLAYER_BINARY:'/x',MAXPLAYER_RELAY_URL:'ws://y',MAXPLAYER_HOME:'/old'},'/buyer');
  assert.deepEqual(e,{PATH:'/bin',MAXPLAYER_HOME:'/buyer'});
  assert.equal(config([],{SWARM_MAXPLAYER_BINARY:'/bin/mp',MAXPLAYER_HOME:'/h',RELAY_URL:'ws://a'}).binary,'/bin/mp');
});

const priced=(input,amount=5)=>({assignments:assignments(input).assignments.map(a=>({...a,amountSats:amount}))});
test('mixed batch uses normal priced and free posting arguments and reserves before post',async()=>{
  const f=fixture(),orig=f.io.mcp;
  f.setPlan(input=>({assignments:assignments(input).assignments.map((a,i)=>({...a,amountSats:i?5:0}))}));
  f.io.mcp=async(t,a)=>{if(t==='post_job')assert.deepEqual(f.journal.saved.jobs.at(-1).args,a);return orig(t,a);};
  await f.runner.tick();
  assert.deepEqual(f.calls.filter(([t])=>t==='post_job').map(([,a])=>[a.payment,a.amount_sats,a.max_sats]),[['none',0,0],['sat',5,5]]);
  assert.equal(reservedBudget(f.snap,f.journal.state.jobs,100).committedSats,5);
});
test('invalid and over-budget prices are dropped with reasons',async()=>{
  for(const amountSats of [-1,1.5,'5',null,Number.MAX_SAFE_INTEGER+1,11]){
    const f=fixture(),logs=[];f.io.report=x=>logs.push(x);f.setPlan(input=>priced(input,amountSats));await f.runner.tick();
    assert.equal(f.remote.size,0);assert.ok(logs.some(x=>x.includes(amountSats===11?'budget_exceeded':'amountSats')));
  }
});
test('confirmed expired never-awarded job releases reservation, incomplete/possibly payable states do not',async()=>{
  for(const uncertainty of [null,'unarmed-null','unarmed-missing','read','award-read','awarded','accepted','claims','results','offer','reservation','auto-award']){
    const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);f.setPlan(input=>priced(input));await f.runner.tick();
    const state=[...f.remote.values()][0];Object.assign(state,{read_confirmed:true,award_read_confirmed:true,reservation_state:'none',auto_award_state:'parked',offer:{deadline_unix:130},claims:[],results:[],accepted:null});
    if(uncertainty==='unarmed-null')state.auto_award_state=null;
    if(uncertainty==='unarmed-missing')delete state.auto_award_state;
    if(uncertainty==='auto-award')state.auto_award_state='pending';
    if(uncertainty==='reservation')state.reservation_state='reserved';
    if(uncertainty==='read')state.read_confirmed=false;
    if(uncertainty==='award-read')state.award_read_confirmed=false;
    if(uncertainty==='awarded')state.awarded={claim_id:'x'};
    if(uncertainty==='accepted')state.accepted={};
    if(uncertainty==='claims')state.claims=[{live:false}];
    if(uncertainty==='results')delete state.results;
    if(uncertainty==='offer')delete state.offer;
    f.time(131);f.setPlan(()=>({wait:true}));await f.runner.tick();
    assert.equal(reservedBudget(f.snap,f.journal.state.jobs,131).committedSats,uncertainty&&!uncertainty.startsWith('unarmed')?5:0,uncertainty);
    assert.equal(f.journal.state.jobs[0].done,!uncertainty||uncertainty.startsWith('unarmed'));
  }
});
test('expired awarded job stays polled and a late collect still consumes budget exactly once',async()=>{
  const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);f.setPlan(input=>priced(input));await f.runner.tick();
  const state=[...f.remote.values()][0];state.awarded={};f.time(131);f.setPlan(()=>({wait:true}));await f.runner.tick();
  assert.equal(f.journal.state.jobs[0].done,false);
  state.results=[{}];const orig=f.io.mcp;f.io.mcp=async(t,a)=>t==='collect'?{pay:{state:'Closed { attempt_id: "x" }',amount_sats:5}}:orig(t,a);
  await f.runner.tick();await f.runner.tick();
  assert.deepEqual(jobBudget(f.journal.state.jobs[0]),{paidSats:5,reservedSats:0});
  assert.equal(f.inputs.at(-1).budgetHeadroomSats,5);
});
test('disk restart retains paid reservations, including ambiguous posts',async()=>{
  const f=fixture(),dir=mkdtempSync(join(tmpdir(),'swarm-pay-')),path=join(dir,'journal');
  try {
    const journal=new Journal(path);f.setPlan(input=>priced(input,6));const orig=f.io.mcp;
    f.io.mcp=async(t,a)=>{const result=await orig(t,a);if(t==='post_job')throw Error('lost reply');return result;};
    await assert.rejects(new Swarm(f.c,f.io,journal).tick(),/lost reply/);
    f.io.mcp=orig;const recovered=new Journal(path);await new Swarm(f.c,f.io,recovered).tick();
    assert.equal(f.remote.size,1);assert.equal(reservedBudget(f.snap,recovered.state.jobs,100).committedSats,6);
  } finally {rmSync(dir,{recursive:true});}
});
test('price refusal and seller pricing/spend reach planner without prematurely releasing',async()=>{
  const f=fixture();f.snap.sellers[0].capabilities=[['rate','9'],['takes_payment','none']];
  f.setPlan(input=>priced(input));await f.runner.tick();
  const state=[...f.remote.values()][0];state.claim_prices=[{seller_pubkey:'a',amount_sats:9}];
  f.setPlan(()=>({wait:true}));await f.runner.tick();
  assert.equal(f.inputs.find(i=>i.mode==='analyze').sellers[0].pricing.rateSats[0],'9');
  assert.ok(f.inputs.at(-1).results.some(r=>r.outcome==='price too high: asked 9'));
  assert.equal(f.inputs.at(-1).budgetTotalSats,10);assert.equal(f.inputs.at(-1).budgetHeadroomSats,0);
  assert.equal(jobBudget(f.journal.state.jobs[0]).reservedSats,5);
});
test('zero budget still funds free work; paid-only idle plan stops, wait does not',async()=>{
  for(const mode of ['free','paid','wait']){
    const f=fixture();f.snap.budget.totalSats=0;
    if(mode==='paid')f.setPlan(input=>priced(input));if(mode==='wait')f.setPlan(()=>({wait:true}));
    await f.runner.tick();assert.equal(f.remote.size,mode==='free'?2:0);
    assert.equal(Boolean(Object.values(f.journal.state.days)[0].stop),mode==='paid');
  }
});
test('settled free receipt releases a legacy reservation; unknown payment state fails closed',()=>{
  assert.deepEqual(jobBudget({args:{max_sats:5},done:true,pay:{state:'none',amount_sats:0}}),{reservedSats:0,paidSats:0});
  assert.deepEqual(jobBudget({args:{max_sats:5},done:true,pay:{state:'Sent',amount_sats:5}}),{reservedSats:5,paidSats:0});
});

test('expired undelivered award releases only after buyer ledger reconciliation',async()=>{
  const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);f.setPlan(input=>priced(input));await f.runner.tick();
  const state=[...f.remote.values()][0];Object.assign(state,{read_confirmed:true,offer:{deadline_unix:130},claims:[{live:false}],results:[],accepted:null,awarded:{},reservation_state:'reserved'});
  f.time(131);f.setPlan(()=>({wait:true}));await f.runner.tick();assert.equal(jobBudget(f.journal.state.jobs[0]).reservedSats,5);
  state.reservation_state='released';await f.runner.tick();assert.equal(jobBudget(f.journal.state.jobs[0]).reservedSats,0);
  assert.equal(f.journal.state.jobs[0].done,true);
});

test('lowered budget does not block free work and invalid paid assignments do not mean exhaustion',async()=>{
  const f=fixture();f.snap.budget.totalSats=0;f.snap.jobs=[{id:'external',amountSats:5}];
  await f.runner.tick();assert.equal(f.remote.size,2);
  const g=fixture();g.snap.budget.totalSats=0;
  g.setPlan(()=>({assignments:[{seller:'missing',amountSats:5}]}));await g.runner.tick();
  assert.equal(Object.values(g.journal.state.days)[0].stop,undefined);
});

test('expired over-price never-awarded claim releases and leaves its price outcome for the buyer',async()=>{
  const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);f.setPlan(input=>priced(input));await f.runner.tick();
  const state=[...f.remote.values()][0];Object.assign(state,{read_confirmed:true,award_read_confirmed:true,reservation_state:'none',auto_award_state:'parked',offer:{deadline_unix:130},claims:[{live:false,status:'expired'}],claim_prices:[{seller_pubkey:'a',amount_sats:9}],results:[],accepted:null});
  f.time(131);f.setPlan(()=>({wait:true}));await f.runner.tick();
  assert.equal(jobBudget(f.journal.state.jobs[0]).reservedSats,0);
  assert.equal(f.inputs.at(-1).results[0].outcome,'price too high: asked 9');
  assert.equal(f.inputs.at(-1).sellers[0].challengeSpend.reservedSats,0);
});

test('mixed successful collections reduce budget by exactly paid sats, never public estimates twice',async()=>{
  const f=fixture();f.setPlan(input=>({assignments:assignments(input).assignments.map((a,i)=>({...a,amountSats:i?5:0}))}));
  await f.runner.tick();for(const state of f.remote.values())state.results=[{}];
  const orig=f.io.mcp;f.io.mcp=async(t,a)=>t==='collect'?{pay:a.job_id==='job-1'?{state:'Closed { attempt_id: "paid" }',amount_sats:5}:{state:'none',amount_sats:0}}:orig(t,a);
  f.snap.jobs=f.journal.state.jobs.map(j=>({id:j.id,seller:j.seller,result:{},amountSats:j.args.amount_sats,accepted:true}));
  f.setPlan(()=>({wait:true}));await f.runner.tick();await new Swarm(f.c,f.io,f.journal).tick();
  const budget=reservedBudget(f.snap,f.journal.state.jobs,100);
  assert.equal(budget.committedSats,0);assert.equal(budget.paidSats,5);
  assert.equal(f.inputs.at(-1).budgetHeadroomSats,5);
  assert.deepEqual(f.inputs.at(-1).sellers.map(s=>s.challengeSpend),[{paidSats:0,reservedSats:0},{paidSats:5,reservedSats:0}]);
});

test('runner uses public challenge defaults independently of the trade URL',()=>{
  const env={MAXPLAYER_HOME:'/tmp/buyer',RELAY_URL:'ws://trade'};
  assert.deepEqual(config([],env).challengeRelays,['wss://nos.lol','wss://relay.primal.net','wss://offchain.pub']);
  const c=config([],{...env,SWARM_CHALLENGE_RELAYS:'ws://challenge-a, ws://challenge-b,ws://challenge-a'});
  assert.equal(c.url,'ws://trade');assert.deepEqual(c.challengeRelays,['ws://challenge-a','ws://challenge-b']);
  assert.throws(()=>config([],{...env,SWARM_CHALLENGE_RELAYS:'https://bad'}));
});

// Exercise the same pure projection as the page (transport signature checks are separate).
async function projectedFixture({lateMember=false,backdated=false,ordinary=false,honest=false,accepted=false}={}) {
  const {snapshot}=await import('../../web/app/src/challenge/shared/model.mjs');
  const buyer='b'.repeat(64),seller='a'.repeat(64),offer='1'.repeat(64),claim='2'.repeat(64);
  const e=(kind,pubkey,created_at,tags,content='')=>({kind,pubkey,created_at,tags:[['t','maxplayer'],['v','1'],...tags],content,id:String(kind).padStart(64,'0')});
  const events=[e(30341,buyer,1,[['d','day'],['title','Test'],['starts','100'],['ends','1000'],['budget','10','sat'],['status','in_progress']]),
    e(30342,seller,lateMember?101:1,[['d',buyer],['p',buyer],['since','1'],['minutes',honest?'60':'1'],['status','joined']]),
    e(30343,buyer,2,[['d',seller],['p',seller],['decision','accepted']])];
  if(backdated||ordinary||honest)events.push({...e(3401,buyer,100,[['p',seller],['i',ordinary?'Normal job':`[challenge:${buyer}:day]\nTask`],['param','deadline',honest?'1000':'160'],['param','payment','none']]),id:offer},
    {...e(3402,seller,100,[['e',offer,'','root'],['p',buyer],['status','processing']]),id:claim},
    e(3405,buyer,100,[['e',offer,'','root'],['e',claim],['p',buyer],['p',seller],['status','accepted']]),
    e(3403,seller,honest?101:99,[['e',offer,'','root'],['p',buyer]],'Done'));
  if(accepted)events.push(e(3406,buyer,160,[['e',offer,'','root'],['e',claim],['p',buyer],['p',seller],['status','accepted']]));
  const f=fixture();f.project=now=>snapshot(events,buyer,now);Object.assign(f.snap,snapshot(events,buyer,200));f.time(200);
  return f;
}
test('late membership with old since and prior acceptance cannot post',async()=>{
  const f=await projectedFixture({lateMember:true});await f.runner.tick();assert.equal(f.remote.size,0);
});
test('backdated result consumes offered minutes and swarm posts no further job',async()=>{
  const f=await projectedFixture({backdated:true});await f.runner.tick();assert.equal(f.remote.size,0);assert.equal(f.snap.sellers[0].minutesUsed,1);
});
test('ordinary buyer jobs do not consume challenge tally or minutes',async()=>{
  const f=await projectedFixture({ordinary:true});assert.equal(f.snap.jobs.length,0);assert.equal(f.snap.sellers[0].minutesUsed,0);
});

test('honest result without ACCEPT keeps seller busy and prevents post_job',async()=>{
  const f=await projectedFixture({honest:true});await f.runner.tick();
  assert.equal(f.calls.filter(([t])=>t==='post_job').length,0);
  assert.equal(f.snap.sellers[0].state,'awaiting acceptance');
});
test('ACCEPT freezes first job minutes and later assignment fits donated remainder',async()=>{
  const f=await projectedFixture({honest:true,accepted:true});await f.runner.tick();
  assert.equal(f.snap.jobs[0].usedMinutes,1);assert.equal(f.project(900).jobs[0].usedMinutes,1);
  const posts=f.calls.filter(([t])=>t==='post_job');assert.equal(posts.length,1);
  assert.ok(f.snap.jobs[0].usedMinutes+(posts[0][1].deadline_unix-200)/60<=60);
});
test('collected journal job waits for ACCEPT projection rather than RESULT projection',async()=>{
  const f=fixture();await f.runner.tick();
  for(const job of f.remote.values())job.results=[{}];
  f.snap.jobs=[...f.remote.values()].map(j=>({id:j.id,result:{},running:false,accepted:false}));
  await f.runner.tick();assert.equal(f.calls.filter(([t])=>t==='post_job').length,2);
});

test('planner running roster includes collected work while projected meter runs',async()=>{
  const f=fixture();await f.runner.tick();
  for(const job of f.remote.values())job.results=[{}];
  f.snap.jobs=f.journal.state.jobs.map(j=>({id:j.id,seller:j.seller,result:{},running:true,accepted:false,start:100,deadline:1000}));
  await f.runner.tick();
  assert.equal(f.inputs.at(-1).running.length,2);
  assert.equal(f.calls.filter(([t])=>t==='post_job').length,2);
});

// Roster uncertainty blocks exhaustion through readiness only; display warnings never do.
test('metadata/applicant warnings do not block exhaustion; incomplete trusted history does',async()=>{
  const f=fixture();f.snap.sellers=[];f.setPlan(()=>({wait:true}));
  f.snap.relay.historyComplete=false;await f.runner.tick();assert.ok(!f.publishes.some(p=>p[2]==='completed'));
  Object.assign(f.snap.relay,{historyComplete:true,warnings:['Profile cap: metadata shown for 256 of 300 accepted sellers'],applicantsIncomplete:true});
  await f.runner.tick();assert.equal(f.publishes.at(-1)?.[2],'completed','display warnings must not block exhaustion');assert.match(f.publishes.at(-1)[3],/exhausted/);
});

// A relay coverage gap between planning and posting (relay_not_ready only) defers the assignment.
const gapOnce=(f,plan=assignments)=>{let first=true;return input=>{if(!first)return {wait:true};first=false;f.snap.relay.failedRelays=['ws://challenge'];return plan(input);};};
const posts=f=>f.calls.filter(([t])=>t==='post_job');
test('deferred: relay gap between planning and posting posts exactly once after the relay is ready again',async()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);f.setPlan(gapOnce(f));
  await f.runner.tick();
  assert.equal(posts(f).length,0);
  assert.deepEqual(f.journal.saved.jobs.map(j=>[j.seller,j.deferred,j.id]),[['a',true,undefined],['b',true,undefined]]);
  assert.ok(reports.some(r=>r.includes('Deferred assignment for a: relay_not_ready')));
  f.time(103);await f.runner.tick();assert.equal(posts(f).length,0,'never posts while relay_not_ready holds');
  // An unready snapshot (e.g. reader restarting, history not yet loaded) is not evidence against it.
  const snapshot=f.io.snapshot;f.io.snapshot=async()=>({...await snapshot(),challenge:null,sellers:[],budget:null});
  f.time(104);await f.runner.tick();f.io.snapshot=snapshot;
  assert.equal(posts(f).length,0);assert.ok(f.journal.state.jobs.every(j=>j.deferred&&!j.done));
  f.snap.relay.failedRelays=[];f.time(105);await f.runner.tick();
  assert.deepEqual(posts(f).map(([,a])=>[a.seller_pubkey,a.deadline_unix,a.request_id]),[['a',135,requestId(f.snap.challenge,'a',1)],['b',135,requestId(f.snap.challenge,'b',1)]]);
  assert.ok(f.journal.state.jobs.every(j=>j.id&&j.deferred===false&&j.created===105));
  f.time(106);await f.runner.tick();assert.equal(posts(f).length,2);
});
test('deferred: retry re-runs the full serial budget admission; a deferral reserves nothing',async()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);f.setPlan(gapOnce(f,input=>priced(input,6)));
  await f.runner.tick();
  assert.equal(f.journal.state.jobs.filter(j=>j.deferred).length,2);
  assert.equal(reservedBudget(f.snap,f.journal.state.jobs,100).committedSats,0);
  f.snap.relay.failedRelays=[];f.time(105);await f.runner.tick();
  assert.deepEqual(posts(f).map(([,a])=>[a.seller_pubkey,a.max_sats]),[['a',6]]);
  assert.match(f.journal.state.jobs[1].outcome,/abandoned deferred assignment: .*budget_exceeded/);
  assert.equal(reservedBudget(f.snap,f.journal.state.jobs,105).committedSats,6);
});
test('deferred: relay not ready past max wait or work window, or another reason appears, drops it unposted',async()=>{
  for(const [bound,expected] of [['wait',/not ready within 60s/],['window',/work window exceeded/],['minutes',/insufficient_minutes/],['stop',/not_in_progress/]]){
    const f=fixture(),reports=[];f.io.report=m=>reports.push(m);f.c.maxDefer=60;
    if(bound==='window')f.snap.challenge.ends=150;
    f.setPlan(gapOnce(f));await f.runner.tick();
    assert.ok(f.journal.state.jobs.length===2&&f.journal.state.jobs.every(j=>j.deferred&&!j.done),bound);
    // Other reasons are judged only on a ready snapshot.
    if(bound==='minutes'){f.snap.sellers.forEach(s=>s.minutesRemaining=.1);f.snap.relay.failedRelays=[];}
    if(bound==='stop'){f.snap.challenge.status='completed';f.snap.relay.failedRelays=[];}
    f.time({wait:161,window:121,minutes:105,stop:105}[bound]);await f.runner.tick();
    assert.ok(f.journal.state.jobs.every(j=>j.done&&j.released&&expected.test(j.outcome)),bound);
    assert.ok(reports.some(r=>r.startsWith('Dropped deferred assignment for a')),bound);
    f.snap.relay.failedRelays=[];await f.runner.tick();assert.equal(posts(f).length,0,bound);
  }
});
test('deferred: seller leaving, denied or ineligible while deferred is dropped; others still post',async()=>{
  for(const change of [s=>{s.state='left';},s=>{s.decision='denied';},s=>{s.eligible=false;}]){
    const f=fixture();f.setPlan(gapOnce(f));await f.runner.tick();
    // The change lands during the gap; nothing is judged or posted until the relay is ready.
    change(f.snap.sellers[0]);f.time(102);await f.runner.tick();
    assert.ok(f.journal.state.jobs.every(j=>j.deferred&&!j.done));assert.equal(posts(f).length,0);
    f.snap.relay.failedRelays=[];f.time(104);await f.runner.tick();
    assert.match(f.journal.state.jobs[0].outcome,/abandoned deferred assignment: .*seller busy or ineligible/);
    assert.equal(f.journal.state.jobs[0].id,undefined);
    assert.deepEqual(posts(f).map(([,a])=>a.seller_pubkey),['b']);
  }
});
test('deferred: restart with a journaled deferral posts once; a lost reply after un-deferral recovers read-only',async()=>{
  const f=fixture(),dir=mkdtempSync(join(tmpdir(),'swarm-defer-')),path=join(dir,'events.jsonl');
  try {
    f.snap.sellers=f.snap.sellers.slice(0,1);f.setPlan(gapOnce(f));
    await new Swarm(f.c,f.io,new Journal(path)).tick();
    assert.equal(new Journal(path).state.jobs[0].deferred,true);assert.equal(posts(f).length,0);
    f.snap.relay.failedRelays=[];f.time(105);
    const orig=f.io.mcp;let fail=true;
    f.io.mcp=async(t,a)=>{const r=await orig(t,a);if(t==='post_job'&&fail){fail=false;throw Error('lost reply');}return r;};
    await new Swarm(f.c,f.io,new Journal(path)).tick();
    const intent=new Journal(path).state.jobs[0];assert.equal(intent.deferred,false);assert.equal(intent.id,undefined);
    f.time(106);await new Swarm(f.c,f.io,new Journal(path)).tick();
    f.time(107);await new Swarm(f.c,f.io,new Journal(path)).tick();
    assert.equal(posts(f).length,1);assert.equal(f.remote.size,1);
    assert.deepEqual(posts(f)[0][1],intent.args);assert.equal(new Journal(path).state.jobs[0].id,'job-0');
  } finally {rmSync(dir,{recursive:true});}
});
test('deferred: seller stays busy (not in assign roster, shown running, no second assignment)',async()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);f.setPlan(gapOnce(f));await f.runner.tick();
  // Retry reads still see the gap; the planning snapshot is ready.
  let reads=0;const snapshot=f.io.snapshot;f.io.snapshot=async()=>{const s=await snapshot();if(reads++<2)s.relay.failedRelays=['ws://challenge'];return s;};
  f.snap.relay.failedRelays=[];f.setPlan(()=>({assignments:[{seller:'a',workstream:'piece',task:'second',durationSeconds:30}]}));
  f.time(101);await f.runner.tick();
  const assign=f.inputs.filter(i=>i.mode==='assign').at(-1);
  assert.deepEqual(assign.sellers,[]);assert.deepEqual(assign.running.map(r=>[r.seller,r.deferred]),[['a',true],['b',true]]);
  assert.ok(reports.some(r=>r.includes('Rejected assignment for a: seller busy')));
  assert.equal(posts(f).length,0);assert.equal(f.journal.state.jobs.length,2);
  // Within one gap batch a duplicate for the deferred seller is busy, not deferred twice.
  const g=fixture();g.setPlan(gapOnce(g,()=>({assignments:['first','second'].map(task=>({seller:'a',workstream:'piece',task,durationSeconds:30}))})));
  await g.runner.tick();assert.deepEqual(g.journal.state.jobs.map(j=>[j.seller,j.args.task.split('\n').at(-1)]),[['a','first']]);
});
