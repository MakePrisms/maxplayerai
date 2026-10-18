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
  await f.runner.tick();assert.equal(f.remote.size,1);assert.equal(reports.filter(r=>r.startsWith('Rejected assignment')).length,5);
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
  for(const [bound,expected] of [['wait',/still not ready after 60s/],['window',/work window exceeded/],['minutes',/insufficient_minutes/],['stop',/not_in_progress/]]){
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
  await g.runner.tick();assert.deepEqual(g.journal.state.jobs.map(j=>[j.seller,j.args.task.split('\n')[1]]),[['a','first']]);
});

// Bug 1 (first live run): real sellers deliver files and no inline answer.txt.
test('delivery: collect succeeds but the answer read fails -> recorded once, seller freed, planner sees the error',async()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);f.snap.sellers=f.snap.sellers.slice(0,1);
  await f.runner.tick();[...f.remote.values()][0].results=[{}];
  f.io.answer=async()=>{throw Object.assign(Error("ENOENT: no such file or directory, open '/r/answer.txt'"),{code:'ENOENT'});};
  f.setPlan(()=>({wait:true}));
  for(let t=101;t<=105;t++){f.time(t);await f.runner.tick();}
  const collects=f.calls.filter(([t])=>t==='collect');
  assert.equal(collects.length,1,'collect (and payment) runs exactly once');
  const j=f.journal.state.jobs[0];
  assert.equal(j.done,true);assert.equal(j.outcome,'collected');assert.match(j.answerError,/ENOENT/);
  assert.match(j.answer,/^\[collected, answer unreadable: ENOENT/);
  const r=f.inputs.at(-1).results[0];assert.match(r.answer,/answer unreadable/);assert.match(r.answerError,/ENOENT/);
  assert.ok(reports.some(m=>m.includes('answer unreadable')));
  // The seller is released through the normal ACCEPT projection and gets its next job.
  f.snap.jobs=[{id:j.id,seller:'a',result:{},accepted:true,running:false}];f.setPlan(assignments);f.time(106);await f.runner.tick();
  assert.equal(f.calls.filter(([t])=>t==='post_job').length,2);
});
test('delivery: task keeps the exact challenge prefix, carries the inline-answer note; planner sees only its own task',async()=>{
  const {challengeTaskPrefix}=await import('../../web/app/src/challenge/shared/events.mjs');
  const {DELIVERY_NOTE,plannerTask}=await import('./swarm.mjs');
  const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);await f.runner.tick();
  const task=f.calls.find(([t])=>t==='post_job')[1].task;
  assert.ok(task.startsWith(challengeTaskPrefix('b','day')+'piece'));assert.ok(task.endsWith(DELIVERY_NOTE));
  assert.match(DELIVERY_NOTE,/\nMAXPLAYER-ANSWER-V1 alone on its line|line MAXPLAYER-ANSWER-V1 alone on its line/);
  f.setPlan(()=>({wait:true}));f.time(101);await f.runner.tick();
  assert.equal(f.inputs.at(-1).running[0].task,challengeTaskPrefix('b','day')+'piece');assert.equal(plannerTask('legacy task'),'legacy task');
});
test('delivery: readDeliveredAnswer prefers answer.txt, else hardened deterministic text-file fallback',async()=>{
  const {readDeliveredAnswer}=await import('./swarm.mjs');
  const fs=await import('node:fs'),{execFileSync}=await import('node:child_process');
  const base=mkdtempSync(join(tmpdir(),'swarm-delivery-')),dir=join(base,'result'),outside=join(base,'secret.txt');
  try {
    fs.mkdirSync(dir);fs.writeFileSync(outside,'TOP SECRET');
    fs.writeFileSync(join(dir,'answer.txt'),'inline answer');
    assert.equal(readDeliveredAnswer(dir),'inline answer');
    fs.rmSync(join(dir,'answer.txt'));
    fs.writeFileSync(join(dir,'REPORT.txt'),'report body');fs.writeFileSync(join(dir,'MAXPLAYER_EXECUTION_SENTINEL'),'sentinel');
    fs.mkdirSync(join(dir,'src'));fs.writeFileSync(join(dir,'src','main.rs'),'fn main(){}');
    for(const d of ['.git','target','node_modules']){fs.mkdirSync(join(dir,d));fs.writeFileSync(join(dir,d,'x.txt'),'skip-'+d);}
    fs.writeFileSync(join(dir,'blob.bin'),Buffer.from([1,0,2]));
    fs.symlinkSync(outside,join(dir,'link.txt'));fs.symlinkSync(base,join(dir,'linkdir'));
    execFileSync('mkfifo',[join(dir,'pipe')]);
    fs.writeFileSync(join(dir,'big.txt'),'x'.repeat(100));
    const out=readDeliveredAnswer(dir,{maxFileBytes:20});
    assert.ok(!out.includes('TOP SECRET')&&!out.includes('sentinel')&&!out.includes('skip-'),'never follows links or reads skipped trees');
    assert.ok(out.indexOf('===== REPORT.txt')<out.indexOf('===== src/main.rs'),'files before subdirectories, sorted');
    assert.ok(out.indexOf('===== REPORT.txt')<out.indexOf('===== big.txt'),'code-unit sort order');
    assert.match(out,/fn main\(\)\{\}/);assert.match(out,/\[truncated: first 20 of 100 bytes shown\]/);assert.ok(!out.includes('report body\n[truncated'));
    assert.match(out,/blob\.bin \(binary\)/);assert.match(out,/link\.txt \(not a regular file\)/);assert.match(out,/pipe \(not a regular file\)/);
    assert.equal(readDeliveredAnswer(dir,{maxFileBytes:20}),out,'deterministic');
    assert.match(readDeliveredAnswer(dir,{maxTotalChars:40}),/\[truncated: \d+ more file\(s\) omitted after 40 chars\]/);
    // A symlinked answer.txt is not followed either.
    fs.symlinkSync(outside,join(dir,'answer.txt'));assert.ok(!readDeliveredAnswer(dir).includes('TOP SECRET'));
    const empty=join(base,'empty');fs.mkdirSync(empty);assert.match(readDeliveredAnswer(empty),/no readable text files/);
  } finally {rmSync(base,{recursive:true});}
});
// Advisor r1 F3: one huge directory was listed (readdirSync) and sorted in full before maxEntries
// applied, and nothing bounded the walk's time. Notes: answer.txt is capped, and an empty one falls through.
test('delivery: a directory far larger than maxEntries is streamed, never listed in full; deterministic; deadline bounded',async()=>{
  const {readDeliveredAnswer}=await import('./swarm.mjs');
  const fs=await import('node:fs');
  const base=mkdtempSync(join(tmpdir(),'swarm-bigdir-')),dir=join(base,'result');
  try {
    fs.mkdirSync(join(dir,'huge'),{recursive:true});fs.writeFileSync(join(dir,'REPORT.txt'),'the finding');
    for(let i=0;i<5000;i++)fs.writeFileSync(join(dir,'huge',`f${String(i).padStart(5,'0')}.txt`),`file ${i}`);
    let reads=0;
    const opendir=p=>{const d=fs.opendirSync(p);return {readSync:()=>{reads++;return d.readSync();},closeSync:()=>d.closeSync()};};
    const out=readDeliveredAnswer(dir,{maxEntries:50,opendir});
    assert.ok(reads>0&&reads<=50+2,`streamed: ${reads} directory entries read for a 5001-entry tree`);
    assert.match(out,/the finding/);assert.match(out,/\[truncated: delivery walk stopped after 50 entries\]/);
    assert.ok((out.match(/^===== huge\//gm)??[]).length<50,'only the names actually read are shown');
    assert.equal(readDeliveredAnswer(dir,{maxEntries:50}),readDeliveredAnswer(dir,{maxEntries:50}),'deterministic for what is read');
    // The deadline throws (the runner records it once as answerError, see the collect test above).
    let t=0;assert.throws(()=>readDeliveredAnswer(dir,{deadlineMs:5,clock:()=>t++}),/delivery read exceeded its 5 ms deadline/);
    // answer.txt: capped at maxFileBytes with a marker; an empty one falls through to the files.
    fs.writeFileSync(join(dir,'answer.txt'),'a'.repeat(100));
    assert.equal(readDeliveredAnswer(dir,{maxFileBytes:10}),'a'.repeat(10)+'\n[truncated: first 10 of 100 bytes shown]');
    fs.writeFileSync(join(dir,'answer.txt'),'');
    assert.match(readDeliveredAnswer(dir,{maxEntries:50}),/^Delivered files \(no inline answer\):\n===== REPORT\.txt =====\nthe finding/);
    // A newline in a name cannot forge a ===== frame.
    fs.rmSync(join(dir,'huge'),{recursive:true});fs.writeFileSync(join(dir,'x\n===== fake ====='),'y');
    assert.ok(!/^===== fake =====$/m.test(readDeliveredAnswer(dir)));assert.match(readDeliveredAnswer(dir),/===== "x\\n===== fake =====" =====/);
  } finally {rmSync(base,{recursive:true});}
});
test('delivery: a read past its deadline is recorded once as answerError; collect is not repeated',async()=>{
  const {readDeliveredAnswer}=await import('./swarm.mjs');
  const fs=await import('node:fs');const dir=mkdtempSync(join(tmpdir(),'swarm-slow-'));
  try {
    fs.writeFileSync(join(dir,'a.txt'),'x');
    const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);let t=0;
    f.io.answer=async()=>readDeliveredAnswer(dir,{deadlineMs:1,clock:()=>t+=10});
    await f.runner.tick();[...f.remote.values()][0].results=[{}];f.setPlan(()=>({wait:true}));
    for(let n=101;n<=104;n++){f.time(n);await f.runner.tick();}
    assert.equal(f.calls.filter(([x])=>x==='collect').length,1);
    const j=f.journal.state.jobs[0];assert.equal(j.done,true);assert.match(j.answerError,/deadline/);
  } finally {rmSync(dir,{recursive:true});}
});

// Bug 2 (first live run, 15:49Z): after a long planner call the per-assignment fresh read was
// stale, and stale_snapshot dropped planned assignments instead of holding them.
const staleAfterPlan=(f,{relay=false}={})=>{let stale=false;const snapshot=f.io.snapshot;
  f.io.snapshot=async()=>{const s=await snapshot();if(stale){s.asOf-=10;if(relay)s.relay.failedRelays=['ws://challenge'];}return s;};
  const plan=f.io.plan;f.io.plan=async x=>{const r=await plan(x);if(x.mode==='assign')stale=true;return r;};
  return {heal:()=>{stale=false;}};};
test('deferred: stale_snapshot (alone or with relay_not_ready) is held, re-checked on a genuinely fresh read, posted once',async()=>{
  for(const relay of [false,true]){
    const f=fixture(),reports=[];f.io.report=m=>reports.push(m);const s=staleAfterPlan(f,{relay});
    await f.runner.tick();
    assert.equal(posts(f).length,0,`${relay}`);
    assert.ok(f.journal.state.jobs.length===2&&f.journal.state.jobs.every(j=>j.deferred&&!j.done),`${relay}`);
    assert.ok(reports.some(r=>r.includes('Deferred assignment for a: ')&&r.includes('stale_snapshot')),`${relay}`);
    assert.ok(!reports.some(r=>r.includes('Rejected assignment')),`${relay}`);
    // Still stale (and relay healthy if relay=false): waits, never posts on a stale read.
    f.setPlan(()=>({wait:true}));f.time(103);await f.runner.tick();assert.equal(posts(f).length,0,`${relay}`);
    s.heal();f.time(105);await f.runner.tick();
    assert.deepEqual(posts(f).map(([,a])=>[a.seller_pubkey,a.deadline_unix]),[['a',135],['b',135]],`${relay}`);
    f.time(106);await f.runner.tick();assert.equal(posts(f).length,2,`${relay}`);
  }
});
test('deferred: stale plus a real reason drops immediately; stale past the max wait drops unposted',async()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);staleAfterPlan(f);
  f.setPlan(input=>{f.snap.sellers.forEach(s=>s.minutesRemaining=.1);return assignments(input);});
  await f.runner.tick();assert.equal(f.journal.state.jobs.length,0);assert.ok(reports.some(r=>r.includes('insufficient_minutes')&&r.includes('stale_snapshot')));
  const g=fixture();g.c.maxDefer=60;staleAfterPlan(g);await g.runner.tick();
  g.setPlan(()=>({wait:true}));g.time(161);await g.runner.tick();
  assert.ok(g.journal.state.jobs.every(j=>j.done&&j.released&&/still not ready after 60s/.test(j.outcome)));
  assert.equal(posts(g).length,0);
});
// #1164 review nit (a): the max wait applies only while the snapshot is still not ready.
test('deferred: a ready snapshot after the max wait runs admission first and posts',async()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);f.c.maxDefer=60;f.setPlan(gapOnce(f));
  await f.runner.tick();assert.ok(f.journal.state.jobs.every(j=>j.deferred));
  f.snap.relay.failedRelays=[];f.time(200);await f.runner.tick();
  assert.deepEqual(posts(f).map(([,a])=>[a.seller_pubkey,a.deadline_unix]),[['a',230],['b',230]]);
  assert.ok(!reports.some(r=>r.startsWith('Dropped deferred')));
  // ...and a ready-but-inadmissible one past the cap drops with its real reason, not "not ready".
  const g=fixture();g.c.maxDefer=60;g.setPlan(gapOnce(g));await g.runner.tick();
  g.snap.relay.failedRelays=[];g.snap.sellers.forEach(s=>s.minutesRemaining=.1);g.time(200);await g.runner.tick();
  assert.ok(g.journal.state.jobs.every(j=>j.done&&/insufficient_minutes/.test(j.outcome)&&!/not ready/.test(j.outcome)));
});

