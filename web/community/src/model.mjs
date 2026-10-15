import {verify,tag,one,number,hex,root,claimRef,mentions,newer,namespace,tradeNamespace,validChallenge,validMembership,validDecision,npubEncode} from './events.mjs';
const latest = (events,key) => { const map=new Map(); for(const e of events) if(newer(e,map.get(key(e)))) map.set(key(e),e); return [...map.values()]; };
const ascending = (a,b) => a.created_at-b.created_at || a.id.localeCompare(b.id);
export class EventStore {
  events=new Map();
  add(e) { if(!verify(e)||this.events.has(e.id)) return false; this.events.set(e.id,e); return true; }
  snapshot(buyer,now=Math.floor(Date.now()/1000)) { return snapshot([...this.events.values()],buyer,now); }
}
// Inputs to this pure projection must already have passed EventStore.add (signature + id).
export function snapshot(events,buyer,now,selectedChallenge=null,includeHistory=true) {
  const challenges=latest(events.filter(e=>validChallenge(e,buyer)),e=>tag(e,'d')).sort((a,b)=>b.created_at-a.created_at||a.id.localeCompare(b.id));
  const c=selectedChallenge??challenges.find(e=>tag(e,'status')!=='cancelled')??challenges[0];
  const empty={asOf:now,challenge:null,sellers:[],budget:null,jobs:[],updates:[]};
  if(!c) return empty;
  const apps=latest(events.filter(e=>validMembership(e,c)),e=>e.pubkey);
  const decisions=latest(events.filter(e=>validDecision(e,c)),e=>tag(e,'p'));
  const trade=events.filter(tradeNamespace);
  const jobs=[];
  for(const offer of trade.filter(e=>e.kind===3401 && e.pubkey===buyer && e.created_at>=+tag(c,'starts') && e.created_at<=+tag(c,'ends') && one(e,'p') && hex(tag(e,'p')))) {
    const seller=tag(offer,'p');
    const deadlines=offer.tags.filter(t=>t[0]==='param'&&t[1]==='deadline');
    const deadline=deadlines.length===1?number(deadlines[0][2]):null;
    if(deadline===null) continue;
    const claims=trade.filter(e=>e.kind===3402&&e.pubkey===seller&&root(e)===offer.id&&mentions(e,buyer)&&tag(e,'status')==='processing');
    const awards=trade.filter(e=>e.kind===3405&&e.pubkey===buyer&&root(e)===offer.id&&tag(e,'status')==='accepted'&&mentions(e,buyer)&&mentions(e,seller)&&claims.some(cl=>cl.id===claimRef(e))).sort(ascending);
    const award=awards[0]; if(!award) continue;
    const result=trade.filter(e=>e.kind===3403&&e.pubkey===seller&&root(e)===offer.id).sort(ascending)[0];
    const running=!result&&now<deadline;
    const end=result?.created_at??(running?now:deadline);
    const usedSeconds=Math.max(0,Math.min(end-award.created_at,deadline-award.created_at));
    const accepted=trade.some(e=>e.kind===3406&&e.pubkey===buyer&&root(e)===offer.id&&claimRef(e)===claimRef(award)&&mentions(e,buyer)&&mentions(e,seller)&&tag(e,'status')==='accepted');
    // 3405 has no amount in protocol v1. The selected fixed-price offer is the public estimate.
    const amountTag=offer.tags.find(t=>t[0]==='amount');
    const noPayment=offer.tags.some(t=>t[0]==='param'&&t[1]==='payment'&&t[2]==='none');
    const amount=noPayment?0:(one(offer,'amount')&&amountTag?.[2]==='sat'?number(amountTag[1]):null);
    jobs.push({id:offer.id,seller,npub:npubEncode(seller),task:tag(offer,'i')??'',start:award.created_at,deadline,end:running?null:end,running,usedMinutes:usedSeconds/60,award:award.id,claim:claimRef(award),result:result?{id:result.id,content:result.content}:null,accepted,amountSats:amount});
  }
  const priorJobs=includeHistory?challenges.filter(old=>old.id!==c.id).flatMap(old=>snapshot(events,buyer,now,old,false).jobs.map(j=>({...j,challengeId:tag(old,'d')}))):[];
  const sellers=apps.map(app=>{
    const own=jobs.filter(j=>j.seller===app.pubkey),running=own.filter(j=>j.running);
    const decision=decisions.find(e=>tag(e,'p')===app.pubkey);
    const decisionState=decision?tag(decision,'decision'):'joined';
    const eligible=tag(app,'status')==='joined'&&+tag(app,'since')<=+tag(c,'join_closes')+900&&decisionState==='accepted';
    const used=own.reduce((n,j)=>n+j.usedMinutes,0),offered=+tag(app,'minutes');
    const profile=latest(events.filter(e=>e.kind===0&&e.pubkey===app.pubkey),e=>e.pubkey)[0];
    let name=''; try { const p=JSON.parse(profile?.content??'{}'); name=typeof p.display_name==='string'?p.display_name:typeof p.name==='string'?p.name:''; } catch {}
    const beat=latest(trade.filter(e=>e.kind===30340&&e.pubkey===app.pubkey&&tag(e,'d')==='maxplayer-seller'),e=>e.pubkey)[0];
    const capabilities=beat?.tags.filter(t=>['agents','harness_family','harness_model','capabilities','harness_variant','hardware'].includes(t[0]))??[];
    const state=tag(app,'status')==='left'?'left':decisionState==='denied'?'denied':running.length?'working':eligible&&own.length?'done':decisionState;
    return {since:+tag(app,'since'),membershipStatus:tag(app,'status'),pubkey:app.pubkey,npub:npubEncode(app.pubkey),name,minutesOffered:offered,minutesUsed:used,minutesRemaining:offered-used,state,eligible,decision:decisionState,currentJobStart:running.length?Math.min(...running.map(j=>j.start)):null,capabilities,heartbeatAt:beat?.created_at??null,note:app.content,pastJobs:[...new Map([...priorJobs.filter(j=>j.seller===app.pubkey),...own.map(j=>({...j,challengeId:tag(c,'d')}))].filter(j=>!j.running).map(j=>[j.id,j])).values()]};
  }).sort((a,b)=>a.npub.localeCompare(b.npub));
  return {asOf:now,challenge:{id:tag(c,'d'),eventId:c.id,buyer,buyerNpub:npubEncode(buyer),title:tag(c,'title'),goal:c.content,joinOpens:+tag(c,'join_opens'),joinCloses:+tag(c,'join_closes'),starts:+tag(c,'starts'),ends:+tag(c,'ends'),status:tag(c,'status'),result:tag(c,'result')??''},sellers,jobs,budget:{totalSats:+tag(c,'budget'),committedSats:jobs.filter(j=>!j.accepted).reduce((n,j)=>n+(j.amountSats??0),0),paidSats:jobs.filter(j=>j.accepted).reduce((n,j)=>n+(j.amountSats??0),0),estimated:true,unknownAmounts:jobs.filter(j=>j.amountSats===null).length,note:'Estimates from awarded offer prices; 3406 authorises payment, not proof of settlement. Private payments and fees are not visible.'},updates:tag(c,'update')?[tag(c,'update')]:[]};
}
