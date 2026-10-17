#!/usr/bin/env node
// Local no-wallet E2E: reuse the deterministic planner, explicitly request free work.
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { command } from './swarm.mjs';
const input=readFileSync(0,'utf8');
const plan=JSON.parse(await command(process.execPath,[new URL('./planner-stub.mjs',import.meta.url).pathname],{input}));
for(const assignment of plan.assignments??[])assignment.amountSats=0;
// Optional coverage-gap fault: on the first non-empty assign batch, ask gap-proxy.py to drop the
// runner's challenge relay and return only once that gap is visible, i.e. between planning and
// the runner's fresh per-assignment recheck.
const gap=process.env.SWARM_E2E_GAP_TRIGGER;
if(gap&&plan.assignments?.length&&!existsSync(gap+'.done')) {
  writeFileSync(gap,'gap\n');writeFileSync(gap+'.done','');
  await new Promise(r=>setTimeout(r,1500));
}
console.log(JSON.stringify(plan));
