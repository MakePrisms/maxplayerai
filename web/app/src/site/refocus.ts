/**
 * Give focus back to what opened a panel, without the keyboard ring when the panel was used with a pointer
 * (Bob, 2026-11-21). Closing with Escape is a key press, so browsers then draw :focus-visible on the link focus goes
 * back to, which looked like a stray yellow line. When the last click was a mouse click or a tap, the link gets focus
 * quietly (no ring) until it loses focus; a keyboard user (Enter on the link) still sees the ring.
 */
// How the panel was opened decides it, not the last key: a stray key (Shift, a screenshot shortcut) between a click
// and Escape brought the ring back. A click from the mouse or a tap has detail >= 1; Enter or Space on a link or
// button fires a click with detail 0.
let pointer = false;
addEventListener("pointerdown", () => { pointer = true; }, true);
addEventListener("click", (e) => { pointer = e.detail > 0; }, true);

export function refocus(target: HTMLElement): void {
  if (pointer) {
    // Quiet until focus moves to something else on the page. Not on blur: switching windows blurs the link and gives
    // it focus back on return, and the browser then drew the ring (Bob, 2026-11-21).
    target.classList.add("quiet");
    const off = (e: FocusEvent) => {
      if (e.target === target) return;
      target.classList.remove("quiet");
      document.removeEventListener("focusin", off, true);
    };
    document.addEventListener("focusin", off, true);
  }
  target.focus({ preventScroll: true });
}
