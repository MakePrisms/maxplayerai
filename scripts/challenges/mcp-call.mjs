// Bounded one-tool stdio MCP call. stdout contains only the decoded tool result.
import { spawn } from 'node:child_process';
const [bin,tool,input] = process.argv.slice(2);
const child = spawn(bin,['mcp'],{stdio:['pipe','pipe','inherit']});
let buffer='',done=false;
const timer=setTimeout(()=>finish(Error('MCP timeout')),90000);
function finish(error,result) { if(done)return;done=true;clearTimeout(timer);child.kill();if(error){console.error(error.message);process.exitCode=1;}else console.log(JSON.stringify(result)); }
const send=m=>child.stdin.write(JSON.stringify({jsonrpc:'2.0',...m})+'\n');
child.on('error',e=>finish(e));child.on('exit',code=>{if(!done)finish(Error(`MCP exited ${code} before response`));});
child.stdout.on('data',chunk=>{buffer+=chunk;let i;while((i=buffer.indexOf('\n'))>=0){const line=buffer.slice(0,i);buffer=buffer.slice(i+1);let m;try{m=JSON.parse(line);}catch{continue;}
if(m.id===1){if(m.error){finish(Error(JSON.stringify(m.error)));return;}send({method:'notifications/initialized'});send({id:2,method:'tools/call',params:{name:tool,arguments:JSON.parse(input)}});}
if(m.id===2){if(m.error||m.result?.isError){finish(Error(JSON.stringify(m.error??m.result)));return;}try{const r=m.result;finish(null,r.structuredContent??JSON.parse(r.content.find(c=>c.type==='text').text));}catch(e){finish(e);}}
}});
send({id:1,method:'initialize',params:{protocolVersion:'2025-06-18',capabilities:{},clientInfo:{name:'challenge-e2e',version:'1'}}});
