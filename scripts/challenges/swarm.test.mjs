import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, appendFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Journal, Swarm, validatePlan, requestId, preparationId, config } from './swarm.mjs';
function fixture() {
  let now=100;
  const snap={asOf:100,relay:{connected:true,authenticated:true,historyComplete:true},challenge:{buyer:'b',id:'day',starts:100,ends:1000,status:'in_progress',goal:'solve',title:'test'},budget:{totalSats:10,unknownAmounts:0,committedSats:0,paidSats:0},jobs:[],sellers:['a','b'].map(pubkey=>({pubkey,eligible:true,decision:'accepted',state:'accepted',minutesRemaining:1,currentJobStart:null}))};
  const calls=[],publishes=[],inputs=[],remote=new Map();let planner=()=>({task:'piece'});
  const journal={state:{jobs:[],days:{}},save(){this.saved=structuredClone(this.state);}};
  const io={now:()=>now,snapshot:async()=>({...structuredClone(snap),asOf:now}),plan:async x=>{inputs.push(x);return planner(x);},answer:async()=> 'verified answer',publish:async(...a)=>publishes.push(a),mcp:async(tool,args)=>{
    calls.push([tool,structuredClone(args)]);
    if(tool==='post_job') {if(!remote.has(args.request_id))remote.set(args.request_id,{id:`job-${remote.size}`,results:[]});return {job_id:remote.get(args.request_id).id};}
    if(tool==='get_job') {
      if(args.job_id.startsWith('preparation:')) {const entry=[...remote.entries()].find(([key])=>preparationId(key)===args.job_id);if(!entry)throw Error('unknown preparation handle');return {status:'posted',job_id:entry[1].id};}
      return [...remote.values()].find(j=>j.id===args.job_id)??{results:[]};
    }
    if(tool==='collect')return {pay:{state:'none',amount_sats:0}};
  }};
  const c={duration:30,amount:0,updateEvery:180,awardAfter:5,awardFallback:false};
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
  const f=fixture();f.c.amount=6;await f.runner.tick();assert.equal(f.remote.size,1);
  const args=f.calls.find(x=>x[0]==='post_job')[1];assert.equal(args.payment,'sat');assert.equal(args.max_sats,6);
  await f.runner.tick();assert.match(f.publishes.at(-1)[3],/budget exhausted/);
});
test('fallback award is disabled by default and bounded when explicitly enabled',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.claims=[{live:true,seller_pubkey:j.id==='job-0'?'a':'b',claim_id:'claim'}];
  f.time(110);await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='award_claim').length,0);
  f.c.awardFallback=true;await f.runner.tick();assert.equal(f.calls.filter(x=>x[0]==='award_claim').length,2);
});
test('planner latency rechecks deadline and changed acceptance immediately before post',async()=>{
  for(const change of [f=>f.time(999),f=>{f.snap.challenge.ends=110;},f=>f.snap.sellers.forEach(s=>s.decision='denied'),f=>f.snap.challenge.status='completed']){
    const f=fixture();f.setPlan(()=>{change(f);return {task:'too late'};});await f.runner.tick();assert.equal(f.remote.size,0);
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
  const c=config([],{MAXPLAYER_HOME:'/tmp/buyer',COMMUNITY_APP_URL:'http://localhost'});assert.equal(c.awardFallback,false);assert.equal(c.amount,0);
  assert.throws(()=>config(['--amount-sats','-1'],{MAXPLAYER_HOME:'/tmp/b',COMMUNITY_APP_URL:'http://localhost'}));
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
  f.snap.jobs=[...f.remote.values()].map((j,i)=>({id:j.id,result:{},seller:i?'b':'a',running:false,amountSats:0}));
  await f.runner.tick();assert.equal(f.remote.size,4);assert.ok(f.inputs.at(-1).results.every(r=>r.answer==='verified answer'));
});
test('progress is throttled independently from immediate completion publication',async()=>{
  const f=fixture();f.setPlan(()=>({wait:true}));f.time(200);await f.runner.tick();assert.equal(f.publishes.length,1);
  f.time(210);await f.runner.tick();assert.equal(f.publishes.length,1);f.time(381);await f.runner.tick();assert.equal(f.publishes.length,2);
  f.setPlan(()=>({solved:true,result:'verified'}));f.time(382);await f.runner.tick();assert.equal(f.publishes.length,3);assert.equal(f.publishes[2][2],'completed');
});
test('last paid result can solve even when budget is fully reserved',async()=>{
  const f=fixture();f.c.amount=10;await f.runner.tick();assert.equal(f.remote.size,1);
  [...f.remote.values()][0].results=[{}];f.setPlan(()=>({solved:true,result:'last answer'}));await f.runner.tick();
  assert.equal(f.publishes.at(-1)[4],'last answer');assert.equal(f.remote.size,1);
});
test('one failed collection does not block settlement for another seller',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.results=[{}];
  const orig=f.io.mcp;f.io.mcp=async(t,a)=>{if(t==='collect'&&a.job_id==='job-0')throw Error('temporary failure');return orig(t,a);};
  await f.runner.tick();assert.equal(f.journal.state.jobs.filter(j=>j.done).length,1);assert.equal(f.journal.state.jobs.find(j=>j.done).id,'job-1');
});
test('settlement continues when community HTTP is unavailable',async()=>{
  const f=fixture();await f.runner.tick();for(const j of f.remote.values())j.results=[{}];
  f.io.snapshot=async()=>{throw Error('HTTP unavailable');};await assert.rejects(f.runner.tick(),/HTTP unavailable/);
  assert.equal(f.journal.state.jobs.filter(j=>j.done).length,2);
});
test('new public reservations during planning are included in the final budget gate',async()=>{
  const f=fixture();f.c.amount=6;f.setPlan(()=>{f.snap.jobs.push({id:'foreign',amountSats:5});return {task:'piece'};});
  await f.runner.tick();assert.equal(f.remote.size,0);
});
test('stub planner solves only after N successful results',async()=>{
  const {command}=await import('./swarm.mjs');
  const input={results:[{answer:'ok'},{answer:'[expired/undelivered]'}],seller:{pubkey:'a'}};
  const invoke=async data=>JSON.parse(await command(process.execPath,['scripts/challenges/planner-stub.mjs'],{input:JSON.stringify(data),env:{...process.env,SWARM_STUB_SOLVE_AFTER:'2'}}));
  assert.ok((await invoke(input)).task);input.results.push({answer:'second'});assert.equal((await invoke(input)).solved,true);
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
