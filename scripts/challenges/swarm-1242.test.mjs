// #1242: generic planner robustness (F-1 reduced-input retry and token-ish cap, F-2 adopting an
// invented workstream id, F-3 prompt lines, F-4 unusable-delivery annotation, F-7 window-ended stop).
// Imports go through `* as m` so a base without these exports fails at the named assertion, not at load.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import * as m from './swarm.mjs';
const {Swarm,command}=m;
const KEY=JSON.stringify(['b','day']);
const analysis={approach:'split',workstreams:[{id:'piece',description:'piece',needs:{skills:[]},estMinutes:1,dependsOn:[]}],verification:'check'};
const seller=pubkey=>({pubkey,eligible:true,decision:'accepted',state:'accepted',minutesRemaining:60,currentJobStart:null});
function fixture({sellers=['a','b'],planner=()=>({wait:true,idleReason:'test'})}={}) {
  let now=100;
  const snap={asOf:100,relay:{connected:true,authenticated:true,historyComplete:true},challenge:{buyer:'b',id:'day',starts:100,ends:5000,status:'in_progress',goal:'solve',title:'test'},budget:{totalSats:1000,unknownAmounts:0,committedSats:0,paidSats:0},jobs:[],sellers:sellers.map(seller)};
  const calls=[],publishes=[],inputs=[],reports=[],remote=new Map();
  const journal={state:{jobs:[],days:{}},save(){this.saved=structuredClone(this.state);}};
  const f={snap,calls,publishes,inputs,reports,remote,journal,planner,answer:'a complete and readable answer, long enough to be used',
    time:n=>{now=n;},posts:()=>calls.filter(([t])=>t==='post_job').map(([,a])=>a),assigns:()=>inputs.filter(i=>i.mode==='assign')};
  f.io={now:()=>now,report:r=>reports.push(r),snapshot:async()=>({...structuredClone(snap),asOf:now}),
    plan:async x=>{inputs.push(structuredClone(x));return x.mode==='analyze'?structuredClone(analysis):x.mode==='assess'?{wait:true}:f.planner(x,inputs.filter(i=>i.mode==='assign').length);},
    answer:async()=>f.answer,publish:async(...a)=>publishes.push(a),latestChallenge:async()=>({...structuredClone(snap.challenge),budgetSats:snap.budget.totalSats}),
    mcp:async(tool,args)=>{
      calls.push([tool,structuredClone(args)]);
      if(tool==='post_job'){if(!remote.has(args.request_id))remote.set(args.request_id,{id:`job-${remote.size}`,results:[]});return {job_id:remote.get(args.request_id).id};}
      if(tool==='get_job')return [...remote.values()].find(j=>j.id===args.job_id)??{results:[]};
      if(tool==='collect')return {pay:{state:'Closed { paid }',amount_sats:args.job_id==='job-0'?5:0}};
    }};
  f.c={minDuration:300,maxDuration:900,duration:300,amount:0,updateEvery:180,awardAfter:5,awardFallback:false,plannerIdleSeconds:0};
  f.runner=new Swarm(f.c,f.io,journal);
  f.day=()=>Object.values(journal.state.days)[0];
  return f;
}
// n collected results from a seller outside the roster (settled: its ACCEPT is projected).
function withResults(f,n,chars=3000) {
  for(let i=0;i<n;i++) {
    const id=`old-${i}`;
    f.journal.state.jobs.push({key:KEY,id,seller:'old',workstream:'piece',challenge:{buyer:'b',id:'day'},created:10+i,durationSeconds:300,
      args:{task:'[challenge:b:day]\nold task '+i,output:'text/plain',amount_sats:0,max_sats:0,payment:'none',deadline_unix:60,request_id:'rq-'+id},
      done:true,outcome:'collected',answer:`answer ${i} `+'x'.repeat(chars),pay:{state:'none',amount_sats:0},ts:20+i});
    f.snap.jobs.push({id,seller:'old',running:false,accepted:true,result:{},start:10+i,deadline:60,amountSats:0});
  }
}
const assign=(seller,workstream,task='do it',durationSeconds=300)=>({seller,workstream,task,durationSeconds});
const exitError=(code,msg)=>Object.assign(Error(`/bin/sh exited ${code}: ${msg}`),{exitCode:code,stderr:msg});

