/**
 * Clipboard write with a legacy fallback; shared by /swarm and /tokens (/market has the same in market.js).
 * The fallback copies a selection in an unseen span (.copysrc in market.css and styles.css), as market.js does: no
 * text box, so focus does not move (and no focus ring is drawn on the button afterwards), and no style text is
 * written from script under the page's CSP (review of #1361, r2 #5 / r3).
 */
function copyLegacy(text: string): boolean {
  const span = document.createElement("span"), sel = getSelection(), range = document.createRange();
  span.className = "copysrc";
  span.textContent = text;
  document.body.appendChild(span);
  range.selectNodeContents(span);
  sel?.removeAllRanges(); sel?.addRange(range);
  let ok = false;
  try { ok = document.execCommand("copy"); } catch { ok = false; }
  sel?.removeAllRanges(); span.remove();
  return ok;
}

export async function writeClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
    return copyLegacy(text);
  } catch {
    return copyLegacy(text);
  }
}
