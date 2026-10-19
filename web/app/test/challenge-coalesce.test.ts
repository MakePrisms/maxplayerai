import test from 'node:test';
import assert from 'node:assert/strict';
import {coalesce} from '../src/challenge/coalesce.js';

test('a burst of reader changes rebuilds the snapshot once, and a later change rebuilds again',()=>{
  const queued: (()=>void)[]=[];let runs=0;
  const changed=coalesce(()=>{runs++;},250,{set:(fn)=>{queued.push(fn);}});
  for(let i=0;i<1000;i++)changed();
  assert.equal(queued.length,1);assert.equal(runs,0);
  queued.shift()!();assert.equal(runs,1);
  changed();changed();
  assert.equal(queued.length,1);queued.shift()!();assert.equal(runs,2);
});
