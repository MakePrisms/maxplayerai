// Shared browser/Node projection; no HTTP service or persisted authentication key.
import { EventStore } from '../../web/app/src/challenge/shared/model.mjs';
import { RelayReader } from '../../web/app/src/challenge/shared/relay.mjs';
import { writeFileSync, renameSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';
export function openSnapshot({url,buyer,onChange=()=>{}}) {
  const store=new EventStore();
  const reader=new RelayReader({url,buyer,store,onChange:()=>onChange(snapshot())});
  const snapshot=()=>({...store.snapshot(buyer),relay:{...reader.status}});
  reader.start();
  const timer=setInterval(()=>onChange(snapshot()),1000);
  return {snapshot,stop:()=>{clearInterval(timer);reader.stop();}};
}
// E2E consumes an atomic local artifact, exercising the same reader as the page.
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)) {
  const [url,buyer,path]=process.argv.slice(2);
  if(!url||!buyer||!path)throw Error('Usage: relay-snapshot.mjs relay-url buyer-pubkey output-file');
  const source=openSnapshot({url,buyer,onChange:s=>{writeFileSync(path+'.tmp',JSON.stringify(s),{mode:0o600});renameSync(path+'.tmp',path);console.log(JSON.stringify(s));}});
  for(const signal of ['SIGINT','SIGTERM'])process.on(signal,()=>{source.stop();process.exit(0);});
}
