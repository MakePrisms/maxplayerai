#!/usr/bin/env node
import { readFileSync } from 'node:fs';
const input=JSON.parse(readFileSync(0,'utf8'));
const n=Number(process.env.SWARM_STUB_SOLVE_AFTER??4);
const delivered=input.results.filter(r=>!r.answer.startsWith('['));
console.log(JSON.stringify(delivered.length>=n
  ? {solved:true,result:'Challenge fixture result: 42.',summary:`Solved after ${delivered.length} results`}
  : input.seller ? {task:`Return exactly the fixture answer 42. Contribution ${input.results.length+1} for ${input.seller.pubkey}. Build on prior results: ${JSON.stringify(input.results)}`}
  : {wait:true}));
