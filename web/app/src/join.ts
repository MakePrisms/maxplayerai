/**
 * The team invite page (public/join-invite.html, served for /join/<token> in a browser). All the script does is put
 * this page's link into the line for the agent and the join step, and the Copy button. It sends nothing anywhere: no
 * analytics (the token is in the path) and no request that could use the invite; only `maxplayer team join` does.
 */
import { agentLine, inviteLink } from "./site/join-link.js";
import { writeClipboard } from "./ui/clipboard.js";

const link = inviteLink(location.origin, location.pathname);
const line = document.getElementById("join-line");
const button = document.getElementById("join-copy") as HTMLButtonElement | null;

if (link && line && button) {
  const text = agentLine(link);
  line.textContent = text;
  for (const el of document.querySelectorAll("[data-join-link]")) el.textContent = link;
  button.hidden = false;
  let timer = 0;
  button.addEventListener("click", async () => {
    // Bob (2026-11-23): clicking Copy moves no text. The button keeps one width (join.css) and only ever says Copy
    // or Copied; if the browser refuses to copy, the line is selected instead, ready for the person to copy.
    if (!(await writeClipboard(text))) {
      const range = document.createRange();
      range.selectNodeContents(line);
      getSelection()?.removeAllRanges();
      getSelection()?.addRange(range);
      return;
    }
    button.textContent = "Copied";
    button.classList.add("ok");
    clearTimeout(timer);
    timer = window.setTimeout(() => { button.textContent = "Copy"; button.classList.remove("ok"); }, 1600);
  });
}
