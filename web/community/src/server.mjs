import { createServer } from 'node:http';
import { mkdirSync,readFileSync,writeFileSync,statSync } from 'node:fs';
import { dirname,resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { schnorr } from '@noble/curves/secp256k1';
import { decodeNpub,hex } from './events.mjs';
import { EventStore } from './model.mjs';
import { RelayReader } from './relay.mjs';
const escape=s=>String(s).replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
export function review(s) {
  return `<!doctype html><html><head><meta name="viewport" content="width=device-width"><title>Challenge review</title></head><body><h1>Challenge members</h1><p>Read-only. Run decisions in the buyer’s CLI home. Names, notes and capabilities are seller-advertised.</p><p>${escape(s.challenge?.title??'No challenge published')}</p>${s.sellers.map(a=>`<section><h2>${escape(a.name||a.npub)}</h2><p>${escape(a.npub)}</p><p>${a.minutesUsed.toFixed(1)} of ${a.minutesOffered} minutes today · ${escape(a.state)} · Eligible: ${a.eligible === true ? 'yes' : 'no'}</p><p>${escape(a.note)}</p><pre>${escape(JSON.stringify(a.capabilities,null,2))}</pre><p>Heartbeat: ${escape(a.heartbeatAt??'unknown')}</p><p>Past challenge jobs: ${a.pastJobs.length}</p><ul>${a.pastJobs.map(j=>`<li>${escape(j.task)} · ${j.usedMinutes.toFixed(1)} min · ${escape(j.id)}</li>`).join('')}</ul>${['accept','deny'].map(action=>`<p><code>${escape(`maxplayer challenge decide --seller ${a.npub} --${action}`)}</code></p>`).join('')}</section>`).join('')}</body></html>`;
}
export function createApp({snapshot,origin}) {
  const clients=new Set();let last='';
  const server=createServer((req,res)=>{
    const headers={'Access-Control-Allow-Origin':origin,'Vary':'Origin','Cache-Control':'no-store','X-Content-Type-Options':'nosniff'};
    if(req.method!=='GET'){res.writeHead(405,headers).end();return;}
    const path=new URL(req.url,'http://localhost').pathname;
    if(path==='/api/challenges/current/stream') {
      res.writeHead(200,{...headers,'Content-Type':'text/event-stream','Connection':'keep-alive','X-Accel-Buffering':'no'});
      res.write(`data: ${JSON.stringify(snapshot())}\n\n`);clients.add(res);req.on('close',()=>clients.delete(res));
    } else if(path==='/api/challenges/current')res.writeHead(200,{...headers,'Content-Type':'application/json'}).end(JSON.stringify(snapshot()));
    else if(path==='/review')res.writeHead(200,{...headers,'Content-Type':'text/html; charset=utf-8','Content-Security-Policy':"default-src 'none'; frame-ancestors 'none'; base-uri 'none'"}).end(review(snapshot()));
    else res.writeHead(404,headers).end('Not found');
  });
  const send=()=>{const s=snapshot();const value=JSON.stringify(s);const comparable=JSON.stringify({...s,asOf:0});if(comparable===last)return;last=comparable;for(const c of clients)if(!c.write(`data: ${value}\n\n`)){clients.delete(c);c.destroy();}};
  const timer=setInterval(send,1000),heartbeat=setInterval(()=>{for(const c of clients)c.write(': heartbeat\n\n');},15000);
  server.on('close',()=>{clearInterval(timer);clearInterval(heartbeat);for(const c of clients)c.end();});
  return {server,send};
}
export function config(env=process.env) {
  const input=env.CHALLENGE_BUYER_PUBKEY??'94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5';
  const decoded=decodeNpub(input),buyer=decoded?Buffer.from(decoded).toString('hex'):input;
  if(!hex(buyer))throw Error('CHALLENGE_BUYER_PUBKEY must be npub or lowercase hex');
  schnorr.utils.lift_x(BigInt(`0x${buyer}`));
  const url=env.RELAY_URL??'wss://relay.maxplayer.ai';if(!['ws:','wss:'].includes(new URL(url).protocol))throw Error('Invalid RELAY_URL');
  const port=Number(env.PORT??4910);if(!Number.isInteger(port)||port<0||port>65535)throw Error('Invalid PORT');
  const origin=env.CORS_ORIGIN??'https://www.maxplayer.ai';if(new URL(origin).origin!==origin)throw Error('CORS_ORIGIN must be an exact origin');
  const host=env.HOST||undefined;
  return {buyer,url,port,host,origin,keyFile:resolve(env.APP_AUTH_KEY_FILE??fileURLToPath(new URL('../.state/auth-key',import.meta.url)))};
}
export function authKey(path,buyer) {
  mkdirSync(dirname(path),{recursive:true,mode:0o700});
  try {writeFileSync(path,Buffer.from(schnorr.utils.randomPrivateKey()).toString('hex')+'\n',{flag:'wx',mode:0o600});} catch(e) {if(e.code!=='EEXIST')throw e;}
  if((statSync(path).mode&0o077)!==0)throw Error('APP_AUTH_KEY_FILE must have mode 0600');
  const value=readFileSync(path,'utf8').trim();if(!hex(value))throw Error('Invalid app auth key file');
  const key=Buffer.from(value,'hex');if(Buffer.from(schnorr.getPublicKey(key)).toString('hex')===buyer)throw Error('App auth key MUST NOT be the buyer key');return key;
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)) {
  const c=config(),store=new EventStore();let reader;
  const app=createApp({origin:c.origin,snapshot:()=>({...store.snapshot(c.buyer),relay:reader.status})});
  reader=new RelayReader({...c,key:authKey(c.keyFile,c.buyer),store,onChange:()=>app.send()});
  app.server.listen(c.port,c.host,()=>console.log(`community listening on ${c.host??'*'}:${c.port}`));reader.start();
  const stop=()=>{reader.stop();app.server.closeAllConnections();app.server.close();};process.on('SIGTERM',stop);process.on('SIGINT',stop);
}
