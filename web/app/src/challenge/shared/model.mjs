import { OFFER, CLAIM, RESULT, AWARD, ACCEPT, HEARTBEAT, PROFILE, MEMBERSHIP, DECISION } from '../../model/kinds.ts';
import { servingFromTags } from '../../model/serving.mjs';
import {boundMembership,membershipNote,verify,tag,one,number,hex,root,claimRef,mentions,newer,namespace,tradeNamespace,validChallenge,validMembership,validDecision,npubEncode,challengeTaskPrefix} from './events.mjs';
const latest = (events,key) => { const map=new Map(); for(const e of events) if(newer(e,map.get(key(e)))) map.set(key(e),e); return [...map.values()]; };
const ascending = (a,b) => a.created_at-b.created_at || a.id.localeCompare(b.id);
// Trusted roster: the latest buyer-signed decision per seller. Only these pubkeys' memberships
// are read for eligibility (relay.mjs rosterFilters); nothing a stranger signs can change it.
export function acceptedSellers(events,buyer) {
  const c={pubkey:buyer},latestBy=new Map();
  for(const e of events) if(e.kind===DECISION&&validDecision(e,c)&&newer(e,latestBy.get(tag(e,'p'))))latestBy.set(tag(e,'p'),e);
  return [...latestBy].filter(([,e])=>tag(e,'decision')==='accepted').map(([pk])=>pk).sort();
}
export class EventStore {
  events=new Map();
  // One stored membership per (seller, d): older versions are evicted on replacement.
  memberships=new Map();
  // Untrusted "waiting for approval" sample, latest per pubkey, newest `applicantCap` kept.
  applicants=new Map(); applicantsTruncated=false;
  constructor({applicantCap=256}={}) { this.applicantCap=applicantCap; }
  add(e) {
    // Already-stored ids return false either way; checking them first skips the id hash on re-reads.
    if(this.events.has(e?.id)||!verify(e)) return false;
    if(e.kind===MEMBERSHIP) {
      // An invalid replacement must never evict a valid one.
      if(!validMembership(e,{pubkey:tag(e,'d')})) return false;
      const key=`${e.pubkey}:${tag(e,'d')}`,old=this.memberships.get(key);
      if(old) { if(!newer(e,old)) return false; this.events.delete(old.id); }
      const kept=boundMembership(e); this.memberships.set(key,kept); this.events.set(e.id,kept); return true;
    }
    this.events.set(e.id,e); return true;
  }
  // Never consulted by eligibility, minutes or the runner; display of pending joins only.
  addApplicant(e) {
    if(e.kind!==MEMBERSHIP||!validMembership(e,{pubkey:tag(e,'d')})||!verify(e)) return false;
    const old=this.applicants.get(e.pubkey);
    if(old&&!newer(e,old)) return false;
    this.applicants.set(e.pubkey,boundMembership(e));
    if(this.applicants.size<=this.applicantCap) return true;
    let oldest; for(const a of this.applicants.values()) if(!oldest||newer(oldest,a)) oldest=a;
    this.applicants.delete(oldest.pubkey); this.applicantsTruncated=true;
    return oldest.id!==e.id;
  }
  snapshot(buyer,now=Math.floor(Date.now()/1000)) { return snapshot([...this.events.values()],buyer,now,null,true,[...this.applicants.values()]); }
}
// Pending joins: valid joined memberships with no buyer decision. Accepted sellers come from
// the trusted roster instead; denied ones are not listed publicly.
function applicantRows(events,buyer,decided) {
  return latest(events.filter(e=>validMembership(e,{pubkey:buyer})&&tag(e,'status')==='joined'&&!decided.has(e.pubkey)),e=>e.pubkey)
    .sort((a,b)=>b.created_at-a.created_at||a.pubkey.localeCompare(b.pubkey))
    .map(e=>({pubkey:e.pubkey,npub:npubEncode(e.pubkey),minutesOffered:+tag(e,'minutes'),since:+tag(e,'since'),joinedAt:e.created_at,note:membershipNote(e)}));
}
// Inputs to this pure projection must already have passed EventStore.add (signature + id).
export function snapshot(events,buyer,now,selectedChallenge=null,includeHistory=true,applicantEvents=[]) {
  const decisions=latest(events.filter(e=>validDecision(e,{pubkey:buyer})),e=>tag(e,'p'));
  const applicants=applicantRows(applicantEvents,buyer,new Set(decisions.map(e=>tag(e,'p'))));
  const challenges=latest(events.filter(e=>validChallenge(e,buyer)),e=>tag(e,'d')).sort((a,b)=>b.created_at-a.created_at||a.id.localeCompare(b.id));
  const c=selectedChallenge??challenges.find(e=>tag(e,'status')!=='cancelled')??challenges[0];
  const empty={asOf:now,challenge:null,sellers:[],budget:null,jobs:[],updates:[],applicants};
  if(!c) return empty;
  // Sellers are exactly the trusted roster: buyer-accepted pubkeys (latest decision), whose
  // memberships are read through an authors filter. Undecided joins are applicants; denied
  // sellers are not listed (their awarded jobs still count in jobs and budget).
  const roster=new Set(decisions.filter(d=>tag(d,'decision')==='accepted').map(d=>tag(d,'p')));
  const apps=latest(events.filter(e=>validMembership(e,c)&&roster.has(e.pubkey)),e=>e.pubkey);
  const trade=events.filter(e=>tradeNamespace(e) &&
    ([OFFER,AWARD,ACCEPT].includes(e.kind) ? e.pubkey===buyer :
      [CLAIM,RESULT].includes(e.kind) ? mentions(e,buyer) : true));
  const jobs=[];
  for(const offer of trade.filter(e=>e.kind===OFFER && e.pubkey===buyer && one(e,'i') && typeof tag(e,'i')==='string' && tag(e,'i').startsWith(challengeTaskPrefix(buyer,tag(c,'d'))) && e.created_at>=+tag(c,'starts') && e.created_at<=+tag(c,'ends') && one(e,'p') && hex(tag(e,'p')))) {
    const seller=tag(offer,'p');
    const deadlines=offer.tags.filter(t=>t[0]==='param'&&t[1]==='deadline');
    const deadline=deadlines.length===1?number(deadlines[0][2]):null;
    if(deadline===null) continue;
    const claims=trade.filter(e=>e.kind===CLAIM&&e.pubkey===seller&&root(e)===offer.id&&mentions(e,buyer)&&tag(e,'status')==='processing');
    const awards=trade.filter(e=>e.kind===AWARD&&e.pubkey===buyer&&root(e)===offer.id&&tag(e,'status')==='accepted'&&mentions(e,buyer)&&mentions(e,seller)&&claims.some(cl=>cl.id===claimRef(e))).sort(ascending);
    const award=awards[0]; if(!award) continue;
    const result=trade.filter(e=>e.kind===RESULT&&e.pubkey===seller&&root(e)===offer.id).sort(ascending)[0];
    const acceptance=trade.filter(e=>e.kind===ACCEPT&&e.pubkey===buyer&&root(e)===offer.id&&claimRef(e)===claimRef(award)&&mentions(e,buyer)&&mentions(e,seller)&&tag(e,'status')==='accepted'&&e.created_at>=award.created_at).sort(ascending)[0];
    const running=!acceptance&&now<deadline;
    // Seller time never closes the meter. Charge until buyer ACCEPT (or now/deadline).
    // An impossible result timestamp conservatively consumes the whole award window.
    const end=acceptance?.created_at??(result&&result.created_at<=award.created_at?deadline:now);
    const usedSeconds=Math.max(0,Math.min(end-award.created_at,deadline-award.created_at));
    const accepted=!!acceptance;
    // AWARD has no amount in protocol v1. The selected fixed-price offer is the public estimate.
    const amountTag=offer.tags.find(t=>t[0]==='amount');
    const noPayment=offer.tags.some(t=>t[0]==='param'&&t[1]==='payment'&&t[2]==='none');
    const amount=noPayment?0:(one(offer,'amount')&&amountTag?.[2]==='sat'?number(amountTag[1]):null);
    jobs.push({id:offer.id,seller,npub:npubEncode(seller),task:tag(offer,'i').slice(challengeTaskPrefix(buyer,tag(c,'d')).length),start:award.created_at,deadline,end:running?null:end,running,usedMinutes:usedSeconds/60,award:award.id,claim:claimRef(award),result:result?{id:result.id,content:result.content}:null,accepted,amountSats:amount});
  }
  const priorJobs=includeHistory?challenges.filter(old=>old.id!==c.id).flatMap(old=>snapshot(events,buyer,now,old,false).jobs.map(j=>({...j,challengeId:tag(old,'d')}))):[];
  const sellers=apps.map(app=>{
    const own=jobs.filter(j=>j.seller===app.pubkey),running=own.filter(j=>j.running);
    const decision=decisions.find(e=>tag(e,'p')===app.pubkey);
    const decisionState=decision?tag(decision,'decision'):'joined';
    const eligible=tag(app,'status')==='joined'&&app.created_at<=+tag(c,'starts')&&+tag(app,'since')<=+tag(c,'starts')&&decisionState==='accepted';
    const used=own.reduce((n,j)=>n+j.usedMinutes,0),offered=+tag(app,'minutes');
    const profile=latest(events.filter(e=>e.kind===PROFILE&&e.pubkey===app.pubkey),e=>e.pubkey)[0];
    let name=''; try { const p=JSON.parse(profile?.content??'{}'); name=typeof p.display_name==='string'?p.display_name:typeof p.name==='string'?p.name:''; } catch {}
    const beat=latest(trade.filter(e=>e.kind===HEARTBEAT&&e.pubkey===app.pubkey&&tag(e,'d')==='maxplayer-seller'),e=>e.pubkey)[0];
    const capabilities=beat?.tags.filter(t=>['agents','harness_family','harness_model','capabilities','harness_variant','hardware','rate','takes_payment'].includes(t[0]))??[];
    // R3-2: the newest beat's serving statement. No beat or no tag ⇒ 'unknown' (eligible as before).
    const served=servingFromTags(beat?.tags);
    const state=tag(app,'status')==='left'?'left':decisionState==='denied'?'denied':running.length?(running.every(j=>j.result)?'awaiting acceptance':'working'):eligible&&own.length?'done':decisionState;
    return {since:+tag(app,'since'),membershipStatus:tag(app,'status'),pubkey:app.pubkey,npub:npubEncode(app.pubkey),name,minutesOffered:offered,minutesUsed:used,minutesRemaining:offered-used,state,eligible,decision:decisionState,currentJobStart:running.length?Math.min(...running.map(j=>j.start)):null,capabilities,serving:served.count,servingState:served.state,heartbeatAt:beat?.created_at??null,note:membershipNote(app),pastJobs:[...new Map([...priorJobs.filter(j=>j.seller===app.pubkey),...own.map(j=>({...j,challengeId:tag(c,'d')}))].filter(j=>!j.running).map(j=>[j.id,j])).values()]};
  }).sort((a,b)=>a.npub.localeCompare(b.npub));
  return {asOf:now,applicants,challenge:{id:tag(c,'d'),eventId:c.id,buyer,buyerNpub:npubEncode(buyer),title:tag(c,'title'),goal:c.content,starts:+tag(c,'starts'),ends:+tag(c,'ends'),status:tag(c,'status'),result:tag(c,'result')??''},sellers,jobs,budget:{totalSats:+tag(c,'budget'),committedSats:jobs.filter(j=>!j.accepted).reduce((n,j)=>n+(j.amountSats??0),0),paidSats:jobs.filter(j=>j.accepted).reduce((n,j)=>n+(j.amountSats??0),0),estimated:true,unknownAmounts:jobs.filter(j=>j.amountSats===null).length,note:'Estimates from awarded offer prices; ACCEPT authorises payment, not proof of settlement. Private payments and fees are not visible.'},updates:tag(c,'update')?[tag(c,'update')]:[]};
}
