import { HISTORY_LIMIT, RELAY_URL } from "../config.js";
import { MOBEE_TAG, MOBEE_TAGGED_KINDS, PROFILE, UNTAGGED_KINDS } from "./kinds.js";

/**
 * Single-owner NIP-01 websocket client.
 * Persistent market subscription + lazy kind-0 profile subscription.
 * Reconnect: capped exponential backoff, since-cursor resume (no replay flood).
 *
 * @param {{
 *   onEvent: (ev: unknown) => void,
 *   onStatus: (s: ConnectionStatus) => void,
 * }} hooks
 */
export function createRelayClient(hooks) {
  let ws = null;
  let intentionalClose = false;
  let attempt = 0;
  let retryTimer = null;
  let subSeq = 0;
  let marketSubId = "mobee-net-m-0";
  let profileSubId = "mobee-net-p-0";

  /** @type {number | null} max created_at seen — resume cursor */
  let sinceCursor = null;

  /** @type {Set<string>} authors we want kind-0 for */
  const profileAuthors = new Set();
  /** @type {Set<string>} authors already satisfied (cached upstream) */
  const profileDone = new Set();

  function status(state, detail = "") {
    hooks.onStatus({ state, detail, url: RELAY_URL, attempt });
  }

  function connect() {
    intentionalClose = false;
    clearRetry();
    status(attempt === 0 ? "connecting" : "reconnecting", `attempt ${attempt + 1}`);

    let socket;
    try {
      socket = new WebSocket(RELAY_URL);
    } catch (err) {
      status("disconnected", String(err?.message || err));
      scheduleRetry();
      return;
    }
    ws = socket;

    socket.onopen = () => {
      attempt = 0;
      status("connected");
      openMarketSub(socket);
      openProfileSub(socket);
    };

    socket.onmessage = (msg) => {
      let data;
      try {
        data = JSON.parse(msg.data);
      } catch {
        return;
      }
      if (!Array.isArray(data) || data.length < 1) return;
      const type = data[0];
      // NIP-42 AUTH: open-read still serves REQ — ignore AUTH.
      if (type === "AUTH") return;
      if (type === "EOSE") return; // keep subscription open for live stream
      if (type === "EVENT" && data[2]) {
        try {
          const ev = data[2];
          const created = typeof ev?.created_at === "number" ? ev.created_at : null;
          if (created != null) {
            sinceCursor = sinceCursor == null ? created : Math.max(sinceCursor, created);
          }
          hooks.onEvent(ev);
        } catch {
          // never let a bad handler tear down the socket loop
        }
      } else if (type === "NOTICE") {
        status("connected", String(data[1] || ""));
      } else if (type === "CLOSED" && (data[1] === marketSubId || data[1] === profileSubId)) {
        status("disconnected", String(data[2] || "CLOSED"));
        scheduleRetry();
      }
    };

    socket.onerror = () => {
      status("disconnected", "socket error");
    };

    socket.onclose = () => {
      ws = null;
      if (!intentionalClose) {
        status("disconnected", "socket closed");
        scheduleRetry();
      } else {
        status("disconnected", "closed");
      }
    };
  }

  function openMarketSub(socket) {
    subSeq += 1;
    marketSubId = `mobee-net-m-${subSeq}`;
    // Two filters in one REQ: the mobee-namespaced kinds are scoped to ["t","mobee"] so a
    // foreign event squatting a trade kind is filtered at the relay; the handler announce
    // carries no t-tag, so it rides an unscoped filter.
    /** @type {Record<string, unknown>} */
    const tagged = { kinds: [...MOBEE_TAGGED_KINDS], "#t": [MOBEE_TAG] };
    /** @type {Record<string, unknown>} */
    const untagged = { kinds: [...UNTAGGED_KINDS] };
    if (sinceCursor != null) {
      // Inclusive since — store dedupes by id; avoids gaps on same-second events.
      tagged.since = sinceCursor;
      untagged.since = sinceCursor;
    } else {
      tagged.limit = HISTORY_LIMIT;
      untagged.limit = HISTORY_LIMIT;
    }
    safeSend(socket, ["REQ", marketSubId, tagged, untagged]);
  }

  function openProfileSub(socket) {
    const authors = [...profileAuthors].filter((a) => !profileDone.has(a));
    if (!authors.length) return;
    subSeq += 1;
    profileSubId = `mobee-net-p-${subSeq}`;
    // Cap authors per REQ to keep filter sane; remainder wait for next flush.
    const batch = authors.slice(0, 100);
    safeSend(socket, ["REQ", profileSubId, { kinds: [PROFILE], authors: batch }]);
  }

  /**
   * Request kind-0 for authors not yet resolved. Deduped; no-op if already done/pending.
   * @param {string[]} pubkeys
   */
  function requestProfiles(pubkeys) {
    let added = false;
    for (const pk of pubkeys) {
      if (!pk || typeof pk !== "string") continue;
      if (profileDone.has(pk) || profileAuthors.has(pk)) continue;
      profileAuthors.add(pk);
      added = true;
    }
    if (!added || !ws || ws.readyState !== WebSocket.OPEN) return;
    // CLOSE prior profile sub then reopen with expanded author set.
    try {
      safeSend(ws, ["CLOSE", profileSubId]);
    } catch {
      /* ignore */
    }
    openProfileSub(ws);
  }

  /** Mark an author as resolved so we stop re-requesting. */
  function markProfileDone(pubkey) {
    if (!pubkey) return;
    profileDone.add(pubkey);
    profileAuthors.delete(pubkey);
  }

  function scheduleRetry() {
    clearRetry();
    attempt += 1;
    const delay = Math.min(30000, 1000 * 2 ** Math.min(attempt - 1, 4));
    status("reconnecting", `retry in ${Math.round(delay / 1000)}s`);
    retryTimer = setTimeout(connect, delay);
  }

  function clearRetry() {
    if (retryTimer) {
      clearTimeout(retryTimer);
      retryTimer = null;
    }
  }

  function disconnect() {
    intentionalClose = true;
    clearRetry();
    if (ws) {
      try {
        safeSend(ws, ["CLOSE", marketSubId]);
        safeSend(ws, ["CLOSE", profileSubId]);
      } catch {
        /* ignore */
      }
      try {
        ws.close();
      } catch {
        /* ignore */
      }
      ws = null;
    }
    status("disconnected", "manual");
  }

  return {
    connect,
    disconnect,
    requestProfiles,
    markProfileDone,
    getUrl: () => RELAY_URL,
    getSinceCursor: () => sinceCursor,
  };
}

function safeSend(ws, payload) {
  if (ws.readyState === WebSocket.OPEN) {
    ws.send(JSON.stringify(payload));
  }
}

/**
 * @typedef {{ state: 'connecting'|'connected'|'reconnecting'|'disconnected', detail: string, url: string, attempt: number }} ConnectionStatus
 */
