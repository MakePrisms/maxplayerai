#!/usr/bin/env node
import { readFileSync } from 'node:fs';
const input=JSON.parse(readFileSync(0,'utf8'));
const plan={approach:'Split fixture research and Rust verification',workstreams:[
  {id:'research',description:'Find fixture answer',needs:{skills:[]},estMinutes:2,dependsOn:[]},
  {id:'rust',description:'Verify fixture answer with Rust',needs:{skills:['rust']},estMinutes:3,dependsOn:[]}
],verification:'Check fixture answer is 42'};
const delivered=(input.results??[]).filter(r=>!r.answer.startsWith('['));
const min=input.minDurationSeconds??120;
console.log(JSON.stringify(input.mode==='analyze'?plan:
  delivered.length>=Number(process.env.SWARM_STUB_SOLVE_AFTER??4)
    ? {solved:true,result:'Challenge fixture result: 42.',summary:`Solved after ${delivered.length} results`}
    : input.mode==='assign'?{assignments:(input.sellers??[]).map(s=>{
      const rust=s.capabilities.includes('rust') && s.minutesRemaining*60>=min+10 && input.challenge.ends-input.now>=min+10 && input.maxDurationSeconds>=min+10;
      return {seller:s.pubkey,workstream:rust?'rust':'research',amountSats:rust?5:0,durationSeconds:rust?min+10:min,
        task:`Return exactly the fixture answer 42. Contribution ${input.results.length+1}. Build on prior results: ${JSON.stringify(input.results)}`};
    })}:{wait:true}));
