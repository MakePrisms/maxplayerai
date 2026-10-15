import { challenge } from "./challenge/content.js";
import { buyerHex, joinState, validateChallenge, whitelistInstruction } from "./challenge/model.js";
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

if (challenge) {
  try {
    const c = validateChallenge(challenge);
    text("challenge-title", c.title);
    text("challenge-goal", c.goal);
    text("challenge-update", c.update);
    text("challenge-status", {
      scheduled: "Scheduled", in_progress: "In progress", completed: "Completed", cancelled: "Cancelled",
    }[c.status]);
    text("challenge-budget", `${new Intl.NumberFormat().format(c.budgetSats)} sats`);
    showTime("challenge-start", c.startsAt);
    showTime("join-open", c.joinOpensAt);
    showTime("join-close", new Date(Date.parse(c.joinOpensAt) + 3_600_000).toISOString());
    text("buyer-npub", c.buyerNpub);
    text("buyer-hex", buyerHex(c.buyerNpub));
    text("buyer-instruction", whitelistInstruction(c));
    document.querySelectorAll<HTMLButtonElement>("[data-challenge-copy]").forEach((button) => {
      button.disabled = false;
      button.addEventListener("click", async () => {
        const value = document.getElementById(button.dataset.challengeCopy!)!.textContent!;
        const copied = await writeClipboard(value);
        text("copy-feedback", copied ? "Copied." : "Could not copy. Select the text and copy it manually.");
      });
    });
    const updateClock = () => text("join-status", joinState(c, Date.now()));
    updateClock();
    setInterval(updateClock, 1000);
  } catch {
    text("challenge-status", "Unavailable");
    text("challenge-goal", "Challenge details could not be loaded. Please check back later.");
  }
}
