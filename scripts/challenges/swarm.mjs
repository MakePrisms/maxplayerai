#!/usr/bin/env node
import {challengeTaskPrefix} from '../../web/app/src/challenge/shared/events.mjs';
import { parseChallengeRelays } from '../../web/app/src/challenge/shared/relay.mjs';
import { openSnapshot } from './relay-snapshot.mjs';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync, openSync, closeSync, fsyncSync, writeSync, truncateSync, readSync, fstatSync, lstatSync, opendirSync, realpathSync, constants as fsConstants } from 'node:fs';
import { resolve, dirname, join, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { coordinate, relayReady, snapshotFresh, TRANSIENT_REASONS } from './coordinator.mjs';
const here = dirname(fileURLToPath(import.meta.url));
const seconds = () => Math.floor(Date.now()/1000);
// Process groups of in-flight commands, so a runner told to stop can kill them (planner, MCP).
const activeGroups=new Set();
export function killActiveCommands() {for(const pid of activeGroups){try{process.kill(-pid,'SIGKILL');}catch{}}activeGroups.clear();}
export function command(bin, args, {input='', env=process.env, timeout=120000}={}) {
  return new Promise((resolve,reject) => {
    const child=spawn(bin,args,{env,detached:true,stdio:['pipe','pipe','pipe']});
    if(child.pid)activeGroups.add(child.pid);child.on('close',()=>activeGroups.delete(child.pid));
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
// Delivery entries that are never answer text (execution proof, VCS and build/dependency trees).
export const DELIVERY_SKIP = new Set(['MAXPLAYER_EXECUTION_SENTINEL','.git','target','node_modules']);
// The planner-visible answer of a collected delivery. An inline answer is materialized by the
// buyer as answer.txt and returned as read: like every file it is capped at maxFileBytes (64 KiB)
// with a stated `[truncated: first N of M bytes shown]` marker. An EMPTY (or binary/unreadable)
// answer.txt is not an answer, so it falls through to the delivered files like a missing one.
// Real harnesses often deliver files instead (a report, a data file, a source tree) with no inline
// answer, so otherwise the delivered text files are concatenated: regular files only (lstat +
// O_NOFOLLOW, symlinks/devices/FIFOs never followed or opened), confined to the result directory,
// binary (NUL) files skipped, skipped names above ignored, files before subdirectories, per-file
// and total caps with stated truncation markers. The walk is bounded: directories are streamed
// (opendir), so at most maxEntries names are ever read in total and an oversized directory is
// never listed in full; the names read from each directory are sorted, so output is deterministic
// for what was read (the unseen tail is not sorted or listed). The whole read also has a deadline
// (deadlineMs): past it the function throws, and the runner records that once as answerError
// (collect is never repeated). Names with control characters are shown JSON-quoted so they cannot
// break the ===== framing.
export const DELIVERY_READ_DEADLINE_MS = 10000;
export function readDeliveredAnswer(dir, {maxFileBytes=65536, maxTotalChars=200000, maxEntries=2000, maxDepth=12, deadlineMs=DELIVERY_READ_DEADLINE_MS, clock=Date.now, opendir=opendirSync}={}) {
  const deadline=clock()+deadlineMs;
  const check=()=>{if(clock()>deadline)throw Error(`delivery read exceeded its ${deadlineMs} ms deadline`);};
  const root=realpathSync(dir);
  if(!lstatSync(root).isDirectory())throw Error('delivery path is not a directory');
  const inside=p=>{const parent=realpathSync(dirname(p));return parent===root||parent.startsWith(root+sep);};
  const show=r=>/[\u0000-\u001f\u007f]/.test(r)?JSON.stringify(r):r;
  const read=p=>{
    let fd;
    try {fd=openSync(p,fsConstants.O_RDONLY|fsConstants.O_NOFOLLOW|fsConstants.O_NONBLOCK);}
    catch(e) {if(['ELOOP','ENOENT','EMLINK','ENXIO','EACCES'].includes(e.code))return null;throw e;}
    try {
      const st=fstatSync(fd);if(!st.isFile())return null;
      const buf=Buffer.alloc(Math.min(st.size,maxFileBytes));let n=0;
      while(n<buf.length){const r=readSync(fd,buf,n,buf.length-n,n);if(!r)break;n+=r;}
      const bytes=buf.subarray(0,n);
      if(bytes.includes(0))return {binary:true,size:st.size};
      const text=bytes.toString('utf8');
      return {text:st.size>n?`${text}\n[truncated: first ${n} of ${st.size} bytes shown]`:text,size:st.size};
    } finally {closeSync(fd);}
  };
  const inline=join(root,'answer.txt');
  let st;try{st=lstatSync(inline);}catch(e){if(e.code!=='ENOENT')throw e;}
  if(st?.isFile()&&inside(inline)){const r=read(inline);if(r&&!r.binary&&r.text)return r.text;}
  const files=[],skipped=[];let entries=0,stopped=false;
  const walk=(abs,rel,depth)=>{
    if(stopped)return;
    if(depth>maxDepth){skipped.push(`${show(rel)}/ (too deep)`);return;}
    const names=[],d=opendir(abs);
    try {
      for(let e;(e=d.readSync())!==null;) {
        check();
        if(++entries>maxEntries){stopped=true;break;}
        names.push(e.name);
      }
    } finally {d.closeSync();}
    const dirs=[];
    for(const name of names.sort()) {
      check();
      if(DELIVERY_SKIP.has(name)||(depth===0&&name==='answer.txt'))continue;
      const p=join(abs,name),r=rel?`${rel}/${name}`:name,s=lstatSync(p);
      if(s.isDirectory())dirs.push([p,r]);else if(s.isFile())files.push([p,r]);else skipped.push(`${show(r)} (not a regular file)`);
    }
    for(const [p,r] of dirs)walk(p,r,depth+1);
  };
  walk(root,'',0);
  let out='',total=0,omitted=0;
  for(const [p,r] of files) {
    if(omitted){omitted++;continue;}
    check();
    const got=inside(p)?read(p):null;
    if(!got){skipped.push(`${show(r)} (unreadable)`);continue;}
    if(got.binary){skipped.push(`${show(r)} (binary)`);continue;}
    const block=`===== ${show(r)} =====\n${got.text}\n`;
    if(total+block.length>maxTotalChars){omitted=1;continue;}
    out+=block;total+=block.length;
  }
  const notes=[];
  if(omitted)notes.push(`[truncated: ${omitted} more file(s) omitted after ${maxTotalChars} chars]`);
  if(stopped)notes.push(`[truncated: delivery walk stopped after ${maxEntries} entries]`);
  if(skipped.length)notes.push(`[skipped: ${skipped.slice(0,20).join(', ')}${skipped.length>20?`, and ${skipped.length-20} more`:''}]`);
  if(!out)return `[delivery contained no readable text files]${notes.length?'\n'+notes.join('\n'):''}`;
  return `Delivered files (no inline answer):\n${out}${notes.join('\n')}`;
}
// Appended after the challenge prefix (which stays exactly as the shared projection matches it).
export const DELIVERY_NOTE = '\n\n---\nDELIVERY: give your final answer inline. Start your final message with the line MAXPLAYER-ANSWER-V1 alone on its line, then the answer (or, if it is long, a short summary of it) as plain text. If you also leave files, write the same answer or summary to answer.txt at the top level.';
// The planner sees the task it wrote: the delivery note is runner boilerplate (older journals lack it).
export const plannerTask = task => typeof task==='string'&&task.endsWith(DELIVERY_NOTE)?task.slice(0,-DELIVERY_NOTE.length):task;
// Planner call timeouts (seconds; SWARM_ANALYZE_TIMEOUT_SECONDS / SWARM_PLANNER_TIMEOUT_SECONDS).
// The runner kills the planner command at `runner`; the bundled planner-llm.mjs kills its own LLM
// call PLANNER_LLM_MARGIN_SECONDS earlier, so it can still report the failure before being killed.
export const PLANNER_LLM_MARGIN_SECONDS = 10;
export function plannerTimeouts(env=process.env) {
  const t={analyze:+(env.SWARM_ANALYZE_TIMEOUT_SECONDS??360),assign:+(env.SWARM_PLANNER_TIMEOUT_SECONDS??180)};
  for(const [k,v] of Object.entries(t))if(!Number.isSafeInteger(v)||v<=PLANNER_LLM_MARGIN_SECONDS)throw Error(`Invalid ${k==='analyze'?'SWARM_ANALYZE_TIMEOUT_SECONDS':'SWARM_PLANNER_TIMEOUT_SECONDS'}: must be an integer > ${PLANNER_LLM_MARGIN_SECONDS}`);
  const ms=mode=>(mode==='analyze'?t.analyze:t.assign)*1000;
  return {...t,runnerMs:ms,llmMs:mode=>ms(mode)-PLANNER_LLM_MARGIN_SECONDS*1000};
}
// Bounded planner history. Results stay oldest-to-newest; the newest `fullResults` keep their full
// answer, older ones get a `digestChars` digest: the head AND the tail of the text (long reports
// often put the conclusion last), joined by a marked cut. If the serialized input still exceeds
// `maxInputChars`, it degrades in this fixed order until it fits: newest answers to digests (oldest
// first), older answers omitted, then older result entries dropped (oldest first). A dropped entry
// leaves one compact line in history.droppedIndex (seller | outcome | ts | answer chars), which no
// later step removes. Every cut is marked in the text and counted in input.history.
// Non-result inputs are bounded first, whatever the budget, so one huge string cannot push every
// later call over it: each seller's trackRecord keeps its newest `trackRecordTasks` tasks (counts
// stay exact; `tasksOmitted`), each with a digested task; running tasks are digested to
// `fieldChars`; seller name/note to `fieldChars`; advertised tag arrays to 32 values of 200 chars.
// goal, challenge and plan are buyer/planner-authored and are not cut. Returns null when even that
// cannot fit.
export const TRUNCATION_MARKER = '[truncated:';
export function boundPlannerInput(input,{fullResults=8,digestChars=600,maxInputChars=200000,fieldChars=2000,trackRecordTasks=10}={}) {
  const all=input.results??[],total=all.length;
  let fieldsCut=0;
  const cut=(text,n,why)=>{
    if(typeof text!=='string'||text.length<=n)return text;
    const tail=Math.floor(n/2),head=n-tail;
    return `${text.slice(0,head)}\n${TRUNCATION_MARKER} ${why}; first ${head} and last ${tail} of ${text.length} chars shown]\n${tail?text.slice(-tail):''}`;
  };
  const field=(text,n,why)=>{const out=cut(text,n,why);if(out!==text)fieldsCut++;return out;};
  const tags=v=>Array.isArray(v)?(v.length>32||v.some(x=>typeof x==='string'&&x.length>200)?(fieldsCut++,v.slice(0,32).map(x=>typeof x==='string'?x.slice(0,200):x)):v):v;
  const seller=p=>{
    if(!p||typeof p!=='object')return p;
    const tr=p.trackRecord,tasks=Array.isArray(tr?.tasks)?tr.tasks:null;
    const kept=tasks?tasks.slice(-trackRecordTasks).map(t=>t&&typeof t==='object'?{...t,task:field(t.task,digestChars,'track-record task')}:t):tasks;
    return {...p,name:field(p.name,fieldChars,'seller name'),note:field(p.note,fieldChars,'seller note'),
      agents:tags(p.agents),capabilities:tags(p.capabilities),hardware:tags(p.hardware),
      ...(p.harness&&typeof p.harness==='object'?{harness:Object.fromEntries(Object.entries(p.harness).map(([k,v])=>[k,tags(v)]))}:{}),
      ...(p.pricing&&typeof p.pricing==='object'?{pricing:Object.fromEntries(Object.entries(p.pricing).map(([k,v])=>[k,tags(v)]))}:{}),
      ...(tr&&typeof tr==='object'?{trackRecord:{...tr,...(tasks?{tasks:kept,...(tasks.length>kept.length?{tasksOmitted:tasks.length-kept.length}:{})}:{})}}:{})};
  };
  const base={...input,
    ...(Array.isArray(input.sellers)?{sellers:input.sellers.map(seller)}:{}),
    ...(Array.isArray(input.running)?{running:input.running.map(r=>r&&typeof r==='object'?{...r,task:field(r.task,fieldChars,'running task')}:r)}:{})};
  const digest=r=>({...r,task:cut(r.task,digestChars,'older task'),answer:cut(r.answer,digestChars,'older result digest'),...(typeof r.answer==='string'&&r.answer.length>digestChars?{answerChars:r.answer.length}:{})});
  let results=all.map((r,i)=>i<total-fullResults?digest(r):r);
  let dropped=0;const droppedIndex=[];
  const build=()=>{
    const full=results.filter(r=>!r.answerChars&&!r.answerOmitted).length;
    const history={results:total,fullText:Math.min(full,total-dropped),digested:results.filter(r=>r.answerChars).length,answersOmitted:results.filter(r=>r.answerOmitted).length,resultsDropped:dropped,droppedIndex,fieldsCut,fullResultsKept:fullResults,digestChars,maxInputChars,marker:TRUNCATION_MARKER};
    return {...base,results,history};
  };
  const size=x=>JSON.stringify(x).length;
  let out=build();
  // Newest answers to digests, oldest of them first.
  for(let i=Math.max(0,total-fullResults);size(out)>maxInputChars&&i<results.length;i++){results[i]=digest(results[i]);out=build();}
  // Older answers omitted, oldest first (the newest digests stay).
  for(let i=0;size(out)>maxInputChars&&i<Math.max(0,results.length-fullResults);i++){
    const r=results[i];results[i]={...r,task:cut(r.task,200,'older task'),answer:`${TRUNCATION_MARKER} answer omitted to fit the planner input budget; ${typeof all[i].answer==='string'?all[i].answer.length:0} chars]`,answerOmitted:true};delete results[i].answerChars;out=build();
  }
  // Older entries dropped, oldest first; each leaves its index line.
  while(size(out)>maxInputChars&&results.length){
    const o=all[dropped];
    droppedIndex.push(`${String(o?.seller)} | ${String(o?.outcome).slice(0,200)} | ts ${o?.ts} | ${typeof o?.answer==='string'?o.answer.length:0} answer chars`);
    results=results.slice(1);dropped++;out=build();
  }
  return size(out)>maxInputChars?null:out;
}
// get_job fields the budget release proofs (expired paid jobs) depend on. They first reached main
// in #1162 (merge 7a14298d, from 942b40e7 and 49925c79) and are in NO release tag: v0.6.1 lacks
// them and prints the same version string as main, so the runner probes the buyer instead.
export const RELEASE_FIELDS = ['award_read_confirmed','reservation_state','auto_award_state','claim_prices'];
export const PROBE_JOB_ID = createHash('sha256').update('maxplayer-swarm get_job capability probe').digest('hex');
export const missingReleaseFields = view => RELEASE_FIELDS.filter(k=>!(view&&typeof view==='object'&&k in view));
// Floor for every assignment. Real seller harnesses spend a large part of a 2-3 minute task just
// starting the sandbox container and agent (first live run: 5 of 6 expiries were short or
// over-scoped tasks), so the default is 5 minutes. The same floor gates the assign roster: a
// seller whose remaining donated minutes cannot fit it is not offered to the planner, and no-fit
// exhaustion uses it. Minutes are still counted only by the shared projection.
export const DEFAULT_MIN_DURATION = 300;
// Seller slot vs sats for an expired job that is still reserved (no release proof yet, e.g. a
// v0.6.1 buyer whose get_job lacks the release fields). Core facts this rule rests on:
// - The seller stops executing at the offer deadline: host delivery stops the agent at the
//   deadline (DeadlineExceeded) and bounds its push at 150 s; container delivery is killed at
//   deadline + PUSH_MARGIN_SECS (300) + CONTAINER_EXIT_GRACE_SECS (120) = 420 s; a restart never
//   re-runs or finalizes a lapsed job (ResumeAction::SkipLapsed).
// - A delivery published late is still payable: derive_claim_liveness marks a past-deadline claim
//   `delivered` (accept_claim treats it like `processing`) for DELIVERY_PAY_WINDOW_SECS (7 days)
//   after the delivery, on main and v0.6.1.
// deadline + 420 s is the kill INSTANT, not a completed stop: the container loop kills only when
// now > deadline + 420 and teardown (client kill, capture, removal) starts after that. So the grace
// must exceed the execution tail by a teardown margin (minimum 420 + 60 = 480 s), and the slot is
// freed only strictly after deadline + grace. Residual: libgit2's delta search ignores the push
// deadline, so a pathological pack can outlive any grace; that is not bounded here.
// So once deadline + this grace has passed and a confirmed read shows no delivery, the old job
// should no longer be executing: its seller slot is freed for new work. The job itself stays open
// (not done): it is still polled, a late delivery is collected exactly once, and its sats stay
// reserved until collected or proven released. Minutes are untouched (the shared projection
// already charged award -> deadline).
export const EXPIRED_SLOT_GRACE_SECONDS = 600;
export const SELLER_EXECUTION_TAIL_SECONDS = 420;
export const SLOT_TEARDOWN_MARGIN_SECONDS = 60;
export const MIN_EXPIRED_SLOT_GRACE_SECONDS = SELLER_EXECUTION_TAIL_SECONDS + SLOT_TEARDOWN_MARGIN_SECONDS;
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
  // A deferred assignment has never been offered (no post_job call), so it holds nothing yet.
  if(j.released||j.deferred) return {reservedSats:0,paidSats:0};
  const pay=j.pay;
  if(j.done && pay?.state==='none' && pay.amount_sats===0) return {reservedSats:0,paidSats:0};
  if(j.done && typeof pay?.state==='string' && /^Closed \{/.test(pay.state) && Number.isSafeInteger(pay.amount_sats) && pay.amount_sats>=0)
    return {reservedSats:0,paidSats:pay.amount_sats};
  return {reservedSats:j.args.max_sats,paidSats:0};
}
export function sellerProfile(s,jobs) {
  const tags=s.capabilities??[], values=name=>tags.filter(t=>t[0]===name).flatMap(t=>t.slice(1));
  const own=jobs.filter(j=>j.seller===s.pubkey);
  const tasks=own.filter(j=>j.done||j.outcome).map(j=>({task:plannerTask(j.args.task),workstream:j.workstream??null,
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
  // Every post_job goes through here, so the log has one line per post call per request id.
  async post(args){this.io.report?.(`post_job request ${args.request_id} seller ${args.seller_pubkey}`);return this.io.mcp('post_job',args);}
  // Does this buyer's get_job report the fields that let an expired PAID job be proven released?
  // Without them such a job stays reserved forever (fail closed), so priced work is refused while
  // free work continues. A definite answer is cached for the process; a failed probe is retried.
  async releaseSupport() {
    if(this.releaseMissing!==undefined)return !this.releaseMissing.length;
    if(!this.io.probeBuyer){this.releaseMissing=[];return true;}
    this.releaseMissing=missingReleaseFields(await this.io.probeBuyer());
    this.io.report?.(this.releaseMissing.length
      ?`WARNING: buyer get_job lacks ${this.releaseMissing.join(', ')}; priced assignments will be refused (free work continues). Use a buyer built from main at or after 7a14298d (no release tag yet; v0.6.1 is too old).`
      :`Buyer get_job reports the budget release fields (${RELEASE_FIELDS.join(', ')})`);
    return !this.releaseMissing.length;
  }
  // One task per seller: any unfinished intent/job (deferred included, other challenges too) and
  // collected work whose ACCEPT/deadline is not yet projected keep the seller busy. An expired
  // job whose slot was freed (see EXPIRED_SLOT_GRACE_SECONDS) cannot be executing any more.
  pendingSellers(snap,key,now,except=null) {
    return this.state.jobs.filter(j=>j!==except&&((!j.done&&!j.slotFreed)||(j.key===key&&j.outcome==='collected'&&!(snap.jobs??[]).some(p=>p.id===j.id&&(p.accepted||now>=p.deadline))))).map(j=>j.seller);
  }
  // Full fresh admission for one assignment: challenge identity and window, eligibility,
  // leave/deny, minutes, budget headroom and one-job-per-seller. Returns failing reasons.
  admission(fresh,c,seller,durationSeconds,amountSats,at,except=null) {
    if(fresh.challenge?.id!==c.id||fresh.challenge?.buyer!==c.buyer||at+durationSeconds>fresh.challenge.ends)return ['challenge changed or work window exceeded'];
    const key=JSON.stringify([c.buyer,c.id]);
    const check=coordinate(fresh,{now:at,durationSeconds,amountSats,budgetState:reservedBudget(fresh,this.state.jobs.filter(j=>j.key===key&&j!==except),at),pendingSellers:this.pendingSellers(fresh,key,at,except)});
    const candidate=check.sellers.find(x=>x.seller_pubkey===seller);
    return candidate?.postingAllowed?[]:[...new Set([...check.reasons,...(candidate?.reasons??['seller busy or ineligible'])])];
  }
  // A deferred assignment was journaled when its ONLY failing reasons were transient readiness
  // (relay_not_ready and/or stale_snapshot) and has never been offered, so it needs no preparation
  // lookup and is dropped without one. While the snapshot is not ready (relay not ready, or not
  // genuinely fresh: same stale_snapshot rule as coordinate()) it only waits; an unready snapshot,
  // e.g. right after restart, is not evidence against it. That wait is bounded by stop, its
  // journaled work window and, only while still not ready, the max wait. Once ready, a fresh full
  // admission runs first and either posts it or drops it with the reason. Returns true when no
  // offer was posted this tick.
  async retryDeferred(j) {
    const fresh=await this.io.snapshot(),at=this.io.now(),maxWait=this.c.maxDefer??120;
    const drop=reason=>{j.released=true;j.done=true;j.outcome=`abandoned deferred assignment: ${reason}`;j.ts=at;this.save();this.io.report?.(`Dropped deferred assignment for ${j.seller}: ${reason}`);return true;};
    if(this.state.days[j.key]?.stop)return drop('challenge stopped');
    if(at+j.durationSeconds>j.challenge.ends)return drop('work window exceeded');
    if(!relayReady(fresh)||!snapshotFresh(fresh,at)) {
      if(at-j.deferredAt>maxWait)return drop(`relay snapshot still not ready after ${maxWait}s`);
      return true;
    }
    const reasons=this.admission(fresh,j.challenge,j.seller,j.durationSeconds,j.args.amount_sats,at,j);
    if(reasons.length)return drop(reasons.join(', '));
    // Freeze the exact arguments durably BEFORE the first post; from here on this is an ordinary
    // unposted intent (preparation lookup before any retry), so the request is never re-posted.
    j.deferred=false;j.created=at;j.args={...j.args,deadline_unix:at+j.durationSeconds};this.save();
    const posted=await this.post(j.args);j.id=posted.job_id??posted.preparation_id;
    if(!j.id)throw Error('Post returned no job or preparation id');this.save();
    this.io.report?.(`Posted deferred assignment for ${j.seller} after ${at-j.deferredAt}s`);
    return false;
  }
  async tick() {
    let snap; let now=this.io.now();
    // Settle even after a stop or a change of challenge day.
    for(const j of this.state.jobs.filter(j=>!j.done)) {
      try {
      if(!j.id&&j.deferred) {if(await this.retryDeferred(j))continue;}
      else if(!j.id) {
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
          const posted=await this.post(j.args);j.id=posted.job_id??posted.preparation_id;this.save();
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
        // collect succeeded (and settled): record it exactly once, whatever the answer read does.
        // An unreadable answer is an error the planner sees, never a reason to collect again.
        let answer,answerError;
        try {answer=await this.io.answer(collected);if(typeof answer!=='string')throw Error('answer is not text');}
        catch(e) {answerError=String(e?.message??e).slice(0,500);answer=`[collected, answer unreadable: ${answerError}]`;
          this.io.report?.(`Job ${j.id}: collected (pay ${collected?.pay?.state}) but answer unreadable: ${answerError}`);}
        // done records collection, not release of the seller slot; pending() waits for meter closure.
        Object.assign(j,{answer,ts:now,done:true,outcome:'collected',pay:collected?.pay,...(answerError?{answerError}:{})});this.save();
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
        j.released=j.done;j.outcome=j.priceAsked?`price too high: asked ${j.priceAsked}`:'expired/undelivered';j.ts=now;
        // Not provably released: keep the sats reserved and keep polling, but free the seller slot
        // once it cannot still be executing and a confirmed read shows no delivery.
        const grace=this.c.expiredSlotGrace??EXPIRED_SLOT_GRACE_SECONDS;
        if(!j.done&&!j.slotFreed&&now>j.args.deadline_unix+grace&&state.read_confirmed===true&&Array.isArray(state.results)&&!state.results.length&&!state.awarded_delivery_pending) {
          j.slotFreed=now;
          this.io.report?.(`Job ${j.id}: expired undelivered ${grace}s past its deadline; seller ${j.seller} freed for new work, ${jobBudget(j).reservedSats} sats stay reserved until collected or proven released`);
        }
        this.save();
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
    const results=jobs.filter(j=>j.done||j.outcome).map(j=>({seller:j.seller,workstream:j.workstream??null,outcome:j.outcome,task:plannerTask(j.args.task),amountSats:j.args.amount_sats,...jobBudget(j),priceAsked:j.priceAsked??null,answer:j.answer??`[${j.outcome}]`,...(j.answerError?{answerError:j.answerError}:{}),ts:j.ts??now}));
    const ready=relayReady(snap)&&snapshotFresh(snap,now);
    const stop=(update,result='')=>{day.stop={update,result};this.save();};
    if(!day.stop && ['completed','cancelled'].includes(c.status)) {day.stop={external:true};this.save();}
    const min=this.c.minDuration??DEFAULT_MIN_DURATION, max=this.c.maxDuration??1800;
    const eligible=s=>(s.eligible===true&&s.decision==='accepted'&&!['left','denied'].includes(s.state));
    const running=()=>[
      ...jobs.filter(j=>!j.done&&!j.slotFreed).map(j=>({seller:j.seller,workstream:j.workstream??null,task:plannerTask(j.args.task),since:j.created,deadline:j.args.deadline_unix,...(j.deferred?{deferred:true}:{})})),
      ...(snap.jobs??[]).filter(j=>j.running&&!jobs.some(local=>local.id===j.id&&!local.done)).map(j=>({seller:j.seller,task:j.task,since:j.start,workstream:null}))];
    const ask=async(mode,sellers=snap.sellers.filter(eligible).map(s=>sellerProfile(s,jobs)))=>{
      const raw={mode,goal:c.goal,challenge:c,plan:day.plan??null,sellers,
        running:running(),results,minDurationSeconds:min,maxDurationSeconds:max,budgetTotalSats:snap.budget.totalSats,
        budgetHeadroomSats:Math.max(0,snap.budget.totalSats-reservedBudget(snap,jobs,this.io.now()).committedSats-reservedBudget(snap,jobs,this.io.now()).paidSats),now:this.io.now(),
        ...(this.releaseMissing?.length?{paidWorkRefused:`buyer get_job lacks ${this.releaseMissing.join(', ')}; assign only amountSats 0`}:{})};
      const input=boundPlannerInput(raw,{fullResults:this.c.plannerFullResults??8,digestChars:this.c.plannerDigestChars??600,maxInputChars:this.c.plannerMaxInputChars??200000});
      if(!input)throw Error(`Planner ${mode} input exceeds SWARM_PLANNER_MAX_INPUT_CHARS (${this.c.plannerMaxInputChars??200000}) even with every result answer omitted`);
      const chars=JSON.stringify(input).length,h=input.history,started=Date.now();let ok=false;
      try {
        const p=validatePlan(await this.io.plan(input),mode);ok=true;
        if(mode==='analyze'){day.plan=p;this.save();}
        else if(p.plan){day.plan=p.plan;this.save();}
        return p;
      } finally {
        this.io.report?.(`Planner ${mode} ${ok?'ok':'failed'} in ${((Date.now()-started)/1000).toFixed(1)}s; input ${chars} chars, results ${h.results} (${h.fullText} full, ${h.digested} digested, ${h.answersOmitted} answers omitted, ${h.resultsDropped} dropped)`);
      }
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
      const pending=()=>this.pendingSellers(snap,key,now);
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
          if(amountSats>0) {
            let ok;try{ok=await this.releaseSupport();}catch(e){reject(`priced work refused: cannot verify buyer get_job release fields (${e.message})`);continue;}
            if(!ok){reject(`priced work refused: buyer get_job lacks ${this.releaseMissing.join(', ')} (needs a buyer built from main at or after 7a14298d)`);continue;}
          }
          if(!candidates.some(s=>s.seller_pubkey===seller)){reject('seller busy or ineligible (not in free roster)');continue;}
          if(typeof task!=='string'||!task.trim()||!day.plan.workstreams.some(w=>w.id===workstream)){reject('invalid task or workstream');continue;}
          if(!Number.isSafeInteger(durationSeconds)||durationSeconds<min||durationSeconds>max){reject('duration outside configured bounds');continue;}
          // Planning and earlier posts may be slow: recheck EVERY assignment and reservation.
          const fresh=await this.io.snapshot(),at=this.io.now();
          const reasons=this.admission(fresh,c,seller,durationSeconds,amountSats,at);
          // Transient readiness alone (relay gap and/or a stale read after a slow plan) defers
          // (journaled, no offer, no budget held); any other reason drops.
          const deferred=reasons.length>0&&reasons.every(r=>TRANSIENT_REASONS.includes(r));
          if(reasons.length&&!deferred){if(reasons.includes('budget_exceeded'))budgetRejected++;reject(reasons.join(', '));continue;}
          const sequence=jobs.filter(j=>j.seller===seller).length+1;
          const args={task:challengeTaskPrefix(c.buyer,c.id)+task+DELIVERY_NOTE,output:'text/plain',visibility:'public',seller_pubkey:seller,payment:amountSats?'sat':'none',amount_sats:amountSats,max_sats:amountSats,deadline_unix:at+durationSeconds,request_id:requestId(c,seller,sequence)};
          const j={key,challenge:c,seller,workstream,durationSeconds,created:at,args};
          if(deferred) {
            Object.assign(j,{deferred:true,deferredAt:at});this.state.jobs.push(j);jobs.push(j);this.save();
            this.io.report?.(`Deferred assignment for ${seller}: ${reasons.join(', ')}; retrying once the relay snapshot is ready and fresh`);continue;
          }
          this.state.jobs.push(j);jobs.push(j);this.save();
          const posted=await this.post(args);j.id=posted.job_id??posted.preparation_id;
          if(!j.id)throw Error('Post returned no job or preparation id');this.save();
        }
        if(!day.stop && decision.budgetHeadroomSats<=0 && budgetRejected===p.assignments?.length && p.assignments?.length && p.assignments.every(a=>Number.isSafeInteger(a?.amountSats)&&a.amountSats>0) && !running().length)stop('Unsolved: budget exhausted');
        // Exhaustion depends only on the trusted roster (ready = complete decision + accepted-member
        // history) and those sellers' state. Metadata caps and the applicant list never block it.
        if(!day.stop&&!candidates.length&&!pending().length&&!(snap.jobs??[]).some(j=>j.running)&&!snap.sellers.some(s=>s.currentJobStart!=null))stop('Unsolved: all eligible sellers exhausted (no task fits remaining minutes/work window)');
      }
    }
    if(day.stop&&!day.stop.external&&!day.published) {
      await this.io.publish(day.challenge,day.budget,'completed',day.stop.update,day.stop.result);day.published=true;this.save();
    } else if(!day.stop&&ready&&now-day.lastUpdate>=this.c.updateEvery) {
      await this.io.publish(c,snap.budget.totalSats,c.status,`${results.length} results recorded; ${this.state.jobs.filter(j=>j.key===key&&!j.done&&!j.slotFreed).length} jobs running`,c.result);day.lastUpdate=now;this.save();
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
  const c={challengeRelays:parseChallengeRelays(env.SWARM_CHALLENGE_RELAYS),home:values['--home']??env.MAXPLAYER_HOME,binary:values['--binary']??env.SWARM_MAXPLAYER_BINARY??'maxplayer',url:values['--relay-url']??env.RELAY_URL,planner:values['--planner']??env.SWARM_PLANNER??`node ${JSON.stringify(join(here,'planner-llm.mjs'))}`,duration:+(values['--duration-seconds']??env.SWARM_DURATION_SECONDS??300),minDuration:+(env.SWARM_MIN_DURATION??DEFAULT_MIN_DURATION),maxDuration:+(env.SWARM_MAX_DURATION??1800),poll:+(values['--poll-seconds']??env.SWARM_POLL_SECONDS??5),updateEvery:+(values['--update-seconds']??env.SWARM_UPDATE_SECONDS??180),awardAfter:+(values['--award-after-seconds']??env.SWARM_AWARD_AFTER_SECONDS??30),maxDefer:+(env.SWARM_MAX_DEFER_SECONDS??120),expiredSlotGrace:+(env.SWARM_EXPIRED_SLOT_GRACE_SECONDS??EXPIRED_SLOT_GRACE_SECONDS),awardFallback:values['--award-fallback']===true,plannerFullResults:+(env.SWARM_PLANNER_FULL_RESULTS??8),plannerDigestChars:+(env.SWARM_PLANNER_DIGEST_CHARS??600),plannerMaxInputChars:+(env.SWARM_PLANNER_MAX_INPUT_CHARS??200000)};
  c.plannerTimeouts=plannerTimeouts(env);
  if(!Number.isSafeInteger(c.plannerFullResults)||c.plannerFullResults<0)throw Error('Invalid plannerFullResults');
  for(const k of ['plannerDigestChars','plannerMaxInputChars'])if(!Number.isSafeInteger(c[k])||c[k]<100)throw Error(`Invalid ${k}`);
  if(!c.home||!c.url)throw Error('Explicit buyer home and relay URL required');
  if(!['ws:','wss:'].includes(new URL(c.url).protocol))throw Error('Invalid relay URL');
  for(const k of ['duration','minDuration','maxDuration','poll','updateEvery','awardAfter','maxDefer'])if(!Number.isSafeInteger(c[k])||c[k]<1)throw Error(`Invalid ${k}`);
  if(c.minDuration>c.maxDuration)throw Error('Invalid duration bounds');
  if(!Number.isSafeInteger(c.expiredSlotGrace)||c.expiredSlotGrace<MIN_EXPIRED_SLOT_GRACE_SECONDS)throw Error(`Invalid SWARM_EXPIRED_SLOT_GRACE_SECONDS: must be an integer >= ${MIN_EXPIRED_SLOT_GRACE_SECONDS} (seller execution tail ${SELLER_EXECUTION_TAIL_SECONDS} s + teardown margin ${SLOT_TEARDOWN_MARGIN_SECONDS} s)`);
  c.home=resolve(c.home);return c;
}
// The locked runner is a grandchild (parent -> flock -> runner) holding the home lock. Signals to
// the parent never reached it, and a killed parent (launchd `kickstart -k`, SIGKILL) left it alive
// with the lock, so every restart exited 1. The parent therefore holds the write end of the
// runner's stdin pipe: when the parent dies for ANY reason, or is asked to stop, the pipe closes
// and the runner stops: no new tick, then after PARENT_GONE_GRACE_MS it kills its in-flight
// commands and exits, releasing the lock. Journaled state is crash-safe, so this is no worse
// than a crash (request-id preparation recovery, torn-tail truncation). The grace timer is ref'd:
// it is the one thing guaranteed to run kill + exit, so it must keep the event loop alive until
// it fires (an unref'd timer let the loop end first, so in-flight commands were never killed and
// node --test on Node 22 cancelled the test and every test after it).
export const PARENT_GONE_GRACE_MS = 5000;
export function watchParent(stream,onStop,{graceMs=PARENT_GONE_GRACE_MS,exit=code=>process.exit(code),kill=killActiveCommands}={}) {
  let gone=false;
  const fire=()=>{if(gone)return;gone=true;console.error(new Date().toISOString(),'Runner parent exited or asked to stop; stopping');
    try{onStop();}catch{}
    setTimeout(()=>{kill();exit(1);},graceMs);};
  stream.on('end',fire);stream.on('close',fire);stream.on('error',fire);stream.resume();
  return ()=>gone;
}
async function main() {
  const args=process.argv.slice(2),locked=args[0]==='--lock-held';if(locked)args.shift();
  const c=config(args);mkdirSync(join(c.home,'swarm'),{recursive:true,mode:0o700});
  if(!locked) {
    const child=spawn('flock',['-n',join(c.home,'swarm/runner.lock'),process.execPath,fileURLToPath(import.meta.url),'--lock-held',...args],{stdio:['pipe','inherit','inherit'],env:{...process.env,SWARM_PARENT_PIPE:'stdin'}});
    child.stdin.on('error',()=>{});
    // Stop = close the pipe; the runner sees EOF (see watchParent). Never kill flock alone.
    for(const sig of ['SIGINT','SIGTERM'])process.on(sig,()=>child.stdin.end());
    child.on('error',e=>{console.error(e.code==='ENOENT'?'flock not found: install util-linux flock (macOS: brew install flock)':e.message);process.exitCode=1;});
    child.on('exit',code=>{process.exitCode=code??1;});return;
  }
  let stopRunner=()=>{};
  if(process.env.SWARM_PARENT_PIPE==='stdin'){delete process.env.SWARM_PARENT_PIPE;watchParent(process.stdin,()=>stopRunner());}
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
    plan:async input=>JSON.parse(await command('/bin/sh',['-c',c.planner],{input:JSON.stringify(input),env,timeout:c.plannerTimeouts.runnerMs(input.mode)})),
    answer:async collected=>readDeliveredAnswer(collected.path),
    publish:async(challenge,budget,status,update,result)=>{
      const goal=join(c.home,'swarm/goal.txt');writeFileSync(goal,challenge.goal,{mode:0o600});
      const args=['publish',...challengeRelays.flatMap(url=>['--relay',url]),'--id',challenge.id,'--title',challenge.title,'--goal-file',goal,'--starts',new Date(challenge.starts*1000).toISOString(),'--ends',new Date(challenge.ends*1000).toISOString(),'--budget-sats',String(budget),'--status',status,'--update',String(update).slice(0,1000)];
      if(result)args.push('--result',result);await command(challengesBinary,args,{env});
    }};
  io.probeBuyer=()=>io.mcp('get_job',{job_id:PROBE_JOB_ID});
  const runner=new Swarm(c,io,journal);
  try{await runner.releaseSupport();}catch(e){io.report(`WARNING: could not probe buyer get_job release fields (${e.message}); priced assignments wait for a successful probe`);}
  let quitting=false;stopRunner=()=>{quitting=true;source.stop();};for(const sig of ['SIGINT','SIGTERM'])process.on(sig,stopRunner);
  try {
    while(!quitting){try{await runner.tick();}catch(e){if(e.journalFailure)throw e;console.error(new Date().toISOString(),e.message);}await new Promise(r=>setTimeout(r,c.poll*1000));}
  } finally {source.stop();}
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url))main().catch(e=>{console.error(e.message);process.exitCode=1;});
