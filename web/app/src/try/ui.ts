import { HEARTBEAT } from "../model/kinds.js";
import { createRecord, openStore, RecordState } from "./store.js";
import { step } from "./controller.js";
import { NotOpen, RateLimited, transport } from "./transport.js";
import { identity, NEMO, now, one, promptText, questionOf, trade } from "./wire.js";
declare const TRY_IT_MARKET_LINK_ENABLED: boolean;
declare const TRY_IT_PREVIEW: boolean;
const preview = typeof TRY_IT_PREVIEW !== "undefined" && TRY_IT_PREVIEW;
export const statuses: Record<string, string> = {
  publishing: "Sending your question…",
  waiting: "Waiting for worker-nemo…",
  delayed: "Still waiting for an agent.",
  starting: "Starting…",
  working: "worker-nemo is working…",
  "accept-pending": "Your answer is here. Finishing up…",
  done: "Your answer",
  timeout: "This question timed out.",
  refused: "worker-nemo couldn’t answer this question.",
  invalid: "We couldn’t verify this answer.",
  files: "worker-nemo delivered files instead of a text answer. This page only shows text.",
  conflict: "Something went wrong. Try refreshing.",
};
// "You're <name> asking worker-nemo.", with worker-nemo linking to its
// market profile in a new tab (so this tab can keep driving the job).
function buyerLine(el: HTMLElement, name: string): void {
  const nemo = document.createElement("a");
  nemo.href = `/market?seller=${NEMO}`;
  nemo.target = "_blank";
  nemo.rel = "noopener";
  nemo.textContent = "worker-nemo";
  el.replaceChildren(`You're ${name} asking `, nemo, ".");
}
export function render(s: RecordState) {
  document.querySelector<HTMLElement>("#try-form")!.hidden = true;
  const question = document.querySelector<HTMLElement>("#try-question")!;
  question.textContent = questionOf(one(s.offer, "i")[1]!);
  question.hidden = false;
  const buyer = document.querySelector<HTMLElement>("#try-buyer")!;
  buyerLine(buyer, s.name);
  buyer.hidden = false;
  const status = document.querySelector<HTMLElement>("#try-status")!;
  status.textContent = statuses[s.phase] ?? "Checking your question…";
  status.hidden = s.phase === "done";
  const market = document.querySelector<HTMLAnchorElement>("#try-market")!;
  // Watch it live only while the job is still running: once it has ended
  // (answer, files, refusal, timeout or error) there is nothing left to watch.
  market.hidden =
    !!s.binding ||
    !s.offerAck ||
    !TRY_IT_MARKET_LINK_ENABLED ||
    ["done", "files", "timeout", "refused", "invalid", "conflict"].includes(s.phase);
  document.querySelector("#try-answer")!.textContent = s.binding?.answer ?? "";
  document.querySelector<HTMLElement>("#try-answer-panel")!.hidden = !s.binding;
  document.querySelector<HTMLElement>("#try-start")!.hidden =
    !s.binding && !["timeout", "refused", "delayed", "invalid", "files", "conflict"].includes(s.phase);
  const pitch = document.querySelector<HTMLElement>("#try-pitch")!;
  pitch.textContent = s.phase === "files"
    ? "That’s the other way agents work here: they deliver real work as git commits. Get started to receive them in your own agent."
    : "Agents on Maxplayer answer in text, like this, or deliver real work as git commits: code, docs, whole projects.";
  pitch.hidden = !s.binding && s.phase !== "files";
  document.querySelector<HTMLElement>("#try-check")!.hidden =
    !["timeout", "refused", "invalid", "conflict"].includes(s.phase);
  document.querySelector<HTMLElement>("#try-again")!.hidden =
    !preview || !["done", "timeout", "refused", "invalid", "files", "conflict"].includes(s.phase);
}
export async function bootTry() {
  const section = document.querySelector<HTMLElement>("#try");
  if (!section) return;
  section.hidden = false;
  document.body.classList.add("try-enabled");
  // The hero keeps "Get started"; a pill floating at the bottom of the screen
  // points down to the Try it section.
  const float = document.createElement("a");
  float.id = "try-float";
  float.className = "try-float";
  float.href = "#try";
  float.innerHTML =
    '<span>Try it first</span><svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true"><path d="M12 4v15m0 0-6-6m6 6 6-6" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
  document.body.append(float);
  // Gone the moment the top of the Try it section reaches the pill, and
  // stays gone for everything below it.
  let queued = false;
  const place = () => {
    queued = false;
    const reached =
      section.getBoundingClientRect().top <= float.getBoundingClientRect().bottom;
    float.classList.toggle("is-gone", reached);
  };
  const schedule = () => {
    if (!queued) (queued = true), requestAnimationFrame(place);
  };
  addEventListener("scroll", schedule, { passive: true });
  addEventListener("resize", schedule);
  place();
  float.onclick = (e) => {
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
  const status = document.querySelector<HTMLElement>("#try-status")!,
    submit = document.querySelector<HTMLButtonElement>("#try-submit")!,
    input = document.querySelector<HTMLTextAreaElement>("#try-prompt")!;
  if (!navigator.locks || !globalThis.BroadcastChannel) {
    status.textContent = "Please try another browser.";
    submit.disabled = true;
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
  const visitor = identity();
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
        throw Error("Please try another browser.");
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
      if (e instanceof NotOpen) {
        // The server is switched off: say so once instead of retrying.
        status.textContent = "Asking isn’t open yet. Check back soon.";
        status.hidden = false;
        failures = 6;
        return;
      }
      status.textContent =
        failures >= 5 ? "Can’t connect. Try refreshing." : "Connection interrupted. We’ll try again.";
      status.hidden = false;
      failures++;
      document.querySelector<HTMLElement>("#try-check")!.hidden = failures < 6;
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
            ? `Taking a breather. Retrying in ${remaining}s…`
            : "Trying again…";
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
        !["done", "refused", "timeout", "invalid", "files", "conflict"].includes(
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
    const count = [...input.value.trim()].length;
    const counter = document.querySelector<HTMLElement>("#try-count")!;
    counter.textContent = `${count} / 1,000`;
    counter.hidden = count < 900;
  };
  document.querySelector("#try-form")!.addEventListener("submit", async (e) => {
    e.preventDefault();
    try {
      try {
        promptText(input.value);
      } catch {
        status.textContent = "Ask a question in 1–1,000 characters.";
        status.hidden = false;
        return;
      }
      if (!online) return;
      submit.disabled = true;
      await db.reserve(createRecord(input.value, now(), visitor));
      await show();
      channel.postMessage("updated");
      void run();
    } catch (e) {
      status.textContent =
        "Enable browser storage to try it";
      submit.disabled = !online;
    }
  });
  document.querySelector("#try-again")!.addEventListener("click", async () => {
    await db.clear();
    channel.postMessage("updated");
    location.reload();
  });
  document.querySelector("#try-check")!.addEventListener("click", () => {
    failures = 0;
    void run();
  });
  const copy = document.querySelector<HTMLButtonElement>("#try-copy-answer")!;
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(document.querySelector("#try-answer")!.textContent ?? "");
      copy.textContent = "Copied";
    } catch {
      copy.textContent = "Select to copy";
    }
    setTimeout(() => { copy.textContent = "Copy answer"; }, 1600);
  });
  const saved = await db.read();
  if (saved) {
    render(saved);
    void run();
  } else {
    const buyer = document.querySelector<HTMLElement>("#try-buyer")!;
    buyerLine(buyer, visitor.name);
    buyer.hidden = false;
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
          // Sellers republish every 5 minutes, so allow two beats plus a minute.
          now() - fresh.created_at <= 660 &&
          one(fresh, "accepting")[1] === "y";
        status.textContent = online ? "" : "The agent is offline. Check back soon.";
        status.hidden = online;
      } catch {
        if (await db.read()) return;
        online = false;
        status.textContent = "Can’t connect right now. We’ll try again.";
        status.hidden = false;
      }
      if (!(await db.read())) {
        submit.disabled = !online;
        setTimeout(heartbeat, 30000);
      }
    };
    void heartbeat();
  }
}
