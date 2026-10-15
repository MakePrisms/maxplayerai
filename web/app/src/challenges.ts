import { parseSnapshot, renderSellers, type Snapshot } from "./challenge/live.js";
declare const COMMUNITY_APP_URL: string;
import { buyerHex, whitelistInstruction } from "./challenge/model.js";
import { writeClipboard } from "./ui/clipboard.js";

function text(id: string, value: string): void {
  document.getElementById(id)!.textContent = value;
}

function showTime(id: string, value: string): void {
  const date = new Date(value);
  const node = document.getElementById(id)!;
  node.textContent = new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium", timeStyle: "short",
  }).format(date);
  node.setAttribute("datetime", value);
  text(`${id}-utc`, `${date.toISOString().slice(0, 16).replace("T", " ")} UTC`);
}

// Shared chrome; no market reader, wallet, relay connection or job posting here.
const toggle = document.getElementById("nav-toggle")!;
const nav = document.getElementById("nav-links")!;
toggle.addEventListener("click", () => {
  const open = nav.classList.toggle("open");
  toggle.setAttribute("aria-expanded", String(open));
});
nav.addEventListener("click", () => {
  nav.classList.remove("open");
  toggle.setAttribute("aria-expanded", "false");
});

let snapshot: Snapshot | null = null;
let receivedAt = 0;
function render(s: Snapshot): void {
  const c = s.challenge;
  if (!c) {
    text("challenge-status", "Not announced");
    text("challenge-title", "The next challenge.");
    text("challenge-goal", "No challenge has been published yet. Check back soon.");
    for (const id of ["challenge-budget", "challenge-start", "join-open", "join-close", "buyer-npub", "buyer-hex", "buyer-instruction", "apply-command"]) text(id, "Not announced");
    for (const id of ["challenge-start-utc", "join-open-utc", "join-close-utc", "budget-progress", "budget-note"]) text(id, "");
    text("challenge-update", "No update yet."); text("challenge-result", "No result yet."); text("join-status", "Window not announced");
    document.querySelectorAll<HTMLButtonElement>("[data-challenge-copy]").forEach(b=>b.disabled=true);
    renderSellers(document, {...s,sellers:[],jobs:[]}, s.asOf);
    return;
  }
  text("challenge-title", c.title);
  text("challenge-goal", c.goal);
  text("challenge-status", {scheduled:"Scheduled",in_progress:"In progress",completed:"Completed",cancelled:"Cancelled"}[c.status]);
  text("challenge-update", s.updates.join("\n") || "No update yet.");
  text("challenge-result", c.result || "No result yet.");
  text("challenge-budget", `${s.budget!.totalSats.toLocaleString()} sats`);
  text("budget-progress", `${s.budget!.committedSats.toLocaleString()} committed · ${s.budget!.paidSats.toLocaleString()} paid (estimates)${s.budget!.unknownAmounts ? ` · ${s.budget!.unknownAmounts} unknown amounts` : ""}`);
  text("budget-note", s.budget!.note);
  showTime("challenge-start", new Date(c.starts*1000).toISOString());
  showTime("join-open", new Date(c.joinOpens*1000).toISOString());
  showTime("join-close", new Date(c.joinCloses*1000).toISOString());
  text("buyer-npub", c.buyerNpub);
  text("buyer-hex", buyerHex(c.buyerNpub));
  text("buyer-instruction", whitelistInstruction({title:c.title,goal:c.goal,buyerNpub:c.buyerNpub,startsAt:new Date(c.starts*1000).toISOString(),joinOpensAt:new Date(c.joinOpens*1000).toISOString(),budgetSats:s.budget!.totalSats,status:c.status,update:s.updates.join("\n")}));
  text("apply-command", `maxplayer challenge apply --buyer ${c.buyerNpub} --challenge ${c.id} --minutes 60`);
  document.querySelectorAll<HTMLButtonElement>("[data-challenge-copy]").forEach(b=>b.disabled=false);
  tick();
}
function tick(): void {
  if (!snapshot?.challenge) return;
  const now = snapshot.asOf + Math.max(0, (performance.now()-receivedAt)/1000);
  const c = snapshot.challenge;
  text("join-status", c.status === "cancelled" ? "Cancelled" : c.status !== "scheduled" || now >= c.joinCloses ? "Joining closed" : now < c.joinOpens ? "Joining opens soon" : "Open to join");
  renderSellers(document, snapshot, now);
}
function receive(value: unknown): void {
  const s=parseSnapshot(value); snapshot=s; receivedAt=performance.now(); render(s);
  text("connection-status", s.relay && (!s.relay.connected || !s.relay.authenticated) ? "Relay disconnected — showing last known data." : s.relay && !s.relay.historyComplete ? "Syncing relay history — totals may be incomplete." : "Live challenge updates");
}
function unavailable(): void {
  text("connection-status", snapshot ? "Live updates unavailable — showing last known data. Reconnecting…" : "Community app unavailable. Please check back later.");
}
document.querySelectorAll<HTMLButtonElement>("[data-challenge-copy]").forEach(button=>button.addEventListener("click", async()=>{
  const copied=await writeClipboard(document.getElementById(button.dataset.challengeCopy!)!.textContent!);
  text("copy-feedback",copied?"Copied.":"Could not copy. Select the text and copy it manually.");
}));
const base=COMMUNITY_APP_URL.replace(/\/$/, "");
async function connect(): Promise<void> {
  try {
    const response=await fetch(`${base}/api/challenges/current`,{signal:AbortSignal.timeout(8000)});
    if(!response.ok)throw Error("Unavailable"); receive(await response.json());
  } catch { unavailable(); }
  const stream=new EventSource(`${base}/api/challenges/current/stream`);
  stream.onmessage=e=>{try{receive(JSON.parse(e.data));}catch{unavailable();}};
  stream.onerror=unavailable;
}
void connect();
setInterval(tick,1000);