// Bug 4 (first live run): every full answer went to every planner call (~639k chars) and the
// planner timeouts were hard-coded.
test('planner input: newest N full, older digested, hard char budget with markers; duration and size logged',async()=>{
  const {boundPlannerInput,TRUNCATION_MARKER}=await import('./swarm.mjs');
  const results=Array.from({length:12},(_,i)=>({seller:'a',task:`task ${i}`,outcome:'collected',answer:`answer-${i} `+'x'.repeat(5000)}));
  const out=boundPlannerInput({mode:'assign',results},{fullResults:3,digestChars:100,maxInputChars:1e6});
  assert.equal(out.results.length,12);
  assert.deepEqual(out.results.slice(-3).map(r=>r.answer),results.slice(-3).map(r=>r.answer),'newest N are full text');
  assert.ok(out.results.slice(0,9).every((r,i)=>r.answer.startsWith(`answer-${i} `)&&r.answer.includes(TRUNCATION_MARKER)&&r.answer.length<250&&r.answerChars===5009));
  assert.deepEqual([out.history.fullText,out.history.digested,out.history.resultsDropped],[3,9,0]);
  // Hard budget: degrade until it fits, never exceed it, always mark the cut.
  // (Below ~1200 chars the never-removed dropped-entry index of 12 results itself no longer fits.)
  for(const budget of [12000,3000,1200]){
    const b=boundPlannerInput({mode:'assign',results},{fullResults:3,digestChars:100,maxInputChars:budget});
    assert.ok(JSON.stringify(b).length<=budget,`${budget}`);
    assert.ok(JSON.stringify(b.results).includes(TRUNCATION_MARKER)||b.history.resultsDropped>0,`${budget}`);
    assert.equal(b.history.results,12);assert.equal(b.results.at(-1)?.task??'task 11','task 11');
  }
  assert.equal(boundPlannerInput({mode:'assign',goal:'g'.repeat(500),results},{maxInputChars:300}),null);
  // The runner applies it to every call and logs duration and size.
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);Object.assign(f.c,{plannerFullResults:1,plannerDigestChars:100,plannerMaxInputChars:100000});
  f.snap.sellers=f.snap.sellers.slice(0,1);f.io.answer=async()=>'y'.repeat(5000);
  for(let n=0;n<3;n++){await f.runner.tick();for(const j of f.remote.values())j.results=[{}];f.time(101+3*n);await f.runner.tick();
    f.snap.jobs=f.journal.state.jobs.filter(j=>j.id).map(j=>({id:j.id,seller:'a',result:{},accepted:true,running:false}));f.time(102+3*n);}
  const last=f.inputs.at(-1);assert.ok(last.results.length>=2);
  assert.equal(last.results.at(-1).answer.length,5000);assert.ok(last.results.slice(0,-1).every(r=>r.answer.includes(TRUNCATION_MARKER)));
  assert.ok(f.inputs.every(i=>JSON.stringify(i).length<=100000&&i.history));
  assert.ok(reports.some(r=>/^Planner assign ok in \d+\.\ds; input \d+ chars, results \d+ \(\d+ full, \d+ digested/.test(r)));
  // An over-budget input fails the call (logged and retried later), never sends unbounded text.
  const g=fixture();g.c.plannerMaxInputChars=100;await assert.rejects(g.runner.tick(),/SWARM_PLANNER_MAX_INPUT_CHARS/);assert.equal(g.inputs.length,0);
});
// Advisor r1 F4: older answers kept only a head, and a hard-budget drop left only a count.
test('planner input: an old long answer keeps its tail (the conclusion); a dropped result stays in the index',async()=>{
  const {boundPlannerInput,TRUNCATION_MARKER}=await import('./swarm.mjs');
  const results=[{seller:'s0',task:'find it',outcome:'collected',ts:111,answer:'preamble '.repeat(1000)+'CONCLUSION: the key is 42'},
    ...Array.from({length:10},(_,i)=>({seller:'s'+(i+1),task:'t'+i,outcome:'expired/undelivered',ts:200+i,answer:'noise '.repeat(800)}))];
  const out=boundPlannerInput({mode:'assess',results},{fullResults:8,digestChars:600,maxInputChars:1e6});
  const old=out.results[0];
  assert.ok(old.answer.startsWith('preamble ')&&old.answer.endsWith('CONCLUSION: the key is 42'),'head and tail');
  assert.ok(old.answer.includes(TRUNCATION_MARKER)&&old.answer.length<700&&old.answerChars===results[0].answer.length);
  // Squeeze until the oldest entry is dropped: its index line survives every later step.
  const tight=boundPlannerInput({mode:'assess',results},{fullResults:8,digestChars:600,maxInputChars:2500});
  assert.ok(tight&&JSON.stringify(tight).length<=2500);assert.ok(tight.history.resultsDropped>=1);
  assert.equal(tight.history.droppedIndex[0],`s0 | collected | ts 111 | ${results[0].answer.length} answer chars`);
  assert.equal(tight.history.droppedIndex.length,tight.history.resultsDropped);
  assert.ok(!tight.results.some(r=>r.seller==='s0'));
});
// Advisor r2 note: a dropped result's outcome (e.g. a relay publish_refused string) kept raw
// newlines inside its index line, and a huge non-hex seller string was not cut at all.
test('planner input: droppedIndex lines are single-line and bounded',async()=>{
  const {boundPlannerInput}=await import('./swarm.mjs');
  const results=[{seller:'x'.repeat(250000),task:'t',outcome:'publish_refused: line one\nline two\r\u0000',ts:1,answer:'a'.repeat(3000)},
    ...Array.from({length:3},(_,i)=>({seller:'s'+i,task:'t',outcome:'collected',ts:2+i,answer:'b'.repeat(3000)}))];
  const out=boundPlannerInput({mode:'assess',results},{fullResults:8,digestChars:600,maxInputChars:4000});
  assert.ok(out&&out.history.resultsDropped>=1,'the huge entry was dropped and the rest fits');
  const line=out.history.droppedIndex[0];
  assert.doesNotMatch(line,/[\u0000-\u001f\u007f]/);assert.match(line,/line one\\nline two\\r\\u0000/);
  assert.ok(line.length<700,`index line ${line.length} chars`);
});
// Review note: trackRecord tasks (and other non-result inputs) were not bounded.
test('planner input: a huge seller task, note or running task is bounded and cannot break later calls',async()=>{
  const {boundPlannerInput}=await import('./swarm.mjs');
  const huge='T'.repeat(300000);
  const tasks=Array.from({length:30},(_,i)=>({task:i===29?huge:`task ${i}`,outcome:'collected',delivered:true}));
  const input={mode:'assign',goal:'g',sellers:[{pubkey:'a',name:huge,note:huge,capabilities:Array(100).fill(huge),harness:{family:[huge]},trackRecord:{tasksDone:30,delivered:30,expired:0,tasks}}],
    running:[{seller:'a',task:huge}],results:[]};
  const out=boundPlannerInput(input,{maxInputChars:200000});
  assert.ok(out&&JSON.stringify(out).length<50000,`${out&&JSON.stringify(out).length}`);
  const tr=out.sellers[0].trackRecord;
  assert.deepEqual([tr.tasksDone,tr.tasks.length,tr.tasksOmitted,tr.tasks[0].task],[30,10,20,'task 20'],'newest 10 kept; counts exact');
  assert.ok(tr.tasks.at(-1).task.length<700&&out.running[0].task.length<2200&&out.sellers[0].note.length<2200&&out.sellers[0].capabilities.length===32);
  assert.ok(out.history.fieldsCut>=5);
  // End to end: a 300k-char past task no longer fails every later planner call.
  const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);f.setPlan(input=>({assignments:[{seller:'a',workstream:'piece',task:huge,durationSeconds:30}]}));
  await f.runner.tick();for(const j of f.remote.values())j.results=[{}];f.time(101);f.setPlan(()=>({wait:true}));await f.runner.tick();
  f.snap.jobs=f.journal.state.jobs.map(j=>({id:j.id,seller:'a',result:{},accepted:true,running:false}));f.time(102);
  const before=f.inputs.length;await f.runner.tick();
  assert.ok(f.inputs.length>before&&f.inputs.slice(before).every(i=>JSON.stringify(i).length<=200000));
  assert.ok(f.inputs.at(-1).sellers[0].trackRecord.tasks[0].task.length<700);
});
test('planner timeouts: env-configurable, defaults kept, bundled LLM timeout stays below the runner',async()=>{
  const {plannerTimeouts,PLANNER_LLM_MARGIN_SECONDS}=await import('./swarm.mjs');
  const d=plannerTimeouts({});assert.deepEqual([d.runnerMs('analyze'),d.runnerMs('assign'),d.runnerMs('assess')],[360000,180000,180000]);
  assert.deepEqual([d.llmMs('analyze'),d.llmMs('assign')],[350000,170000]);
  const e=plannerTimeouts({SWARM_ANALYZE_TIMEOUT_SECONDS:'900',SWARM_PLANNER_TIMEOUT_SECONDS:'600'});
  assert.deepEqual([e.runnerMs('analyze'),e.llmMs('analyze'),e.runnerMs('assign'),e.llmMs('assign')],[900000,890000,600000,590000]);
  for(const bad of ['0','10','x','1.5'])assert.throws(()=>plannerTimeouts({SWARM_PLANNER_TIMEOUT_SECONDS:bad}),/SWARM_PLANNER_TIMEOUT_SECONDS/);
  const base={MAXPLAYER_HOME:'/tmp/b',RELAY_URL:'ws://x'};
  assert.equal(config([],{...base,SWARM_PLANNER_TIMEOUT_SECONDS:'600'}).plannerTimeouts.runnerMs('assign'),600000);
  assert.throws(()=>config([],{...base,SWARM_PLANNER_MAX_INPUT_CHARS:'50'}),/plannerMaxInputChars/);
  assert.ok(PLANNER_LLM_MARGIN_SECONDS>0);
  // planner-llm.mjs really uses the env-derived timeout: a hanging fake LLM is killed at 1 s.
  const {writeFileSync}=await import('node:fs'),{command}=await import('./swarm.mjs');
  const dir=mkdtempSync(join(tmpdir(),'swarm-llm-timeout-')),bin=join(dir,'slow');
  try {
    writeFileSync(bin,'#!/bin/sh\nsleep 30\n',{mode:0o700});
    const started=Date.now();
    await assert.rejects(command(process.execPath,['scripts/challenges/planner-llm.mjs'],{input:JSON.stringify({mode:'assign',results:[]}),env:{...process.env,SWARM_LLM_BINARY:bin,SWARM_PLANNER_TIMEOUT_SECONDS:'11'},timeout:20000}),/Command timed out/);
    assert.ok(Date.now()-started<5000,'llm timeout = runner timeout - margin = 1 s');
  } finally {rmSync(dir,{recursive:true});}
});

