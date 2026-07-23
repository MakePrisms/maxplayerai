import { parseEvent } from "./parse.js";
import { createStore } from "./store.js";
import { createRelayClient } from "./relay.js";
import {
  renderBuyerProfile,
  renderConnection,
  renderFeed,
  renderPulse,
  renderSellerProfile,
  renderStats,
} from "./views.js";

const store = createStore();
const pulseNode = document.getElementById("pulse");
const tabsNode = document.getElementById("tabs");
const feedNode = document.getElementById("feed");
const statsNode = document.getElementById("stats");
const profileNode = document.getElementById("profile");
const connNode = document.getElementById("conn");
const retryBtn = document.getElementById("retry");
const tabFeed = document.getElementById("tab-feed");
const tabStats = document.getElementById("tab-stats");

/** Which marketplace tab is showing — feed is the landing view. */
let activeTab = "feed";
/** Current route: {view:"main"} or {view:"profile", role, pubkey}. */
let route = { view: "main" };
/** Job ids the user has expanded; preserved across live re-renders. */
const expanded = new Set();

let conn = { state: "connecting", detail: "", url: "", attempt: 0 };
let raf = 0;

/** Parse `#/seller/<pk>` or `#/buyer/<pk>`; anything else is the main view. */
function parseRoute() {
  const m = /^#\/(seller|buyer)\/([0-9a-fA-F]{64})$/.exec(location.hash || "");
  if (m) return { view: "profile", role: m[1], pubkey: m[2].toLowerCase() };
  return { view: "main" };
}

function applyRoute() {
  route = parseRoute();
  const onProfile = route.view === "profile";
  // Marketplace chrome (pulse + tabs + panels) hides while a profile page is up.
  pulseNode.classList.toggle("hidden", onProfile);
  tabsNode.classList.toggle("hidden", onProfile);
  profileNode.classList.toggle("hidden", !onProfile);
  if (onProfile) {
    feedNode.classList.add("hidden");
    statsNode.classList.add("hidden");
    window.scrollTo(0, 0);
  } else {
    feedNode.classList.toggle("hidden", activeTab !== "feed");
    statsNode.classList.toggle("hidden", activeTab !== "stats");
  }
  scheduleRender();
}

function scheduleRender() {
  if (raf) return;
  raf = requestAnimationFrame(() => {
    raf = 0;
    try {
      const now = Math.floor(Date.now() / 1000);
      renderConnection(connNode, conn);
      if (route.view === "profile") {
        if (route.role === "seller") {
          renderSellerProfile(profileNode, store.sellerProfile(route.pubkey, now), expanded, toggleJob, now);
        } else {
          renderBuyerProfile(profileNode, store.buyerProfile(route.pubkey, now), expanded, toggleJob, now);
        }
        return;
      }
      const snap = store.snapshot(now);
      renderPulse(pulseNode, snap.pulse);
      if (activeTab === "feed") renderFeed(feedNode, snap.jobs, expanded, toggleJob, now);
      else renderStats(statsNode, snap);
    } catch (err) {
      console.error("render failed", err);
    }
  });
}

function toggleJob(id) {
  if (expanded.has(id)) expanded.delete(id);
  else expanded.add(id);
  scheduleRender();
}

function setTab(tab) {
  activeTab = tab;
  const feedSel = tab === "feed";
  tabFeed.setAttribute("aria-selected", String(feedSel));
  tabStats.setAttribute("aria-selected", String(!feedSel));
  if (route.view === "main") {
    feedNode.classList.toggle("hidden", !feedSel);
    statsNode.classList.toggle("hidden", feedSel);
  }
  scheduleRender();
}

tabFeed.addEventListener("click", () => setTab("feed"));
tabStats.addEventListener("click", () => setTab("stats"));
window.addEventListener("hashchange", applyRoute);

const client = createRelayClient({
  onEvent(raw) {
    const normalized = parseEvent(raw);
    const { newAuthor } = store.ingest(normalized);
    if (normalized?.role === "profile") {
      client.markProfileDone(normalized.pubkey);
    } else if (newAuthor) {
      client.requestProfiles([newAuthor]);
    }
    scheduleRender();
  },
  onStatus(s) {
    conn = s;
    scheduleRender();
  },
});

retryBtn.addEventListener("click", () => {
  client.disconnect();
  client.connect();
});

client.connect();
applyRoute();