// ---------- F-1 ----------
test('#1242 F-1: a failed planner command is retried once in the same call with a reduced input; both attempts journaled',async()=>{
  assert.equal(typeof m.reducedPlannerBounds,'function','reducedPlannerBounds is exported');
  const f=fixture({planner:(x,n)=>{if(n===1)throw exitError(1,'maxplayer-swarm-glm exited 1: empty content (finish_reason=length)');return {assignments:x.sellers.map(s=>assign(s.pubkey,'piece'))};}});
  withResults(f,12);
  await f.runner.tick();
  assert.equal(f.assigns().length,2,'one retry, no more');
  assert.deepEqual(f.posts().map(a=>a.seller_pubkey),['a','b'],'the retry answered in the same tick');
  const [first,retry]=f.assigns();
  assert.equal(first.history.fullText,8);assert.equal(first.history.digested,4);
  assert.equal(retry.history.fullText,2,'fewer full results');assert.equal(retry.history.digestCharsUsed,150,'smaller digests');
  assert.equal(retry.history.answersOmitted,4,'answers older than the newest 8 dropped');
  assert.ok(JSON.stringify(retry).length<JSON.stringify(first).length/2,'materially smaller');
  assert.deepEqual(retry.plan,first.plan,'plan kept');assert.equal(retry.goal,first.goal,'goal kept');
  assert.deepEqual(retry.results.map(r=>r.jobId),first.results.map(r=>r.jobId),'every result is still listed');
  const entry=f.day().plannerRetries.at(-1);
  assert.equal(entry.mode,'assign');assert.match(entry.first.error,/empty content \(finish_reason=length\)/);
  assert.equal(entry.first.fullText,8);assert.equal(entry.retry.fullText,2);assert.equal(entry.retry.ok,true);
  assert.ok(entry.retry.chars<entry.first.chars);
  assert.equal(f.journal.saved.days[KEY].plannerRetries.length,1,'journaled');
  assert.equal(f.day().plannerCalls.assign,2,'the retry counts as a call');
  assert.ok(f.reports.some(r=>/^Planner assign failed in [\d.]+s; input \d+ chars, results 12 \(8 full, 4 digested.*: .*finish_reason=length.*; retrying once with reduced input \(\d+ chars, results 12 \(2 full, 6 digested, 4 answers omitted/.test(r)),f.reports.join('\n'));
  assert.ok(f.reports.some(r=>/^Planner assign ok \(reduced-input retry\) in /.test(r)));
});
test('#1242 F-1: empty or non-JSON planner output is retried too; a second failure is journaled and thrown (no third call)',async()=>{
  const f=fixture({planner:(x,n)=>{if(n===1)JSON.parse('');throw exitError(1,'empty content again');}});
  await assert.rejects(f.runner.tick(),/empty content again/);
  assert.equal(f.assigns().length,2);
  const entry=f.day().plannerRetries.at(-1);assert.equal(entry.retry.ok,false);assert.match(entry.retry.error,/empty content again/);
  assert.equal(f.posts().length,0);
});
test('#1242 F-1: validation rejections, timeouts and kills are NOT retried in the same call',async()=>{
  for(const failure of [
    ()=>({assignments:[{seller:'a'}],wait:true}),                              // validation rejection
    ()=>{throw Error('Command timed out');},                                   // runner kill at its timeout
    ()=>{throw exitError(1,'Command timed out');},                             // bundled LLM call's own timeout
    ()=>{throw Object.assign(Error('/bin/sh exited null: '),{exitCode:null});}, // killed (stopping runner)
    ()=>{throw exitError(1,'Invalid planner response (assign): return exactly one of …');}]) {
    const f=fixture({planner:failure});
    await assert.rejects(f.runner.tick());
    assert.equal(f.assigns().length,1,String(failure));assert.equal(f.day().plannerRetries,undefined);
  }
});
test('#1242 F-1: boundPlannerInput answersKept, the token-ish cap and its config',async()=>{
  assert.equal(typeof m.plannerInputCap,'function','plannerInputCap is exported');
  const results=Array.from({length:10},(_,i)=>({seller:'s',answer:`r${i} `+'y'.repeat(1000)}));
  const out=m.boundPlannerInput({mode:'assign',goal:'g',results},{fullResults:2,answersKept:5});
  assert.deepEqual(out.results.map(r=>r.answerOmitted?'o':r.answerChars?'d':'f').join(''),'oooooddd'+'ff');
  assert.equal(out.history.answersKept,5);
  assert.equal(m.plannerInputCap({plannerMaxInputChars:200000}),200000,'off by default');
  assert.equal(m.plannerInputCap({plannerMaxInputChars:200000,plannerMaxInputTokens:20000}),60000,'default 3 chars per token');
  assert.equal(m.plannerInputCap({plannerMaxInputChars:200000,plannerMaxInputTokens:20000,plannerCharsPerToken:4}),80000);
  assert.equal(m.plannerInputCap({plannerMaxInputChars:50000,plannerMaxInputTokens:20000,plannerCharsPerToken:4}),50000,'the char cap still applies');
  const env={MAXPLAYER_HOME:'/tmp/b',RELAY_URL:'ws://localhost'};
  const c=m.config([],{...env,SWARM_PLANNER_MAX_INPUT_TOKENS:'30000',SWARM_PLANNER_CHARS_PER_TOKEN:'2.5'});
  assert.equal(c.plannerMaxInputTokens,30000);assert.equal(c.plannerCharsPerToken,2.5);
  assert.equal(m.config([],env).plannerMaxInputTokens,0);
  for(const [k,v] of [['SWARM_PLANNER_MAX_INPUT_TOKENS','-1'],['SWARM_PLANNER_MAX_INPUT_TOKENS','50'],['SWARM_PLANNER_MAX_INPUT_TOKENS','x'],['SWARM_PLANNER_CHARS_PER_TOKEN','0'],['SWARM_PLANNER_CHARS_PER_TOKEN','nan']])
    assert.throws(()=>m.config([],{...env,[k]:v}),new RegExp(`Invalid ${k}`));
  // Through the runner: the token cap bounds what is sent.
  const f=fixture({planner:()=>({wait:true,idleReason:'x'})});withResults(f,40,2000);
  f.c.plannerMaxInputTokens=10000;f.c.plannerCharsPerToken=3;
  await f.runner.tick();
  for(const i of f.inputs)assert.ok(JSON.stringify(i).length<=30000,`input ${JSON.stringify(i).length} chars over the 30000 cap`);
  assert.equal(f.assigns()[0].history.maxInputChars,30000);
});

// ---------- F-2 ----------
test('#1242 F-2: an unknown workstream id in an otherwise valid assignment is adopted into the plan, journaled and reported',async()=>{
  assert.equal(typeof m.adoptionProblem,'function','adoptionProblem is exported');
  const f=fixture({planner:(x,n)=>n===1?{assignments:[assign('a','wsQ89','Sweep q=89 over every shift\nDetails: …'),assign('b','piece')]}:{wait:true,idleReason:'x'}});
  await f.runner.tick();
  assert.deepEqual(f.posts().map(a=>a.seller_pubkey),['a','b'],'both assignments posted');
  assert.equal(f.journal.state.jobs.find(j=>j.seller==='a').workstream,'wsQ89');
  const w=f.day().plan.workstreams.find(x=>x.id==='wsQ89');
  assert.deepEqual({...w},{id:'wsQ89',description:'Sweep q=89 over every shift',needs:{},estMinutes:5,dependsOn:[],adopted:true});
  assert.deepEqual(f.day().adoptedWorkstreams.map(({id,description,seller})=>({id,description,seller})),[{id:'wsQ89',description:'Sweep q=89 over every shift',seller:'a'}]);
  assert.ok(f.reports.some(r=>r.startsWith('Adopted workstream "wsQ89" from the assign response (it was not in the plan): "Sweep q=89 over every shift"')),f.reports.join('\n'));
  assert.equal(f.day().rejected.length,0,'nothing rejected');
  // The adopted id is a plan id from now on (validWorkstreams, record).
  f.time(105);f.snap.sellers.push(seller('c'));await f.runner.tick();
  const next=f.assigns().at(-1);
  assert.ok(next.validWorkstreams.includes('wsQ89'));
  assert.equal(next.plan.workstreams.find(x=>x.id==='wsQ89').record.running,1);
  // The analysis plan still validates with the adopted entry in it.
  assert.equal(m.analysisProblem(f.day().plan),null);
});
test('#1242 F-2: adoption is bounded per call, keeps id validation, and never adopts a stale id or an otherwise invalid assignment',async()=>{
  const sellers=Array.from({length:10},(_,i)=>`s${i}`);
  const f=fixture({sellers,planner:(x,n)=>n===1?{assignments:sellers.map((s,i)=>assign(s,`new${i}`))}:{wait:true,idleReason:'x'}});
  await f.runner.tick();
  assert.equal(m.MAX_ADOPTED_PER_CALL,8);
  assert.equal(f.posts().length,8,'8 adopted and posted');
  assert.deepEqual(f.day().adoptedWorkstreams.map(a=>a.id),sellers.slice(0,8).map((_,i)=>`new${i}`));
  assert.equal(f.day().rejected.length,2);
  for(const r of f.day().rejected)assert.match(r.reason,/^unknown workstream "new[89]": .*; not adopted: at most 8 new workstreams are adopted per assign call/);
  // Invalid id, stale id, invalid duration: rejected, and the plan is unchanged.
  const g=fixture({sellers:['a','b','c'],planner:(x,n)=>n===1?{plan:{...analysis,workstreams:[{id:'next',description:'n',needs:{},estMinutes:1,dependsOn:[]}]},assignments:[assign('a','next')]}
    :{assignments:[assign('b','bad id!'),assign('c','piece'),assign('b','x'.repeat(65))]}});
  await g.runner.tick();g.time(105);g.snap.sellers.push(seller('d'));
  g.planner=()=>({assignments:[assign('b','bad id!'),assign('c','piece'),assign('d','fresh',undefined,5)]});
  await g.runner.tick();
  const reasons=g.day().rejected.map(r=>r.reason);
  assert.match(reasons[0],/^unknown workstream "bad id!".*; not adopted: a workstream id must be 1-64 characters of A-Z a-z 0-9 _ \. : -$/);
  assert.match(reasons[1],/^stale workstream "piece" \(it was in an earlier plan; a later revision dropped it\)/);
  assert.doesNotMatch(reasons[1],/adopted/);
  assert.match(reasons[2],/^duration outside configured bounds/);
  assert.deepEqual(g.day().plan.workstreams.map(w=>w.id),['next'],'nothing adopted');
  assert.equal(g.day().adoptedWorkstreams,undefined);
  assert.match(m.adoptionProblem('x'.repeat(65)),/1-64 characters/);
  assert.match(m.adoptionProblem('ok',{planSize:m.MAX_PLAN_WORKSTREAMS}),/already has 100 workstreams/);
  assert.equal(m.adoptionProblem('wsQ89'),null);
});

// ---------- F-3 (and the F-2/F-4 prompt lines) ----------
test('#1242 F-2/F-3/F-4: planner-llm prompt asks for verbatim ids, explains adoption, asks for a feasibility line, closes exhaustive searches, explains unusable deliveries',async()=>{
  const dir=mkdtempSync(join(tmpdir(),'swarm-1242-prompt-')),bin=join(dir,'fake-claude'),out=join(dir,'prompt.txt');
  try {
    writeFileSync(bin,`#!/usr/bin/env node\nrequire('fs').writeFileSync(${JSON.stringify(out)},require('fs').readFileSync(0,'utf8'));console.log(JSON.stringify({wait:true,idleReason:'x'}));\n`,{mode:0o700});
    await command(process.execPath,['scripts/challenges/planner-llm.mjs'],{input:JSON.stringify({mode:'assign',results:[]}),env:{...process.env,SWARM_LLM_BINARY:bin}});
    const p=readFileSync(out,'utf8').replace(/\s+/g,' ');
    // F-2
    assert.match(p,/COPY IDS VERBATIM from validWorkstreams or your revised plan: never rename, suffix or re-letter one/);
    assert.match(p,/put new work in a revised "plan" first/);
    assert.match(p,/the runner adopts it as a new workstream \(description = your task's first line, no dependencies\) only when that assignment is otherwise valid, at most 8 per call, and never for an id a revision dropped/);
    // F-3
    assert.match(p,/"dependsOn":\[\],"feasibility":"\.\.\."\}/);
    assert.match(p,/FEASIBILITY: give every workstream .* a one-line "feasibility" note/);
    assert.match(p,/"best this family can reach is X; bar is Y"/);
    assert.match(p,/CLOSED QUESTIONS: a delivered result that reports an exhaustive search .* closes that question: .* do not re-propose the same search or a subset of it/);
    // F-4
    assert.match(p,/A result with deliveryFlag "unusable" .* is NOT completed work\. Reassign that workstream/);
  } finally {rmSync(dir,{recursive:true});}
});

// ---------- F-4 ----------
test('#1242 F-4: unusableDelivery flags empty, placeholder and trivially short answers only',()=>{
  assert.equal(typeof m.unusableDelivery,'function','unusableDelivery is exported');
  const u=m.unusableDelivery;
  for(const a of ['','   \n','placeholder','Placeholder.','Initializing answer.txt for wsA13_16...','[delivery contained no readable text files]','Delivered files (no inline answer):\n===== a.txt =====\nok\n','ok','42'])
    assert.ok(u(a),`flagged: ${JSON.stringify(a)}`);
  assert.match(u(''),/^empty delivery/);assert.match(u('placeholder'),/^placeholder text only \(11 non-space chars: "placeholder"\)$/);
  assert.match(u('Initializing answer.txt for wsA13_16...'),/^placeholder text only/);
  assert.match(u('42'),/^trivially short answer \(2 non-space chars, under 20\)$/);
  for(const a of ['Challenge fixture result: 42.','Delivered files (no inline answer):\n===== REPORT.txt =====\nChallenge fixture result: 42.\n','BASIS_A: [0, 1, 3, 7, 12, 20, 30, 44]','A placeholder in the middle of a long report is fine: the search covered every case and found nothing better than the bar.'])
    assert.equal(u(a),null,a);
  assert.match(u('short but fine',{minChars:40}),/under 40/);assert.equal(u('42',{minChars:0}),null);assert.ok(u('',{minChars:0}));
  const env={MAXPLAYER_HOME:'/tmp/b',RELAY_URL:'ws://localhost'};
  assert.equal(m.config([],env).unusableAnswerChars,20);assert.equal(m.config([],{...env,SWARM_UNUSABLE_ANSWER_CHARS:'40'}).unusableAnswerChars,40);
  assert.throws(()=>m.config([],{...env,SWARM_UNUSABLE_ANSWER_CHARS:'-1'}),/Invalid SWARM_UNUSABLE_ANSWER_CHARS/);
});
test('#1242 F-4: an unusable delivery is collected and paid exactly as before, annotated in the journal, and the planner is told to reassign it',async()=>{
  const f=fixture({planner:(x,n)=>n===1?{assignments:[{...assign('a','piece'),amountSats:5},assign('b','piece')]}:{wait:true,idleReason:'x'}});
  await f.runner.tick();
  for(const j of f.remote.values())j.results=[{}];
  const answers={'job-0':'placeholder','job-1':'a complete and readable answer, long enough to be used'};let current;
  const collect=f.io.mcp;f.io.mcp=async(t,a)=>{if(t==='collect')current=a.job_id;return collect(t,a);};
  f.io.answer=async()=>answers[current];
  f.time(110);await f.runner.tick();
  const collects=f.calls.filter(([t])=>t==='collect');
  assert.deepEqual(collects.map(([,a])=>a),[{job_id:'job-0'},{job_id:'job-1'}],'collect unchanged: once each, same arguments');
  const [bad,good]=['job-0','job-1'].map(id=>f.journal.state.jobs.find(j=>j.id===id));
  assert.equal(bad.outcome,'collected');assert.equal(bad.done,true);assert.deepEqual(bad.pay,{state:'Closed { paid }',amount_sats:5},'payment recorded as paid, untouched');
  assert.equal(bad.deliveryFlag,'unusable');assert.match(bad.deliveryFlagReason,/^placeholder text only/);assert.equal(bad.answer,'placeholder','the answer is kept as delivered');
  assert.equal(good.deliveryFlag,undefined);
  assert.equal(f.journal.saved.jobs.find(j=>j.id==='job-0').deliveryFlag,'unusable','journaled');
  assert.ok(f.reports.some(r=>/^Job job-0: seller a delivered an UNUSABLE answer \(placeholder text only.*\); collected and settled as usual \(pay Closed \{ paid \}\); the planner is told to reassign workstream piece$/.test(r)),f.reports.join('\n'));
  const input=f.inputs.at(-1),r=input.results.find(x=>x.jobId==='job-0');
  assert.equal(r.deliveryFlag,'unusable');assert.match(r.deliveryFlagReason,/^placeholder/);
  assert.match(r.answer,/^\[unusable delivery: placeholder text only .*; collected, but NOT completed work; reassign this workstream\]\nplaceholder$/);
  assert.equal(input.results.find(x=>x.jobId==='job-1').deliveryFlag,undefined);
  const rec=input.plan.workstreams[0].record.jobs.find(x=>x.jobId==='job-0');assert.equal(rec.status,'collected (unusable delivery)');
  const a=input.sellers?.find(s=>s.pubkey==='a');
  if(a){assert.equal(a.trackRecord.delivered,0);assert.equal(a.trackRecord.unusable,1);}
  const profile=m.sellerProfile(f.snap.sellers[0],f.journal.state.jobs);
  assert.equal(profile.trackRecord.delivered,0);assert.equal(profile.trackRecord.unusable,1);
});

// ---------- F-7 ----------
test('#1242 F-7: exhaustionStop says the window ended when it has or cannot fit the minimum task; exhausted only for a roster out of minutes',()=>{
  assert.equal(typeof m.exhaustionStop,'function','exhaustionStop is exported');
  const c={ends:1000};
  assert.equal(m.exhaustionStop(c,1000,300),'Unsolved: work window ended');
  assert.equal(m.exhaustionStop(c,1200,300),'Unsolved: work window ended');
  assert.equal(m.exhaustionStop(c,800,300),'Unsolved: work window ended (200s left, less than the 300s minimum task)');
  assert.equal(m.exhaustionStop(c,700,300),'Unsolved: all eligible sellers exhausted (no task fits remaining minutes/work window)');
  // Readers: every window text keeps the old `Unsolved: work window ended` prefix, and the exhausted text is unchanged.
  for(const t of [m.exhaustionStop(c,1000,300),m.exhaustionStop(c,800,300)])assert.match(t,/^Unsolved: (work window ended|all eligible sellers exhausted)/);
  assert.equal(m.EXHAUSTED_STOP,'Unsolved: all eligible sellers exhausted (no task fits remaining minutes/work window)');
});
test('#1242 F-7: sellers with minutes left but a window too short for the minimum task stop as "work window ended", not "sellers exhausted"',async()=>{
  const f=fixture({planner:()=>({wait:true,idleReason:'x'})});f.snap.challenge.ends=1000;
  f.time(800);await f.runner.tick(); // 200 s left, minimum task 300 s, sellers have 60 minutes each
  assert.equal(f.publishes.at(-1)?.[2],'completed');
  assert.equal(f.publishes.at(-1)[3],'Unsolved: work window ended (200s left, less than the 300s minimum task)');
  // A roster out of minutes, with plenty of window left, is still "exhausted".
  const g=fixture({planner:()=>({wait:true,idleReason:'x'})});g.snap.sellers.forEach(s=>s.minutesRemaining=1);
  await g.runner.tick();
  assert.equal(g.publishes.at(-1)?.[3],m.EXHAUSTED_STOP??'Unsolved: all eligible sellers exhausted (no task fits remaining minutes/work window)');
});
test('#1242 F-7: admission names "work window exceeded" and "challenge changed" separately',()=>{
  const f=fixture();const fresh={...structuredClone(f.snap),asOf:100};
  assert.deepEqual(f.runner.admission(fresh,f.snap.challenge,'a',300,0,4800),['work window exceeded (deadline 5100 is after ends 5000)']);
  assert.deepEqual(f.runner.admission({...fresh,challenge:{...fresh.challenge,id:'other'}},f.snap.challenge,'a',300,0,100),['challenge changed']);
});