// Bug 5 (first live run): a v0.6.1 buyer's get_job lacks the release-proof fields, so expired
// paid jobs stayed reserved forever. Detect it and refuse priced work; free work continues.
test('buyer capability: get_job without release fields refuses priced work, keeps free work, warns once',async()=>{
  const {RELEASE_FIELDS,PROBE_JOB_ID,missingReleaseFields}=await import('./swarm.mjs');
  assert.deepEqual(RELEASE_FIELDS,['award_read_confirmed','reservation_state','auto_award_state','claim_prices']);
  const oldView={job_id:'x',read_confirmed:true,claims:[],results:[],awarded:null};
  assert.deepEqual(missingReleaseFields(oldView),RELEASE_FIELDS);
  assert.deepEqual(missingReleaseFields({...oldView,award_read_confirmed:true,reservation_state:null,auto_award_state:null,claim_prices:[]}),[]);
  const f=fixture(),reports=[],probes=[];f.io.report=m=>reports.push(m);
  f.io.probeBuyer=async()=>{probes.push(PROBE_JOB_ID);return oldView;};
  f.setPlan(input=>({assignments:assignments(input).assignments.map((a,i)=>({...a,amountSats:i?5:0}))}));
  await f.runner.tick();
  assert.deepEqual(posts(f).map(([,a])=>a.amount_sats),[0],'free posts, priced refused');
  assert.ok(reports.some(r=>r.startsWith('WARNING: buyer get_job lacks award_read_confirmed')));
  assert.ok(reports.some(r=>/Rejected assignment for b: priced work refused: buyer get_job lacks/.test(r)));
  assert.equal(reservedBudget(f.snap,f.journal.state.jobs,100).committedSats,0,'nothing reserved for refused work');
  f.time(101);await f.runner.tick();
  assert.equal(probes.length,1,'definite answer cached');assert.match(f.inputs.at(-1).paidWorkRefused,/assign only amountSats 0/);
  // A current buyer passes; a failed probe refuses priced work and is retried later.
  const g=fixture();let fail=true;g.io.probeBuyer=async()=>{if(fail){fail=false;throw Error('relay down');}return {...oldView,award_read_confirmed:true,reservation_state:'none',auto_award_state:null,claim_prices:[]};};
  g.setPlan(input=>priced(input,5));g.snap.sellers=g.snap.sellers.slice(0,1);
  await g.runner.tick();assert.equal(posts(g).length,0);
  g.time(101);await g.runner.tick();assert.deepEqual(posts(g).map(([,a])=>a.amount_sats),[5]);
  assert.equal(g.inputs.at(-1).paidWorkRefused,undefined);
});

