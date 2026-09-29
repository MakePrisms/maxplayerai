import { Event, RELAY_WS } from "./wire.js";
import { envelope, Transport } from "./controller.js";
export function retryDelay(value: string | null, time = Date.now()): number {
  const seconds = value && /^\d+$/.test(value) ? Number(value) : NaN;
  const delay = Number.isFinite(seconds)
    ? seconds * 1000
    : Date.parse(value ?? "") - time;
  return Math.max(
    1000,
    Math.min(Number.isFinite(delay) ? delay : 60000, 86400000),
  );
}
export class RateLimited extends Error {
  constructor(public retryAt: number) {
    super("Temporarily rate limited.");
  }
}
export class NotOpen extends Error {
  constructor() {
    super("Asking isn’t open yet.");
  }
}
export function transport(): Transport {
  return {
    read(filter) {
      return new Promise((resolve, reject) => {
        const socket = new WebSocket(RELAY_WS),
          id = crypto.randomUUID(),
          events: Event[] = [];
        const timer = setTimeout(() => {
          socket.close();
          reject(Error("Connection unavailable"));
        }, 8000);
        const done = () => {
          clearTimeout(timer);
          socket.close();
        };
        socket.onopen = () => socket.send(JSON.stringify(["REQ", id, filter]));
        socket.onerror = () => {
          done();
          reject(Error("Connection unavailable"));
        };
        socket.onmessage = ({ data }) => {
          if (typeof data !== "string" || data.length > 131072) return;
          try {
            const m = JSON.parse(data);
            if (m[1] !== id) return;
            if (m[0] === "EVENT" && events.length < 100) events.push(m[2]);
            if (m[0] === "EOSE") {
              done();
              resolve(events);
            }
          } catch {
            /* invalid frame */
          }
        };
      });
    },
    async publish(e, secret, evidence) {
      const response = await fetch("/api/try", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(envelope(e, secret, evidence)),
        signal: AbortSignal.timeout(10000),
      });
      if (response.status === 404) throw new NotOpen();
      if (response.status === 429)
        throw new RateLimited(
          Date.now() + retryDelay(response.headers.get("Retry-After")),
        );
      const receipt = await response.json();
      if (
        !response.ok ||
        receipt.accepted !== true ||
        receipt.event_id !== e.id
      )
        throw Error("Checking whether your question was sent…");
    },
  };
}
