import { HEARTBEAT } from "../model/kinds.js";
import { createRecord, openStore, RecordState } from "./store.js";
import { step } from "./controller.js";
import { RateLimited, transport } from "./transport.js";
import { NEMO, now, one, promptText, trade } from "./wire.js";
declare const TRY_IT_MARKET_LINK_ENABLED: boolean;
export const statuses: Record<string, string> = {
  publishing: "Sending your question…",
  waiting: "Waiting for worker-nemo…",
  delayed: "worker-nemo hasn’t picked this up yet.",
  starting: "Starting…",
  working: "worker-nemo is working…",
  "accept-pending": "Answer received; finishing on the market…",
  done: "Your answer",
  timeout: "This question timed out.",
  refused: "worker-nemo couldn’t answer this question.",
  invalid: "We couldn’t verify this answer.",
  conflict: "Conflicting job history. Check status to reconcile.",
};
export function render(s: RecordState) {
  const q = document.querySelector<HTMLTextAreaElement>("#try-prompt")!;
  q.value = one(s.offer, "i")[1]!;
  q.disabled = true;
  document.querySelector("#try-count")!.textContent =
    `${[...q.value].length} / 1,000`;
  document.querySelector<HTMLButtonElement>("#try-submit")!.disabled = true;
  document.querySelector("#try-status")!.textContent =
    statuses[s.phase] ?? s.phase;
  const market = document.querySelector<HTMLAnchorElement>("#try-market")!;
  market.hidden = !s.offerAck || !TRY_IT_MARKET_LINK_ENABLED;
  document.querySelector("#try-job")!.textContent = s.offerAck
    ? s.offer.id.slice(0, 12)
    : "";
  document.querySelector<HTMLElement>("#try-job-panel")!.hidden = !s.offerAck;
  const answer = document.querySelector<HTMLElement>("#try-answer")!;
  answer.textContent = s.binding?.answer ?? "";
  answer.hidden = !s.binding;
  document.querySelector<HTMLElement>("#try-copy-answer")!.hidden = !s.binding;
  document.querySelector<HTMLElement>("#try-start")!.hidden =
    !s.binding &&
    !["timeout", "refused", "delayed", "invalid", "conflict"].includes(s.phase);
  document.querySelector<HTMLElement>("#try-check")!.hidden = false;
}
export async function bootTry() {
  const section = document.querySelector<HTMLElement>("#try");
  if (!section) return;
  section.hidden = false;
  const hero = document.querySelector<HTMLAnchorElement>("#hero-cta")!;
  hero.href = "#try";
  hero.textContent = "Try it";
  hero.onclick = (e) => {
    e.preventDefault();
    history.pushState(null, "", "#try");
    section.scrollIntoView({
      behavior: matchMedia("(prefers-reduced-motion: reduce)").matches
        ? "auto"
        : "smooth",
    });
    document
      .querySelector<HTMLElement>("#try-h")!
      .focus({ preventScroll: true });
  };
  const status = document.querySelector("#try-status")!,
    submit = document.querySelector<HTMLButtonElement>("#try-submit")!,
    input = document.querySelector<HTMLTextAreaElement>("#try-prompt")!;
  if (!navigator.locks || !globalThis.BroadcastChannel) {
    status.textContent = "This browser cannot safely coordinate tabs.";
    return;
  }
  let store;
  try {
    store = await openStore();
    await store.read();
  } catch {
    status.textContent = "Enable browser storage to try it";
    submit.disabled = true;
    return;
  }
  const db = store,
    t = transport(),
    channel = new BroadcastChannel("maxplayer-try");
  let busy = false,
    online = false,
    failures = 0,
    timer: ReturnType<typeof setTimeout>;
  const show = async () => {
    const s = await db.read();
    if (s) render(s);
  };
  channel.onmessage = () => {
    void show();
    if (!busy) {
      clearTimeout(timer);
      timer = setTimeout(run, 3000);
    }
  };
  const run = async () => {
    if (busy) return;
    busy = true;
    clearTimeout(timer);
    let rateDelay = 0;
    try {
      if (!navigator.locks)
        throw Error("This browser cannot safely coordinate tabs.");
      await navigator.locks.request(
        "maxplayer-try-controller",
        { ifAvailable: true },
        async (lock) => {
          if (!lock) return;
          const s = await step(db, t, now(), NEMO, render);
          if (s) render(s);
          channel.postMessage("updated");
        },
      );
      failures = 0;
    } catch (e) {
      await show();
      status.textContent =
        e instanceof Error ? e.message : "Connection unavailable";
      failures++;
      if (e instanceof RateLimited) {
        rateDelay = Math.max(1000, e.retryAt - Date.now());
        const button = document.querySelector<HTMLButtonElement>("#try-check")!;
        button.disabled = true;
        const tick = () => {
          const remaining = Math.max(
            0,
            Math.ceil((e.retryAt - Date.now()) / 1000),
          );
          status.textContent = remaining
            ? `Temporarily rate limited. Check status in ${remaining} seconds.`
            : "You can check status now.";
          if (!remaining) {
            clearInterval(countdown);
            button.disabled = false;
          }
        };
        const countdown = setInterval(tick, 1000);
        tick();
      }
    } finally {
      busy = false;
      const s = await db.read();
      if (
        s &&
        !["done", "refused", "timeout", "invalid", "conflict"].includes(
          s.phase,
        ) &&
        failures < 6
      )
        timer = setTimeout(
          run,
          rateDelay ||
            ([3000, 1000, 2000, 5000, 10000, 30000][failures] ?? 30000),
        );
    }
  };
  input.oninput = () => {
    document.querySelector("#try-count")!.textContent =
      `${[...input.value.trim()].length} / 1,000`;
  };
  document.querySelector("#try-form")!.addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      promptText(input.value);
      if (!online) return;
      submit.disabled = true;
      await db.reserve(createRecord(input.value, now()));
      await show();
      channel.postMessage("updated");
      void run();
    } catch (e) {
      status.textContent =
        e instanceof Error ? e.message : "Enable browser storage to try it";
      submit.disabled = !online;
    }
  });
  document.querySelector("#try-check")!.addEventListener("click", () => {
    failures = 0;
    void run();
  });
  document
    .querySelector("#try-copy-answer")!
    .addEventListener(
      "click",
      () =>
        void navigator.clipboard.writeText(
          document.querySelector("#try-answer")!.textContent ?? "",
        ),
    );
  const saved = await db.read();
  if (saved) {
    render(saved);
    void run();
  } else {
    const heartbeat = async () => {
      try {
        const events = await t.read({
          authors: [NEMO],
          kinds: [HEARTBEAT],
          limit: 5,
        });
        if (await db.read()) return;
        const fresh = events
          .filter((e) => {
            try {
              trade(e);
              return (
                e.pubkey === NEMO &&
                e.kind === HEARTBEAT &&
                one(e, "d")[1] === "maxplayer-seller" &&
                e.created_at <= now() + 60
              );
            } catch {
              return false;
            }
          })
          .sort((a, b) => b.created_at - a.created_at)[0];
        online =
          !!fresh &&
          now() - fresh.created_at <= 90 &&
          one(fresh, "accepting")[1] === "y";
        status.textContent = online
          ? "Ready for your question."
          : "worker-nemo may be offline.";
      } catch {
        if (await db.read()) return;
        online = false;
        status.textContent = "Could not check worker-nemo’s availability.";
      }
      if (!(await db.read())) {
        submit.disabled = !online;
        setTimeout(heartbeat, 30000);
      }
    };
    void heartbeat();
  }
}