// Bug 6 (first live run): 120-200 s tasks expired while real agents were still starting.
test('min duration: default floor 300 s; sellers below the floor are not offered; minutes accounting unchanged',async()=>{
  const {DEFAULT_MIN_DURATION}=await import('./swarm.mjs');
  const base={MAXPLAYER_HOME:'/tmp/b',RELAY_URL:'ws://x'};
  assert.equal(DEFAULT_MIN_DURATION,300);assert.equal(config([],base).minDuration,300);assert.equal(config([],{...base,SWARM_MIN_DURATION:'600'}).minDuration,600);
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);delete f.c.minDuration;f.c.maxDuration=1800;
  f.snap.sellers[0].minutesRemaining=4.99;f.snap.sellers[1].minutesRemaining=5;
  const before=structuredClone(f.snap.sellers);
  f.setPlan(input=>({assignments:[{seller:'a',workstream:'piece',task:'t',durationSeconds:300},{seller:'b',workstream:'piece',task:'short',durationSeconds:200},{seller:'b',workstream:'piece',task:'t',durationSeconds:300}]}));
  await f.runner.tick();
  const assign=f.inputs.find(i=>i.mode==='assign');
  assert.equal(assign.minDurationSeconds,300);assert.deepEqual(assign.sellers.map(s=>s.pubkey),['b'],'seller below the floor is not offered');
  assert.deepEqual(posts(f).map(([,a])=>[a.seller_pubkey,a.deadline_unix]),[['b',400]]);
  assert.ok(reports.some(r=>r.includes('Rejected assignment for a: seller busy or ineligible')));
  assert.ok(reports.some(r=>r.includes('Rejected assignment for b: duration outside configured bounds')));
  assert.deepEqual(f.snap.sellers.map(s=>s.minutesRemaining),before.map(s=>s.minutesRemaining),'runner never edits minutes');
  const prompt=(await import('node:fs')).readFileSync('scripts/challenges/planner-llm.mjs','utf8');assert.match(prompt,/starting its\s+sandbox and agent/);
});

