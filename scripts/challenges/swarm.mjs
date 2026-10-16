#!/usr/bin/env node
import { parseChallengeRelays } from '../../web/app/src/challenge/shared/relay.mjs';
import { openSnapshot } from './relay-snapshot.mjs';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync, openSync, closeSync, fsyncSync, writeSync, truncateSync } from 'node:fs';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { coordinate } from './coordinator.mjs';
const here = dirname(fileURLToPath(import.meta.url));
const seconds = () => Math.floor(Date.now()/1000);
export function command(bin, args, {input='', env=process.env, timeout=120000}={}) {
  return new Promise((resolve,reject) => {
    const child=spawn(bin,args,{env,detached:true,stdio:['pipe','pipe','pipe']});
    let out='',err='',failure;
    const kill=()=>{try{process.kill(-child.pid,'SIGKILL');}catch{}};
    const timer=setTimeout(()=>{failure=Error('Command timed out');kill();},timeout);
    child.on('error',e=>{clearTimeout(timer);reject(e);});
    child.stdout.on('data',b=>{out+=b;if(out.length>4*1024*1024){failure=Error('Command output too large');kill();}});
    child.stderr.on('data',b=>{err=(err+b).slice(-4000);});
    child.stdin.on('error',()=>{}); child.stdin.end(input);
    child.on('close',code=>{clearTimeout(timer);code===0&&!failure?resolve(out):reject(failure??Error(`${bin} exited ${code}: ${err}`));});
  });
}
export class Journal {
  constructor(path) {
    this.path=path; this.state={jobs:[],days:{}};
    let text='';try{text=readFileSync(path,'utf8');}catch(e){if(e.code!=='ENOENT')throw e;}
    // A torn final append was never acknowledged. All prior durable records remain authoritative.
    const end=text.lastIndexOf('\n')+1;
    if(end<text.length)truncateSync(path,Buffer.byteLength(text.slice(0,end)));
    for(const line of text.slice(0,end).split('\n').filter(Boolean))this.state=JSON.parse(line);
  }
  save() {
    let fd;
    try {
      fd=openSync(this.path,'a',0o600);
      const record=Buffer.from(JSON.stringify(this.state)+'\n');
      for(let offset=0;offset<record.length;){const n=writeSync(fd,record,offset,record.length-offset);if(!n)throw Error('Short journal write');offset+=n;}
      fsyncSync(fd);
    }
    catch(e) {e.journalFailure=true;throw e;}
    finally {if(fd!==undefined)closeSync(fd);}
  }
}
export function validateAnalysis(p) {
  if(!p || typeof p.approach!=='string' || !p.approach.trim() || !Array.isArray(p.workstreams) ||
    !p.workstreams.length || !p.verification || new Set(p.workstreams.map(w=>w?.id)).size!==p.workstreams.length ||
    p.workstreams.some(w=>!w || typeof w.id!=='string' || !w.id || typeof w.description!=='string' || !w.needs ||
      !Number.isFinite(w.estMinutes) || w.estMinutes<=0 || !Array.isArray(w.dependsOn) || w.dependsOn.some(id=>!p.workstreams.some(x=>x.id===id))))throw Error('Invalid analysis plan');
  return p;
}
export function validatePlan(p, mode='assign') {
  if(mode==='analyze')return validateAnalysis(p);
  if(!p||typeof p!=='object'||'task' in p||[Array.isArray(p.assignments),p.solved===true,p.wait===true].filter(Boolean).length!==1 ||
    (mode==='assess' && p.assignments))throw Error('Invalid planner response');
  if(p.solved && (typeof p.result!=='string'||!p.result.trim()))throw Error('Solved requires result');
  if(p.plan)validateAnalysis(p.plan);
  return p;
}
// Old journals without a release/receipt stay fully reserved (fail closed).
export function jobBudget(j) {
  if(j.released) return {reservedSats:0,paidSats:0};
  const pay=j.pay;
  if(j.done && pay?.state==='none' && pay.amount_sats===0) return {reservedSats:0,paidSats:0};
  if(j.done && typeof pay?.state==='string' && /^Closed \{/.test(pay.state) && Number.isSafeInteger(pay.amount_sats) && pay.amount_sats>=0)
    return {reservedSats:0,paidSats:pay.amount_sats};
  return {reservedSats:j.args.max_sats,paidSats:0};
}
export function sellerProfile(s,jobs) {
  const tags=s.capabilities??[], values=name=>tags.filter(t=>t[0]===name).flatMap(t=>t.slice(1));
  const own=jobs.filter(j=>j.seller===s.pubkey);
  const tasks=own.filter(j=>j.done||j.outcome).map(j=>({task:j.args.task,workstream:j.workstream??null,
    outcome:j.outcome,delivered:j.outcome==='collected',turnaroundSeconds:(j.ts??j.created)-j.created}));
  return {pubkey:s.pubkey,npub:s.npub??'',name:s.name??'',agents:values('agents'),
    harness:{family:values('harness_family'),model:values('harness_model'),variant:values('harness_variant')},
    capabilities:values('capabilities'),hardware:values('hardware'),minutesRemaining:s.minutesRemaining,note:s.note??'',
    pricing:{rateSats:values('rate'),takesPayment:values('takes_payment')},
    challengeSpend:{paidSats:own.reduce((n,j)=>n+jobBudget(j).paidSats,0),reservedSats:own.reduce((n,j)=>n+jobBudget(j).reservedSats,0)},
    trackRecord:{tasksDone:tasks.length,delivered:tasks.filter(t=>t.delivered).length,expired:tasks.filter(t=>t.outcome==='expired/undelivered').length,tasks}};
}
export function requestId(c,s,n) {return 'swarm-'+createHash('sha256').update(JSON.stringify([c.buyer,c.id,s,n])).digest('hex');}
// This is the buyer's explicit-request preparation identity (buyer/preparations.rs).
// Poll it read-only after a lost post reply, including after the work window closes.
export function preparationId(request) {return 'preparation:'+createHash('sha256').update(`explicit:${request}`).digest('hex');}
export function reservedBudget(snapshot,jobs,now) {
  const known=new Set(jobs.map(j=>j.id));
  return {challengeId:snapshot.challenge.id,asOf:now,totalSats:snapshot.budget.totalSats,
    committedSats:jobs.reduce((n,j)=>n+jobBudget(j).reservedSats,0)+(snapshot.jobs??[]).filter(j=>!known.has(j.id)).reduce((n,j)=>n+(j.amountSats??0),0),paidSats:jobs.reduce((n,j)=>n+jobBudget(j).paidSats,0)};
}
export class Swarm {
  constructor(config, io, journal) {this.c=config;this.io=io;this.journal=journal;this.state=journal.state;}
  save(){this.journal.save();}
  async tick() {
    let snap; let now=this.io.now();
    // Settle even after a stop or a change of challenge day.
    for(const j of this.state.jobs.filter(j=>!j.done)) {
      try {
      if(!j.id) {
        let recovered;
        try { recovered=await this.io.mcp('get_job',{job_id:preparationId(j.args.request_id)}); }
        catch(e) {
          // Failed preparations are sticky once an offer was queued. The buyer explicitly
          // supplies its ID: inspect that offer, never retry the failed preparation forever.
          const offer=e.message.match(/Offer ([0-9a-f]{64}) was queued for publication/);
          if(offer)recovered={job_id:offer[1]};
          else if(!e.message.includes('unknown preparation handle'))throw e;
        }
        if(recovered) {
          j.id=recovered.job_id??recovered.preparation_id??preparationId(j.args.request_id);this.save();
        } else {
          // No preparation exists: a post would be NEW work. Recheck admission before retrying.
          const fresh=await this.io.snapshot(),at=this.io.now();
          const reservation=fresh.challenge?reservedBudget(fresh,this.state.jobs.filter(x=>x.key===j.key),at):null;
          const valid=coordinate(fresh,{now:at,durationSeconds:j.durationSeconds??(j.args.deadline_unix-j.created),amountSats:0,budgetState:reservation});
          if(this.state.days[j.key]?.stop || fresh.challenge?.id!==j.challenge.id || fresh.challenge?.buyer!==j.challenge.buyer || at>=j.args.deadline_unix || j.args.deadline_unix>fresh.challenge.ends || !valid.sellers.some(s=>s.seller_pubkey===j.seller&&s.postingAllowed)) {
            // Unready history is transient, not proof that an intent should be discarded.
            if(valid.reasons.some(r=>['stale_snapshot','relay_not_ready','unknown_budget'].includes(r)))continue;
            j.released=true;j.done=true;j.outcome='abandoned intent';j.ts=at;this.save();continue;
          }
          const posted=await this.io.mcp('post_job',j.args);j.id=posted.job_id??posted.preparation_id;this.save();
        }
      }
      const state=await this.io.mcp('get_job',{job_id:j.id});
      now=this.io.now();
      if(state.status==='preparing')continue;
      if(state.status==='posted' && state.job_id && state.job_id!==j.id){j.id=state.job_id;this.save();continue;}
      const expensive=(state.claim_prices??[]).find(p=>p.seller_pubkey===j.seller&&Number.isSafeInteger(p.amount_sats)&&p.amount_sats>j.args.max_sats);
      if(expensive && j.priceAsked!==expensive.amount_sats) {
        j.priceAsked=expensive.amount_sats;j.outcome=`price too high: asked ${expensive.amount_sats}`;j.ts=now;this.save();
        this.io.report?.(`Job ${j.id}: ${j.outcome}`);
      }
      if(state.results?.length) {
        const collected=await this.io.mcp('collect',{job_id:j.id});
        j.answer=await this.io.answer(collected);j.ts=now;j.done=true;j.outcome='collected';j.pay=collected.pay;this.save();
      } else if(state.publish_refused && !state.awarded && !state.claims?.length) {j.released=true;j.done=true;j.outcome=`publication refused: ${state.publish_refused}`;j.ts=now;this.save();}
      else if(now>j.args.deadline_unix) {
        // Deadline alone is not proof of nonpayment. A confirmed empty/expired claim history,
        // matching expired offer and successful local award lookup prove never awarded.
        // Unresolved claims/awards/accepts or incomplete reads stay reserved and are polled again.
        const neverAwarded=state.read_confirmed===true && state.award_read_confirmed===true && state.reservation_state==='none' && (state.auto_award_state==='parked' || state.auto_award_state==null) &&
          state.offer?.deadline_unix===j.args.deadline_unix && Array.isArray(state.claims) && state.claims.every(c=>c.live===false&&c.status==='expired') &&
          Array.isArray(state.results) && !state.results.length && state.accepted===null && !state.awarded && !state.awarded_delivery_pending;
        const reconciled=state.reservation_state==='released' && state.read_confirmed===true &&
          state.offer?.deadline_unix===j.args.deadline_unix && Array.isArray(state.results) &&
          !state.results.length && state.accepted===null && !state.awarded_delivery_pending;
        j.done=j.args.payment==='none'||neverAwarded||reconciled;
        j.released=j.done;j.outcome=j.priceAsked?`price too high: asked ${j.priceAsked}`:'expired/undelivered';j.ts=now;this.save();
      }
      else if(this.c.awardFallback && now-j.created>=this.c.awardAfter && !state.awarded) {
        const claim=state.claims?.find(c=>c.live&&c.seller_pubkey===j.seller);
        if(claim)await this.io.mcp('award_claim',{job_id:j.id,claim_id:claim.claim_id,max_sats:j.args.max_sats});
      }
      } catch(e) { if(e.journalFailure)throw e;this.io.report?.(`Job ${j.id??j.args.request_id}: ${e.message}`); }
    }
    snap=await this.io.snapshot(); now=this.io.now(); const c=snap.challenge;if(!c)return;
    const key=JSON.stringify([c.buyer,c.id]);
    const day=this.state.days[key]??= {challenge:c,budget:snap.budget.totalSats,lastUpdate:0};
    if(!day.stop){day.challenge=c;day.budget=snap.budget.totalSats;}
    const jobs=this.state.jobs.filter(j=>j.key===key);
    const results=jobs.filter(j=>j.done||j.outcome).map(j=>({seller:j.seller,workstream:j.workstream??null,outcome:j.outcome,task:j.args.task,amountSats:j.args.amount_sats,...jobBudget(j),priceAsked:j.priceAsked??null,answer:j.answer??`[${j.outcome}]`,ts:j.ts??now}));
    const ready=snap.relay?.connected&&snap.relay?.authenticated&&snap.relay?.historyComplete&&!snap.relay?.error && now-snap.asOf<=5 && now>=snap.asOf;
    const stop=(update,result='')=>{day.stop={update,result};this.save();};
    if(!day.stop && ['completed','cancelled'].includes(c.status)) {day.stop={external:true};this.save();}
    const min=this.c.minDuration??120, max=this.c.maxDuration??1800;
    const eligible=s=>(s.eligible===true&&s.decision==='accepted'&&!['left','denied'].includes(s.state));
    const running=()=>[
      ...jobs.filter(j=>!j.done).map(j=>({seller:j.seller,workstream:j.workstream??null,task:j.args.task,since:j.created,deadline:j.args.deadline_unix})),
      ...(snap.jobs??[]).filter(j=>j.running&&!jobs.some(local=>local.id===j.id)).map(j=>({seller:j.seller,task:j.task,since:j.start,workstream:null}))];
    const ask=async(mode,sellers=snap.sellers.filter(eligible).map(s=>sellerProfile(s,jobs)))=>{
      const p=validatePlan(await this.io.plan({mode,goal:c.goal,challenge:c,plan:day.plan??null,sellers,
        running:running(),results,minDurationSeconds:min,maxDurationSeconds:max,budgetTotalSats:snap.budget.totalSats,
        budgetHeadroomSats:Math.max(0,snap.budget.totalSats-reservedBudget(snap,jobs,this.io.now()).committedSats-reservedBudget(snap,jobs,this.io.now()).paidSats),now:this.io.now()}),mode);
      if(mode==='analyze'){day.plan=p;this.save();}
      else if(p.plan){day.plan=p.plan;this.save();}
      return p;
    };
    if(!day.plan&&!day.stop&&ready&&c.status==='in_progress'&&now>=c.starts&&now<c.ends)
      await ask('analyze',snap.sellers.filter(eligible).map(s=>sellerProfile(s,jobs)));
    // Final results can solve even after posting has stopped.
    const resultVersion=createHash('sha256').update(JSON.stringify(results.map(r=>[r.seller,r.task,r.outcome,r.answer]))).digest('hex');
    if(results.length && day.assessed!==resultVersion && !day.stop?.external) {
      const verdict=await ask('assess');
      if(verdict.solved && !day.stop?.result) {stop(verdict.summary??'Solved',verdict.result);day.published=false;}
      day.assessed=resultVersion;this.save();
    }
    if(!day.stop && ready && c.status==='in_progress' && now>=c.starts) {
      const awaitingProjection=jobs.filter(j=>j.outcome==='collected'&&!(snap.jobs??[]).some(p=>p.id===j.id&&p.result));
      const pending=()=>[...this.state.jobs.filter(j=>!j.done),...awaitingProjection].map(j=>j.seller);
      const decision=coordinate(snap,{now,durationSeconds:min,amountSats:0,budgetState:reservedBudget(snap,jobs,now),pendingSellers:pending()});
      if(now>=c.ends)stop('Unsolved: work window ended');
      else if(!decision.reasons.length) {
        const candidates=decision.sellers.filter(s=>s.postingAllowed&&s.deadlineUnix<=c.ends);
        const p=await ask('assign',candidates.map(s=>sellerProfile(snap.sellers.find(x=>x.pubkey===s.seller_pubkey),jobs)));
        if(p.solved)stop(p.summary??'Solved',p.result);
        let budgetRejected=0;
        for(const assignment of p.assignments??[]) {
          const {seller,workstream,task,durationSeconds,amountSats=0}=assignment??{};
          const reject=reason=>this.io.report?.(`Rejected assignment for ${String(seller)}: ${reason}`);
          if(!Number.isSafeInteger(amountSats)||amountSats<0){reject('amountSats must be a nonnegative safe integer');continue;}
          if(!candidates.some(s=>s.seller_pubkey===seller)){reject('seller busy or ineligible (not in free roster)');continue;}
          if(typeof task!=='string'||!task.trim()||!day.plan.workstreams.some(w=>w.id===workstream)){reject('invalid task or workstream');continue;}
          if(!Number.isSafeInteger(durationSeconds)||durationSeconds<min||durationSeconds>max){reject('duration outside configured bounds');continue;}
          // Planning and earlier posts may be slow: recheck EVERY assignment and reservation.
          const fresh=await this.io.snapshot(),at=this.io.now();
          const check=coordinate(fresh,{now:at,durationSeconds,amountSats,budgetState:fresh.challenge?reservedBudget(fresh,jobs,at):null,pendingSellers:pending()});
          if(fresh.challenge?.id!==c.id||fresh.challenge?.buyer!==c.buyer||at+durationSeconds>fresh.challenge.ends){reject('challenge changed or work window exceeded');continue;}
          const candidate=check.sellers.find(x=>x.seller_pubkey===seller);
          if(!candidate?.postingAllowed){if(check.reasons.includes('budget_exceeded'))budgetRejected++;reject([...check.reasons,...(candidate?.reasons??['seller busy or ineligible'])].join(', '));continue;}
          const sequence=jobs.filter(j=>j.seller===seller).length+1;
          const args={task,output:'text/plain',visibility:'public',seller_pubkey:seller,payment:amountSats?'sat':'none',amount_sats:amountSats,max_sats:amountSats,deadline_unix:at+durationSeconds,request_id:requestId(c,seller,sequence)};
          const j={key,challenge:c,seller,workstream,durationSeconds,created:at,args};
          this.state.jobs.push(j);jobs.push(j);this.save();
          const posted=await this.io.mcp('post_job',args);j.id=posted.job_id??posted.preparation_id;
          if(!j.id)throw Error('Post returned no job or preparation id');this.save();
        }
        if(!day.stop && decision.budgetHeadroomSats<=0 && budgetRejected===p.assignments?.length && p.assignments?.length && p.assignments.every(a=>Number.isSafeInteger(a?.amountSats)&&a.amountSats>0) && !running().length)stop('Unsolved: budget exhausted');
        if(!day.stop&&!candidates.length&&!pending().length&&!(snap.jobs??[]).some(j=>j.running)&&!snap.sellers.some(s=>s.currentJobStart!=null))stop('Unsolved: all eligible sellers exhausted (no task fits remaining minutes/work window)');
      }
    }
    if(day.stop&&!day.stop.external&&!day.published) {
      await this.io.publish(day.challenge,day.budget,'completed',day.stop.update,day.stop.result);day.published=true;this.save();
    } else if(!day.stop&&ready&&now-day.lastUpdate>=this.c.updateEvery) {
      await this.io.publish(c,snap.budget.totalSats,c.status,`${results.length} results recorded; ${this.state.jobs.filter(j=>j.key===key&&!j.done).length} jobs running`,c.result);day.lastUpdate=now;this.save();
    }
  }
}
// maxplayer rejects unknown MAXPLAYER_* variables, so only MAXPLAYER_HOME reaches it from here.
export function childEnv(env,home) {
  const out={};for(const [k,v] of Object.entries(env)) if(!k.startsWith('MAXPLAYER_')) out[k]=v;
  return {...out,MAXPLAYER_HOME:home};
}
export function config(argv,env=process.env) {
  if(env.SWARM_AMOUNT_SATS!==undefined)throw Error('SWARM_AMOUNT_SATS removed; use assignment.amountSats');
  const values={};for(let i=0;i<argv.length;i++){const k=argv[i];if(k==='--award-fallback'){values[k]=true;continue;}if(!['--home','--binary','--relay-url','--planner','--duration-seconds','--poll-seconds','--update-seconds','--award-after-seconds'].includes(k)||argv[i+1]===undefined)throw Error(`Unknown or incomplete option ${k}`);values[k]=argv[++i];}
  const c={challengeRelays:parseChallengeRelays(env.SWARM_CHALLENGE_RELAYS),home:values['--home']??env.MAXPLAYER_HOME,binary:values['--binary']??env.SWARM_MAXPLAYER_BINARY??'maxplayer',url:values['--relay-url']??env.RELAY_URL,planner:values['--planner']??env.SWARM_PLANNER??`node ${JSON.stringify(join(here,'planner-llm.mjs'))}`,duration:+(values['--duration-seconds']??env.SWARM_DURATION_SECONDS??300),minDuration:+(env.SWARM_MIN_DURATION??120),maxDuration:+(env.SWARM_MAX_DURATION??1800),poll:+(values['--poll-seconds']??env.SWARM_POLL_SECONDS??5),updateEvery:+(values['--update-seconds']??env.SWARM_UPDATE_SECONDS??180),awardAfter:+(values['--award-after-seconds']??env.SWARM_AWARD_AFTER_SECONDS??30),awardFallback:values['--award-fallback']===true};
  if(!c.home||!c.url)throw Error('Explicit buyer home and relay URL required');
  if(!['ws:','wss:'].includes(new URL(c.url).protocol))throw Error('Invalid relay URL');
  for(const k of ['duration','minDuration','maxDuration','poll','updateEvery','awardAfter'])if(!Number.isSafeInteger(c[k])||c[k]<1)throw Error(`Invalid ${k}`);
  if(c.minDuration>c.maxDuration)throw Error('Invalid duration bounds');
  c.home=resolve(c.home);return c;
}
async function main() {
  const args=process.argv.slice(2),locked=args[0]==='--lock-held';if(locked)args.shift();
  const c=config(args);mkdirSync(join(c.home,'swarm'),{recursive:true,mode:0o700});
  if(!locked) {
    const child=spawn('flock',['-n',join(c.home,'swarm/runner.lock'),process.execPath,fileURLToPath(import.meta.url),'--lock-held',...args],{stdio:'inherit'});
    for(const sig of ['SIGINT','SIGTERM'])process.on(sig,()=>child.kill(sig));
    child.on('error',e=>{console.error(e.message);process.exitCode=1;});child.on('exit',code=>{process.exitCode=code??1;});return;
  }
  const challengesBinary=process.env.SWARM_CHALLENGES_BINARY??'maxplayer-challenges';
  const challengeRelays=(process.env.SWARM_CHALLENGE_RELAYS??'wss://nos.lol,wss://relay.primal.net,wss://offchain.pub').split(',').map(s=>s.trim()).filter(Boolean);
  if(!challengeRelays.length)throw Error('SWARM_CHALLENGE_RELAYS must not be empty');
  const env=childEnv(process.env,c.home);
  const identity=await command(c.binary,['whoami'],{env});
  const buyer=identity.match(/pubkey: ([0-9a-f]{64})/)?.[1];
  if(!buyer)throw Error('Cannot identify configured buyer');
  const journal=new Journal(join(c.home,'swarm/events.jsonl'));
  let relayCoverage;
  const source=openSnapshot({url:c.url,challengeRelays:c.challengeRelays,buyer,onChange:s=>{
    const failed=s.relay.failedRelays.join(', ');
    if(failed!==relayCoverage){relayCoverage=failed;console.error(new Date().toISOString(),failed?`Relay coverage incomplete; unavailable: ${failed}`:'All configured relays available');}
  }});
  const io={now:seconds,report:message=>console.error(new Date().toISOString(),message),snapshot:async()=>source.snapshot(),
    mcp:async(tool,args)=>JSON.parse(await command(process.execPath,[join(here,'mcp-call.mjs'),c.binary,tool,JSON.stringify(args)],{env})),
    plan:async input=>JSON.parse(await command('/bin/sh',['-c',c.planner],{input:JSON.stringify(input),env,timeout:input.mode==='analyze'?360000:180000})),
    answer:async collected=>readFileSync(join(collected.path,'answer.txt'),'utf8'),
    publish:async(challenge,budget,status,update,result)=>{
      const goal=join(c.home,'swarm/goal.txt');writeFileSync(goal,challenge.goal,{mode:0o600});
      const args=['publish',...challengeRelays.flatMap(url=>['--relay',url]),'--id',challenge.id,'--title',challenge.title,'--goal-file',goal,'--starts',new Date(challenge.starts*1000).toISOString(),'--ends',new Date(challenge.ends*1000).toISOString(),'--budget-sats',String(budget),'--status',status,'--update',String(update).slice(0,1000)];
      if(result)args.push('--result',result);await command(challengesBinary,args,{env});
    }};
  const runner=new Swarm(c,io,journal);
  let quitting=false;for(const sig of ['SIGINT','SIGTERM'])process.on(sig,()=>{quitting=true;source.stop();});
  try {
    while(!quitting){try{await runner.tick();}catch(e){if(e.journalFailure)throw e;console.error(new Date().toISOString(),e.message);}await new Promise(r=>setTimeout(r,c.poll*1000));}
  } finally {source.stop();}
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url))main().catch(e=>{console.error(e.message);process.exitCode=1;});
