import { test } from 'node:test';
import assert from 'node:assert/strict';
import { coordinate, waitingOnHarnessProbe } from './coordinator.mjs';
const fixture = () => ({asOf:100,relay:{connected:true,authenticated:true,historyComplete:true},challenge:{id:'test',starts:100,ends:200,status:'in_progress'},budget:{totalSats:10,committedSats:3,paidSats:2,unknownAmounts:0},jobs:[],sellers:[{pubkey:'a'.repeat(64),eligible:true,decision:'accepted',state:'accepted',minutesRemaining:1,currentJobStart:null}]});
test('inclusive work window, fractional minutes and deterministic output',()=>{const s=fixture();assert.deepEqual(coordinate(s),coordinate(s));assert.equal(coordinate(s,{durationSeconds:60}).postingAllowed,true);assert.equal(coordinate(s,{durationSeconds:61}).postingAllowed,false);s.asOf=200;assert.equal(coordinate(s,{now:200,durationSeconds:1}).postingAllowed,true);assert.equal(coordinate(s,{now:201}).postingAllowed,false);});
test('deny, leave, busy and pending exclude sellers',()=>{for(const change of [{eligible:false},{state:'left'},{decision:'denied'},{currentJobStart:100}]){const s=fixture();Object.assign(s.sellers[0],change);assert.equal(coordinate(s).sellers.length,0);}const s=fixture();s.jobs=[{seller:s.sellers[0].pubkey,running:true}];assert.equal(coordinate(s).sellers.length,0);s.jobs=[];assert.equal(coordinate(s,{pendingSellers:[s.sellers[0].pubkey]}).sellers.length,0);});
test('budget caps and fail-closed paid estimates',()=>{const s=fixture(),budgetState={...s.budget,challengeId:'test',asOf:100};assert.equal(coordinate(s,{durationSeconds:60,amountSats:5,budgetState}).postingAllowed,true);assert.equal(coordinate(s,{durationSeconds:60,amountSats:6,budgetState}).postingAllowed,false);assert.equal(coordinate(s,{durationSeconds:60,amountSats:1}).postingAllowed,false);s.budget.unknownAmounts=1;assert.equal(coordinate(s,{durationSeconds:60}).postingAllowed,false);});
test('unready, stale, cancelled and invalid snapshots stop posting',()=>{for(const change of [{historyComplete:false},{authenticated:false},{connected:false},{error:'truncated'}]){const s=fixture();Object.assign(s.relay,change);assert.equal(coordinate(s,{durationSeconds:60}).postingAllowed,false);}assert.equal(coordinate(fixture(),{now:106,durationSeconds:60}).postingAllowed,false);const s=fixture();s.challenge.status='completed';assert.equal(coordinate(s,{durationSeconds:60}).postingAllowed,false);assert.throws(()=>coordinate(s,{amountSats:-1}));});
test('R3-2: a seller whose heartbeat says not_serving or serving=0/N is ineligible; unknown stays eligible',()=>{
  const allowed=change=>{const s=fixture();Object.assign(s.sellers[0],change);return coordinate(s,{durationSeconds:60}).sellers[0];};
  for(const change of [{servingState:'not_serving'},{serving:{available:0,configured:1},servingState:'not_serving'},{serving:{available:0,configured:2}}]){
    const r=allowed(change);assert.equal(r.postingAllowed,false,JSON.stringify(change));assert.ok(r.reasons.includes('seller_not_serving'));
  }
  for(const change of [{},{servingState:'unknown',serving:null},{servingState:'serving',serving:{available:1,configured:2}}]){
    const r=allowed(change);assert.equal(r.postingAllowed,true,JSON.stringify(change));assert.deepEqual(r.reasons,[]);
  }
});
test('R3-2 advisor r1: only a seller whose SOLE reason is seller_not_serving and whose task fits the window holds the day open',()=>{
  const d=(reasons,deadlineUnix=500,roster=[])=>({reasons:roster,sellers:[{reasons,deadlineUnix}]});
  assert.equal(waitingOnHarnessProbe(d(['seller_not_serving']),1000),true);
  assert.equal(waitingOnHarnessProbe(d(['seller_not_serving'],1000),1000),true,'deadline exactly at ends still fits');
  assert.equal(waitingOnHarnessProbe(d(['insufficient_minutes','seller_not_serving']),1000),false,'short of minutes');
  assert.equal(waitingOnHarnessProbe(d(['seller_not_serving'],1001),1000),false,'task cannot finish before ends');
  assert.equal(waitingOnHarnessProbe(d(['seller_not_serving'],500,['relay_not_ready']),1000),false,'roster-wide reason');
  assert.equal(waitingOnHarnessProbe(d([]),1000),false);
  assert.equal(waitingOnHarnessProbe({reasons:[],sellers:[]},1000),false);
});
// Run 2 #13: the challenge relay quorum decides readiness; our trade relay is always required.
test('relay quorum: degraded (quorum met) posts; below quorum or trade relay down does not',()=>{
  const trade=o=>({role:'trade',url:'ws://trade',connected:true,authenticated:true,historyComplete:true,error:null,...o});
  const s=(relay)=>{const x=fixture();x.relay={...x.relay,error:null,...relay};return x;};
  assert.equal(coordinate(s({failedRelays:['ws://c'],quorum:{required:2,complete:2,total:3},degraded:true,relays:[trade()]}),{durationSeconds:60}).postingAllowed,true);
  assert.deepEqual(coordinate(s({failedRelays:['ws://b','ws://c'],quorum:{required:2,complete:1,total:3},relays:[trade()]}),{durationSeconds:60}).reasons,['relay_not_ready']);
  assert.deepEqual(coordinate(s({failedRelays:['ws://trade'],quorum:{required:2,complete:3,total:3},relays:[trade({historyComplete:false})]}),{durationSeconds:60}).reasons,['relay_not_ready']);
  // A snapshot from a reader without quorum fields still needs every relay.
  assert.deepEqual(coordinate(s({failedRelays:['ws://c']}),{durationSeconds:60}).reasons,['relay_not_ready']);
});
// Advisor r1 nit: relayReady re-checks that the quorum is a strict majority (and complete <= total).
test('relay quorum: a snapshot claiming a sub-majority quorum is never ready',()=>{
  const trade={role:'trade',url:'ws://trade',connected:true,authenticated:true,historyComplete:true,error:null};
  const s=quorum=>{const x=fixture();x.relay={...x.relay,error:null,failedRelays:[],relays:[trade],quorum};return x;};
  assert.equal(coordinate(s({required:2,complete:2,total:3}),{durationSeconds:60}).postingAllowed,true);
  for(const q of [{required:1,complete:1,total:3},{required:2,complete:2,total:4},{required:4,complete:4,total:3},{required:2,complete:4,total:3},{required:0,complete:0,total:0}])
    assert.deepEqual(coordinate(s(q),{durationSeconds:60}).reasons,['relay_not_ready'],JSON.stringify(q));
});
