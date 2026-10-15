// Local-only fixture preview and desktop/mobile screenshot check. Never contacts a relay.
import {createServer} from 'node:http';
import {readFileSync,writeFileSync,mkdirSync,existsSync} from 'node:fs';
import {spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {resolve,join} from 'node:path';
import {npubEncode} from '../src/events.mjs';
const root=fileURLToPath(new URL('../',import.meta.url)),dist=resolve(root,'../app/dist'),out=join(root,'.state');mkdirSync(out,{recursive:true});
const buyer='94989f765a69e5c392d603140ed73c7fe61e133fba767ce549558f4efc4e57d5';
const now=Math.floor(Date.now()/1000),seller='79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798';
const snapshot={asOf:now,challenge:{id:'fixture',buyer,buyerNpub:npubEncode(buyer),title:'Make the next challenge better.',goal:'Build useful tools for the community. Two sellers are working together on today’s shared goal.',starts:now-2000,ends:now+3600,joinOpens:now-6000,joinCloses:now-2400,status:'in_progress',result:'First delivery: a tested challenge dashboard.'},sellers:[{pubkey:seller,npub:npubEncode(seller),name:'Community builder',minutesOffered:60,minutesUsed:32,minutesRemaining:28,state:'working',eligible:true,decision:'accepted',currentJobStart:now-1920},{pubkey:buyer,npub:npubEncode(buyer),name:'Research seat',minutesOffered:45,minutesUsed:15,minutesRemaining:30,state:'done',eligible:true,decision:'accepted',currentJobStart:null}],jobs:[{id:'fixture-job',seller,start:now-1920,deadline:now+1680,end:null,running:true,usedMinutes:32}],budget:{totalSats:6000,committedSats:1200,paidSats:800,estimated:true,unknownAmounts:0,note:'Public estimates from awarded offer prices and payment authorisations; settlement is not visible.'},updates:['The first delivery is ready. The dashboard is being tested.'],relay:{connected:true,authenticated:true,historyComplete:true,error:null}};
const server=createServer((req,res)=>{const path=new URL(req.url,'http://localhost').pathname;if(path.startsWith('/api/')){if(path.endsWith('/stream'))res.writeHead(200,{'Content-Type':'text/event-stream'}).write(`data: ${JSON.stringify(snapshot)}\n\n`);else res.writeHead(200,{'Content-Type':'application/json'}).end(JSON.stringify(snapshot));return;}let file=join(dist,path==='/challenges'?'challenges.html':path);try{res.writeHead(200,{'Content-Type':file.endsWith('.js')?'application/javascript':file.endsWith('.css')?'text/css':file.endsWith('.html')?'text/html':'application/octet-stream'}).end(readFileSync(file));}catch{res.writeHead(404).end();}});
await new Promise(r=>server.listen(0,'127.0.0.1',r));const port=server.address().port;
const profile=join(out,`chrome-preview-${process.pid}`);const chrome=spawn('google-chrome',['--headless=new','--no-sandbox','--disable-gpu','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],{stdio:'ignore'});
const delay=ms=>new Promise(r=>setTimeout(r,ms));
let ws;
try {
  for(let n=0;n<100&&!existsSync(join(profile,'DevToolsActivePort'));n++)await delay(100);
  const debugPort=readFileSync(join(profile,'DevToolsActivePort'),'utf8').split('\n')[0];
  const tabs=await(await fetch(`http://127.0.0.1:${debugPort}/json`)).json();ws=new WebSocket(tabs.find(t=>t.type==='page').webSocketDebuggerUrl);await new Promise((r,j)=>{ws.onopen=r;ws.onerror=j;});
  let id=0;const pending=new Map();ws.onmessage=({data})=>{const m=JSON.parse(data);const p=pending.get(m.id);if(p){pending.delete(m.id);m.error?p.reject(Error(JSON.stringify(m.error))):p.resolve(m.result);}};
  const call=(method,params={})=>new Promise((resolve,reject)=>{pending.set(++id,{resolve,reject});ws.send(JSON.stringify({id,method,params}));});
  await call('Page.enable');
  const checks=[];
  for(const [label,width,height] of [['desktop',1440,1000],['mobile',390,844]]) {
    await call('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:false});await call('Page.navigate',{url:`http://127.0.0.1:${port}/challenges`});
    for(let n=0;n<50;n++){await delay(100);const r=await call('Runtime.evaluate',{expression:'document.querySelector("#challenge-sellers")?.children.length',returnByValue:true});if(r.result.value===2)break;}
    const measured=await call('Runtime.evaluate',{expression:`JSON.stringify({width:innerWidth,scroll:document.documentElement.scrollWidth,title:document.querySelector('#challenge-title').textContent,sellers:document.querySelector('#challenge-sellers').textContent,smallTargets:[...document.querySelectorAll('a,button,summary')].filter(e=>e.getClientRects().length&&e.getBoundingClientRect().height<43).map(e=>e.textContent.trim())})`,returnByValue:true});
    if(!measured.result.value){console.log(await call('Runtime.evaluate',{expression:'JSON.stringify({url:location.href,body:document.body.innerText})',returnByValue:true}));throw Error(JSON.stringify(measured));}const result=JSON.parse(measured.result.value);checks.push({label,...result});if(result.scroll>width||!result.sellers.includes('32 of 60 min'))throw Error(JSON.stringify(result));
    const viewport=await call('Page.captureScreenshot',{format:'png'});writeFileSync(join(out,`${label}-viewport.png`),Buffer.from(viewport.data,'base64'));
    const metrics=await call('Page.getLayoutMetrics');const shot=await call('Page.captureScreenshot',{format:'png',captureBeyondViewport:true,clip:{x:0,y:0,width,height:Math.ceil(metrics.cssContentSize.height),scale:1}});writeFileSync(join(out,`${label}.png`),Buffer.from(shot.data,'base64'));
  }
  writeFileSync(join(out,'preview-checks.json'),JSON.stringify(checks,null,2)+'\n');console.log(JSON.stringify(checks,null,2));
} finally {ws?.close();chrome.kill();server.closeAllConnections();server.close();}
