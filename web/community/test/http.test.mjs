import test from 'node:test';
import assert from 'node:assert/strict';
import {createApp} from '../src/server.mjs';
import {RelayReader} from '../src/relay.mjs';
import {EventStore} from '../src/model.mjs';
test('HTTP snapshot, CORS, SSE initial/update and read-only methods',async()=>{
  let value={asOf:1,challenge:null,sellers:[]};const app=createApp({origin:'https://www.maxplayer.ai',snapshot:()=>value});await new Promise(r=>app.server.listen(0,'127.0.0.1',r));const url=`http://127.0.0.1:${app.server.address().port}`;
  try{const response=await fetch(url+'/api/challenges/current');assert.equal(response.headers.get('access-control-allow-origin'),'https://www.maxplayer.ai');assert.deepEqual(await response.json(),value);assert.equal((await fetch(url+'/api/challenges/current',{method:'POST'})).status,405);
    const abort=new AbortController();const stream=await fetch(url+'/api/challenges/current/stream',{signal:abort.signal});const reader=stream.body.getReader();assert.match(new TextDecoder().decode((await reader.read()).value),/"challenge":null/);value={...value,challenge:{id:'new'}};app.send();assert.match(new TextDecoder().decode((await reader.read()).value),/"id":"new"/);abort.abort();
  }finally{app.server.closeAllConnections();await new Promise(r=>app.server.close(r));}
});
class Socket extends EventTarget {static all=[];sent=[];constructor(){super();Socket.all.push(this);}send(data){this.sent.push(JSON.parse(data));}message(data){this.dispatchEvent(new MessageEvent('message',{data:JSON.stringify(data)}));}close(){this.dispatchEvent(new Event('close'));}}
test('relay authenticates before REQ, repeats subscriptions after reconnect, sends only AUTH/REQ/CLOSE',async()=>{
  const reader=new RelayReader({url:'wss://example.test',key:'03'.padStart(64,'0'),buyer:'a'.repeat(64),store:new EventStore(),WebSocketImpl:Socket});reader.start();let socket=Socket.all.at(-1);socket.dispatchEvent(new Event('open'));assert.equal(socket.sent.length,0);socket.message(['AUTH','nonce']);assert.equal(socket.sent[0][0],'AUTH');socket.message(['OK',socket.sent[0][1].id,true,'']);assert.equal(reader.status.authenticated,true);assert.equal(reader.queue.filter(m=>m[0]==='REQ').length,9);socket.close();await new Promise(r=>setTimeout(r,1100));socket=Socket.all.at(-1);socket.message(['AUTH','again']);socket.message(['OK',socket.sent[0][1].id,true,'']);assert.equal(reader.queue.filter(m=>m[0]==='REQ').length,9);assert.ok(Socket.all.every(s=>s.sent.every(m=>['AUTH','REQ','CLOSE'].includes(m[0]))));reader.stop();
});
test('normal CLOSED acknowledgement does not reconnect; active subscription rejection does',()=>{
  const reader=new RelayReader({url:'wss://example.test',key:'03'.padStart(64,'0'),buyer:'a'.repeat(64),store:new EventStore(),WebSocketImpl:Socket});reader.start();const socket=Socket.all.at(-1);socket.message(['AUTH','nonce']);socket.message(['OK',socket.sent[0][1].id,true,'']);socket.message(['EOSE','history-30341']);socket.message(['CLOSED','history-30341','']);assert.equal(reader.status.authenticated,true);socket.message(['CLOSED','live','restricted: fixture']);assert.equal(reader.status.authenticated,false);reader.stop();
});
