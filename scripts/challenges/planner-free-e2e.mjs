#!/usr/bin/env node
// Local no-wallet E2E: reuse the deterministic planner, explicitly request free work.
import { readFileSync } from 'node:fs';
import { command } from './swarm.mjs';
const plan=JSON.parse(await command(process.execPath,[new URL('./planner-stub.mjs',import.meta.url).pathname],{input:readFileSync(0,'utf8')}));
for(const assignment of plan.assignments??[])assignment.amountSats=0;
console.log(JSON.stringify(plan));