// Bug 7 (first live run, macOS/launchd): killing the parent left the flock'd --lock-held runner
// alive with the lock (and a stuck reader), so every restart exited 1 for about an hour.
test('parent death: watchParent stops the runner, then kills in-flight commands and exits after the grace',async()=>{
  const {watchParent}=await import('./swarm.mjs');const {PassThrough}=await import('node:stream');
  const stream=new PassThrough(),events=[];let resolveExit;const exited=new Promise(r=>{resolveExit=r;});
  const gone=watchParent(stream,()=>events.push('stop'),{graceMs:20,kill:()=>events.push('kill'),exit:code=>{events.push(`exit ${code}`);resolveExit();}});
  stream.write('ignored');assert.equal(gone(),false);stream.end();
  await exited;assert.deepEqual(events,['stop','kill','exit 1']);assert.equal(gone(),true);
});
// Advisor r1 F1: the grace timer was unref'd, so with nothing else pending the event loop ended
// before kill + exit ran (and node --test on Node 22 cancelled this file's remaining tests).
test('parent death: the grace timer alone keeps the process alive until it kills and exits 1',async()=>{
  const {spawnSync}=await import('node:child_process');
  const script=`import {watchParent} from ${JSON.stringify(new URL('./swarm.mjs',import.meta.url).href)};import {PassThrough} from 'node:stream';
    const s=new PassThrough();watchParent(s,()=>console.log('stop'),{graceMs:50,kill:()=>console.log('kill')});s.end();`;
  const r=spawnSync(process.execPath,['--input-type=module','-e',script],{encoding:'utf8',timeout:20000});
  assert.deepEqual([r.stdout.trim().split('\n'),r.status],[['stop','kill'],1],r.stderr);
});
test('parent death: SIGKILL of the runner parent releases the home lock and stops the locked child',{skip:!require_flock()},async()=>{
  const fs=await import('node:fs'),{spawn,spawnSync}=await import('node:child_process');
  const dir=mkdtempSync(join(tmpdir(),'swarm-parent-')),home=join(dir,'home'),bin=join(dir,'fake-maxplayer'),pids=join(dir,'pids');
  try {
    // The locked runner's first command (whoami) hangs: it records its own and its parent's pid.
    fs.writeFileSync(bin,`#!/bin/sh\necho "$$ $PPID" > ${JSON.stringify(pids)}\nexec sleep 60\n`,{mode:0o700});
    const parent=spawn(process.execPath,['scripts/challenges/swarm.mjs','--home',home,'--binary',bin,'--relay-url','ws://127.0.0.1:1'],{stdio:['ignore','ignore','pipe']});
    let stderr='';parent.stderr.on('data',b=>{stderr+=b;});
    const until=async(f,ms)=>{const end=Date.now()+ms;while(Date.now()<end){if(f())return true;await new Promise(r=>setTimeout(r,50));}return false;};
    assert.ok(await until(()=>fs.existsSync(pids)&&fs.readFileSync(pids,'utf8').trim(),10000),'locked runner started its first command');
    const [sleeper,runner]=fs.readFileSync(pids,'utf8').trim().split(' ').map(Number);
    const lock=join(home,'swarm/runner.lock'),free=()=>spawnSync('flock',['-n',lock,'true']).status===0;
    assert.equal(free(),false,'lock held while the runner lives');
    parent.kill('SIGKILL');
    const alive=pid=>{try{process.kill(pid,0);return true;}catch{return false;}};
    assert.ok(await until(()=>!alive(runner)&&!alive(sleeper)&&free(),15000),`runner ${alive(runner)} command ${alive(sleeper)} lock free ${free()}; ${stderr}`);
  } finally {rmSync(dir,{recursive:true,force:true});}
});
// Starts a real parent -> flock -> locked runner against a relay that completes the WebSocket
// handshake and then stays silent (never ready). Stopping while a socket is still CONNECTING trips
// relay.mjs's error->close handler, which overflows the stack on Node 22 (shared reader, out of
// scope), so the fixture opens the socket. `mcp` is the fake `maxplayer mcp` shell body.
async function lockedRunner(t,{poll,mcp='',detached=false}) {
  const fs=await import('node:fs'),{spawn,spawnSync}=await import('node:child_process');
  const dir=mkdtempSync(join(tmpdir(),'swarm-stop-')),home=join(dir,'home'),bin=join(dir,'fake-maxplayer'),ready=join(dir,'ready'),mcpMark=join(dir,'mcp');
  t.after(()=>rmSync(dir,{recursive:true,force:true}));
  fs.writeFileSync(bin,`#!/bin/sh\nif [ "$1" = whoami ]; then touch ${JSON.stringify(ready)}; echo "pubkey: ${'a'.repeat(64)}"; exit 0; fi\nif [ "$1" = mcp ]; then touch ${JSON.stringify(mcpMark)}; ${mcp}\nfi\n`,{mode:0o700});
  const net=await import('node:net'),{createHash}=await import('node:crypto'),sockets=[];
  const hold=net.createServer(c=>{sockets.push(c);let buf='';c.on('error',()=>{});c.on('data',b=>{if(buf===null)return;buf+=b;const key=buf.match(/Sec-WebSocket-Key: *(\S+)/i)?.[1];
    if(key&&buf.includes('\r\n\r\n')){buf=null;c.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${createHash('sha1').update(key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64')}\r\n\r\n`);}});});
  await new Promise(r=>hold.listen(0,'127.0.0.1',r));
  const relay=`ws://127.0.0.1:${hold.address().port}`;
  t.after(()=>{sockets.forEach(c=>c.destroy());hold.close();});
  // detached: the parent leads its own process group, as e2e.py's start_new_session does.
  const parent=spawn(process.execPath,['scripts/challenges/swarm.mjs','--home',home,'--binary',bin,'--relay-url',relay,'--poll-seconds',String(poll)],{detached,stdio:['ignore','ignore','pipe'],env:{...process.env,SWARM_CHALLENGE_RELAYS:relay}});
  t.after(()=>{try{process.kill(detached?-parent.pid:parent.pid,'SIGKILL');}catch{}});
  let stderr='';parent.stderr.on('data',b=>{stderr+=b;});
  const lockFree=()=>spawnSync('flock',['-n',join(home,'swarm/runner.lock'),'true']).status===0;
  // The lock is sampled in the exit handler itself: the parent must never exit while it is held.
  const exited=new Promise(r=>parent.on('exit',code=>r({code,lockFreeAtExit:lockFree(),at:Date.now()})));
  const until=async(f,ms)=>{const end=Date.now()+ms;while(Date.now()<end){if(f())return true;await new Promise(r=>setTimeout(r,50));}return false;};
  assert.ok(await until(()=>fs.existsSync(ready)&&sockets.length>0,10000),stderr);
  const result=async ms=>Promise.race([exited,new Promise(r=>setTimeout(()=>r({code:'timeout'}),ms))]);
  return {parent,stderr:()=>stderr,lockFree,until,result,mcpStarted:()=>fs.existsSync(mcpMark)};
}
// Found by the E2E after F1: with a ref'd grace timer a CLEANLY stopped runner still held the home
// lock for the whole grace, so the next start (flock -n) exited 1. A clean stop must exit 0 at once.
// Advisor r2 F1: poll 30 also pins that the poll sleep ends as soon as the runner is asked to stop.
test('parent death: a clean stop exits 0 promptly and frees the lock (grace timer cancelled, poll sleep cut short)',{skip:!require_flock()},async t=>{
  const r=await lockedRunner(t,{poll:30});
  await new Promise(res=>setTimeout(res,1500));
  assert.equal(r.lockFree(),false,'lock held while the runner lives');
  const stopped=Date.now();r.parent.kill('SIGTERM');
  const out=await r.result(15000);
  assert.equal(out.code,0,`clean stop exit code; ${r.stderr()}`);
  assert.ok(out.at-stopped<4000,`stopped in ${out.at-stopped} ms (grace is 5000, poll 30000)`);
  assert.equal(out.lockFreeAtExit,true,'lock free when the parent exits');
});
// Advisor r2 F1: e2e.py (and a process-group supervisor) stops the runner with killpg. flock was in
// the parent's group, died on that SIGTERM, and the parent exited 1 at once while the locked runner
// still held the lock (~900 ms), so an immediate restart hit flock -n and exited 1.
test('stop: a process-group SIGTERM keeps the parent alive until the lock is free, then exits 0',{skip:!require_flock()},async t=>{
  const r=await lockedRunner(t,{poll:30,detached:true});
  await new Promise(res=>setTimeout(res,1500));
  assert.equal(r.lockFree(),false,'lock held while the runner lives');
  const stopped=Date.now();process.kill(-r.parent.pid,'SIGTERM');
  const out=await r.result(15000);
  assert.equal(out.code,0,`exit code after killpg; ${r.stderr()}`);
  assert.equal(out.lockFreeAtExit,true,'parent exited while the runner still held the lock');
  assert.ok(out.at-stopped<4000,`stopped in ${out.at-stopped} ms`);
});
// Advisor r2 F1: a parent-only SIGTERM during an MCP call longer than the 5 s parent-gone grace
// hit the grace kill (exit 1). A stop request lets the in-flight call finish, then exits 0.
test('stop: a SIGTERM during an MCP call longer than the parent-gone grace lets it finish and exits 0',{skip:!require_flock()},async t=>{
  const {PARENT_GONE_GRACE_MS}=await import('./swarm.mjs');
  const r=await lockedRunner(t,{poll:5,mcp:`sleep ${(PARENT_GONE_GRACE_MS+1500)/1000}; exit 0`});
  assert.ok(await r.until(r.mcpStarted,10000),'MCP call started');
  const stopped=Date.now();r.parent.kill('SIGTERM');
  const out=await r.result(20000);
  assert.equal(out.code,0,`exit code; ${r.stderr()}`);
  assert.equal(out.lockFreeAtExit,true,'lock free when the parent exits');
  assert.ok(out.at-stopped>=PARENT_GONE_GRACE_MS,`the in-flight call ran to completion (${out.at-stopped} ms)`);
  assert.doesNotMatch(r.stderr(),/killing in-flight/);
});
// A stop request is bounded too: a genuinely hung tick is killed after the stop grace, and the
// parent's death (or a second signal, which closes the pipe) shortens that to the parent-gone grace.
test('stop: watchParent bounds a stop request by its own grace, and pipe EOF shortens it',async()=>{
  const {watchParent}=await import('./swarm.mjs');const {PassThrough}=await import('node:stream');
  const run=async({stopGraceMs,graceMs,end})=>{
    const stream=new PassThrough(),events=[];let resolveExit;const exited=new Promise(r=>{resolveExit=r;});
    const gone=watchParent(stream,()=>events.push('stop'),{graceMs,stopGraceMs,kill:()=>events.push('kill'),exit:code=>{events.push(`exit ${code}`);resolveExit();}});
    const started=Date.now();stream.write('stop\n');await new Promise(r=>setTimeout(r,30));
    assert.deepEqual(events,['stop'],'stop request: onStop once, no kill yet');assert.equal(gone(),false);
    if(end)stream.end();await exited;return {events,ms:Date.now()-started,gone:gone()};
  };
  const hung=await run({stopGraceMs:150,graceMs:60000});
  assert.deepEqual(hung.events,['stop','kill','exit 1']);assert.ok(hung.ms>=150);
  const dead=await run({stopGraceMs:60000,graceMs:20,end:true});
  assert.deepEqual(dead.events,['stop','kill','exit 1']);assert.ok(dead.ms<5000);assert.equal(dead.gone,true);
});
function require_flock(){try{return (globalThis.__flock??=(()=>{const r=process.getBuiltinModule('node:child_process').spawnSync('flock',['--version']);return r.status===0;})());}catch{return false;}}

// Bug 5, upgraded (Rocky's revised report): on a v0.6.1 buyer an expired, never-delivered PAID
// job never reaches done, so pending() blocked that seller for the rest of the day (argon ~7.5 h).
const expiredPaid=()=>{
  const f=fixture(),reports=[];f.io.report=m=>reports.push(m);f.snap.sellers=f.snap.sellers.slice(0,1);
  let n=0;f.setPlan(input=>input.mode==='assess'?{wait:true}:n++?assignments(input):priced(input,5));
  return {f,reports,old:()=>[...f.remote.values()][0]};
};
test('expired paid job without release fields: seller freed after deadline + grace, sats stay reserved, never collected or paid',async()=>{
  const {EXPIRED_SLOT_GRACE_SECONDS}=await import('./swarm.mjs');
  const {f,reports,old}=expiredPaid();
  await f.runner.tick();assert.deepEqual(posts(f).map(([,a])=>[a.amount_sats,a.deadline_unix]),[[5,130]]);
  // A v0.6.1-shaped view: confirmed read, no results, none of the release-proof fields.
  Object.assign(old(),{read_confirmed:true,results:[],claims:[],accepted:null});
  const deadline=130,grace=EXPIRED_SLOT_GRACE_SECONDS;
  // Advisor r1 F2: deadline + grace itself is not past it (strict >, like the seller's kill check).
  for(const t of [131,deadline+grace-1,deadline+grace]){f.time(t);await f.runner.tick();assert.equal(posts(f).length,1,`still busy at ${t}`);}
  const j=f.journal.state.jobs[0];assert.equal(j.done,false);assert.equal(j.slotFreed,undefined);
  f.time(deadline+grace+1);await f.runner.tick();
  assert.equal(j.slotFreed,deadline+grace+1);assert.equal(j.done,false,'job stays open and polled');assert.equal(j.released,false);
  assert.deepEqual(posts(f).map(([,a])=>[a.seller_pubkey,a.amount_sats]),[['a',5],['a',0]],'seller gets a new assignment');
  assert.ok(reports.some(r=>/expired undelivered 600s past its deadline; seller a freed for new work, 5 sats stay reserved/.test(r)));
  const jobs=f.journal.state.jobs;assert.equal(reservedBudget(f.snap,jobs,f.io.now()).committedSats,5,'old sats still reserved');
  assert.ok(!f.inputs.at(-1).running?.some(r=>r.deadline===deadline),'freed job not shown as running');
  for(let t=deadline+grace+2;t<deadline+grace+5;t++){f.time(t);await f.runner.tick();}
  assert.equal(f.calls.filter(([t])=>t==='collect').length,0,'nothing collected or paid');
  assert.equal(posts(f).length,2,'one new job at a time: the new job keeps the seller busy');
  assert.equal(reservedBudget(f.snap,jobs,f.io.now()).committedSats,5);
});
test('expired paid job: late delivery after the slot was freed is collected exactly once and settles its reservation',async()=>{
  const {f,old}=expiredPaid();const mcp=f.io.mcp;
  f.io.mcp=async(t,a)=>t==='collect'&&a.job_id===old().id?(f.calls.push([t,a]),{pay:{state:'Closed { receipt }',amount_sats:5}}):mcp(t,a);
  await f.runner.tick();Object.assign(old(),{read_confirmed:true,results:[],claims:[],accepted:null});
  f.time(731);await f.runner.tick();assert.equal(posts(f).length,2);
  // Core keeps a past-deadline delivery payable (claim `delivered`, 7-day pay window).
  old().results=[{}];
  for(const t of [732,733,734]){f.time(t);await f.runner.tick();}
  assert.deepEqual(f.calls.filter(([t])=>t==='collect').map(([,a])=>a.job_id),[old().id],'collected (and paid) exactly once');
  const j=f.journal.state.jobs[0];assert.equal(j.outcome,'collected');assert.equal(j.done,true);
  assert.deepEqual(jobBudget(j),{reservedSats:0,paidSats:5});
  assert.equal(posts(f).length,2,'late collection does not hand the seller a second live job');
});
test('expired paid job: slot stays held on an unconfirmed read or a pending awarded delivery; grace is validated',async()=>{
  // No read_confirmed key at all (a buyer older than #603) also holds the slot: fail closed.
  for(const view of [{read_confirmed:false,results:[]},{read_confirmed:true,results:[],awarded_delivery_pending:true},{read_confirmed:true},{results:[]}]){
    const {f,old}=expiredPaid();await f.runner.tick();Object.assign(old(),view);if(!('results' in view))delete old().results;
    f.time(2000);f.snap.challenge.ends=5000;await f.runner.tick();
    assert.equal(f.journal.state.jobs[0].slotFreed,undefined,JSON.stringify(view));assert.equal(posts(f).length,1,JSON.stringify(view));
  }
  // Pre-existing rule, pinned for the README: an expired FREE job is done (seller freed) on the first
  // poll after its deadline, whatever get_job reports (read_confirmed absent here); it never had sats.
  {const f=fixture();f.snap.sellers=f.snap.sellers.slice(0,1);await f.runner.tick();
    const j=f.journal.state.jobs[0];assert.equal(j.args.payment,'none');f.time(j.args.deadline_unix+1);f.setPlan(()=>({wait:true}));await f.runner.tick();
    assert.deepEqual([j.done,j.released,j.outcome,j.slotFreed],[true,true,'expired/undelivered',undefined]);}
  const base={MAXPLAYER_HOME:'/tmp/b',RELAY_URL:'ws://x'};
  assert.equal(config([],base).expiredSlotGrace,600);assert.equal(config([],{...base,SWARM_EXPIRED_SLOT_GRACE_SECONDS:'900'}).expiredSlotGrace,900);
  // Advisor r1 F2: 420 s is the seller's container kill instant, so the floor is 420 + a 60 s teardown margin.
  const {MIN_EXPIRED_SLOT_GRACE_SECONDS,SELLER_EXECUTION_TAIL_SECONDS,SLOT_TEARDOWN_MARGIN_SECONDS}=await import('./swarm.mjs');
  assert.deepEqual([SELLER_EXECUTION_TAIL_SECONDS,SLOT_TEARDOWN_MARGIN_SECONDS,MIN_EXPIRED_SLOT_GRACE_SECONDS],[420,60,480]);
  for(const bad of ['419','420','479'])assert.throws(()=>config([],{...base,SWARM_EXPIRED_SLOT_GRACE_SECONDS:bad}),/SWARM_EXPIRED_SLOT_GRACE_SECONDS: must be an integer >= 480/,bad);
  assert.equal(config([],{...base,SWARM_EXPIRED_SLOT_GRACE_SECONDS:'480'}).expiredSlotGrace,480);
});
