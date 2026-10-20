// Offers are bound to their swarm run by a buyer-signed task prefix. New offers carry
// `[swarm:<buyer>:<run>]`; offers posted before the rename carry `[challenge:<buyer>:<run>]` and
// stay on the relays, so both must bind (current jobs, the roster tally and past jobs), and each
// offer's display task is sliced by the prefix it actually starts with.
import test from 'node:test';
import assert from 'node:assert/strict';
import { schnorr } from '@noble/curves/secp256k1';
// Namespace import: on a base without the new helpers they read as undefined and the tests fail at
// their assertions rather than at module load.
import * as events from '../src/challenge/shared/events.mjs';
const {eventId}=events;
import { EventStore, snapshot } from '../src/challenge/shared/model.mjs';
const keys=[1,2,3].map(n=>n.toString(16).padStart(64,'0'));
const [buyer,seller,other]=keys.map(k=>Buffer.from(schnorr.getPublicKey(k)).toString('hex'));
const event=(kind,who,time,tags,content='')=>{const e={kind,pubkey:[buyer,seller,other][who],created_at:time,tags:[['t','maxplayer'],['v','1'],...tags],content};e.id=eventId(e);e.sig=Buffer.from(schnorr.sign(e.id,keys[who])).toString('hex');return e;};
const NEW=(d,b=buyer)=>`[swarm:${b}:${d}]\n`,OLD=(d,b=buyer)=>`[challenge:${b}:${d}]\n`;
const run=(d,starts,status)=>event(30341,0,starts-100,[['d',d],['title',d],['starts',String(starts)],['ends',String(starts+900)],['budget','1000','sat'],['status',status]]);
// One awarded job with the given task text, posted at `at`; optionally delivered and accepted.
function job(task,at,{accepted=false}={}) {
  const offer=event(3401,0,at,[['p',seller],['i',task],['amount','10','sat'],['param','deadline',String(at+600)]]);
  const claim=event(3402,1,at+1,[['e',offer.id,'','root'],['p',buyer],['status','processing']]);
  const ref=[['e',claim.id],['e',offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']];
  const award=event(3405,0,at+2,ref);
  const done=accepted?[event(3403,1,at+50,[['e',offer.id,'','root'],['p',buyer]],'Done'),event(3406,0,at+62,ref)]:[];
  return {offer,events:[offer,claim,award,...done]};
}
const roster=[event(30342,1,10,[['d',buyer],['p',buyer],['since','10'],['minutes','480'],['status','joined']]),event(30343,0,20,[['d',seller],['p',seller],['decision','accepted']])];
const project=(events,now)=>{const store=new EventStore();for(const e of events)assert.ok(store.add(e));const s=store.snapshot(buyer,now);assert.deepEqual(s,snapshot([...store.events.values()],buyer,now,null,true,[]),'cached = uncached');return s;};

test('builder: new offers start with [swarm:<buyer>:<run>]; the legacy builder keeps the old text',()=>{
  assert.equal(events.challengeTaskPrefix(buyer,'day'),`[swarm:${buyer}:day]\n`);
  assert.equal(typeof events.legacyChallengeTaskPrefix,'function','legacy builder exported');
  assert.equal(events.legacyChallengeTaskPrefix(buyer,'day'),`[challenge:${buyer}:day]\n`);
  assert.equal(typeof events.matchedTaskPrefix,'function','matchedTaskPrefix exported');
  const {matchedTaskPrefix}=events;
  assert.equal(matchedTaskPrefix(NEW('day')+'x',buyer,'day'),NEW('day'));
  assert.equal(matchedTaskPrefix(OLD('day')+'x',buyer,'day'),OLD('day'));
  for(const task of [NEW('day2')+'x',NEW('day',other)+'x',`[Swarm:${buyer}:day]\nx`,`[swarm:${buyer}:day]x`,`x[swarm:${buyer}:day]\n`,'plain',undefined,null,7])
    assert.equal(matchedTaskPrefix(task,buyer,'day'),null,String(task));
});

for(const [name,prefix] of [['new [swarm:] prefix',NEW],['old [challenge:] prefix',OLD]])
test(`a ${name} offer binds to its run: job, task text sliced by that prefix, roster minutes`,()=>{
  const j=job(prefix('day')+'Find the key',1100,{accepted:true});
  const s=project([run('day',1000,'in_progress'),...roster,...j.events],1500);
  assert.equal(s.jobs.length,1);assert.equal(s.jobs[0].id,j.offer.id);
  assert.equal(s.jobs[0].task,'Find the key','sliced by the prefix the offer carries');
  assert.equal(s.jobs[0].accepted,true);
  assert.equal(s.sellers[0].minutesUsed,1,'the roster tally counts the job (award 1102 to accept 1162)');
  assert.equal(s.budget.paidSats,10);
});

test('old and new prefixes mixed in one run: every job binds, each sliced by its own prefix',()=>{
  const a=job(OLD('day')+'old task',1100,{accepted:true}),b=job(NEW('day')+'new task',1200,{accepted:true}),c=job(NEW('day')+'running',1300);
  const s=project([run('day',1000,'in_progress'),...roster,...a.events,...b.events,...c.events],1400);
  assert.deepEqual(s.jobs.map(j=>[j.id,j.task]).sort(),[[a.offer.id,'old task'],[b.offer.id,'new task'],[c.offer.id,'running']].sort());
  assert.equal(s.sellers[0].minutesUsed,1+1+98/60,'two accepted jobs plus the running one through now');
  assert.equal(s.sellers[0].state,'working');
  assert.equal(s.budget.paidSats,20);assert.equal(s.budget.committedSats,10);
});

test('past jobs: an old-prefix past run and a new-prefix past run both appear, the current run separate',()=>{
  const old=job(OLD('run-1')+'legacy work',1100,{accepted:true}),mid=job(NEW('run-2')+'renamed work',2100,{accepted:true}),cur=job(NEW('run-3')+'today',3100);
  const s=project([run('run-1',1000,'completed'),run('run-2',2000,'completed'),run('run-3',3000,'in_progress'),...roster,...old.events,...mid.events,...cur.events],3200);
  assert.deepEqual(s.jobs.map(j=>j.task),['today']);
  assert.deepEqual(s.sellers[0].pastJobs.map(j=>[j.challengeId,j.task]).sort(),[['run-1','legacy work'],['run-2','renamed work']]);
});

test('a late event for an old-prefix past run still re-projects that run (store cache key)',()=>{
  const old=job(OLD('run-1')+'legacy work',1100),store=new EventStore();
  for(const e of [run('run-1',1000,'completed'),run('run-2',2000,'in_progress'),...roster,...old.events])assert.ok(store.add(e));
  assert.equal(store.snapshot(buyer,2100).sellers[0].pastJobs[0].accepted,false);
  const ref=[['e',old.events[1].id],['e',old.offer.id,'','root'],['p',buyer],['p',seller],['status','accepted']];
  assert.ok(store.add(event(3406,0,1150,ref)));
  assert.equal(store.snapshot(buyer,2100).sellers[0].pastJobs[0].accepted,true,'the cached facts were invalidated by the [challenge:] offer key');
});

test('neither prefix binds another buyer, another run, or a lookalike',()=>{
  const lookalikes=[NEW('day',other)+'x',OLD('day',other)+'x',NEW('day2')+'x',OLD('day2')+'x',`[SWARM:${buyer}:day]\nx`,`[ swarm:${buyer}:day]\nx`,`[swarm:${buyer}:day] x`,`[challenges:${buyer}:day]\nx`,'Normal job'];
  const events=[run('day',1000,'in_progress'),...roster,...lookalikes.flatMap((t,i)=>job(t,1100+10*i).events)];
  assert.equal(project(events,1500).jobs.length,0);
});
