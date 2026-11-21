/**
 * Give focus back to what opened a panel, without the keyboard ring when the panel was used with a pointer
 * (Bob, 2026-11-21). Closing with Escape is a key press, so browsers then draw :focus-visible on the link focus goes
 * back to, which looked like a stray yellow line. When the panel was opened with a mouse click or a tap, the link
 * gets focus quietly (no ring); a keyboard user (Enter on the link, or Tab inside the panel) still sees the ring.
 *
 * How the panel was opened decides it, not the last key: a stray Shift or screenshot shortcut between a click and
 * Escape must not bring the ring back. A click from the mouse or a tap has detail >= 1; Enter or Space on a link or
 * button fires a click with detail 0.
 *
 * One element at most is quiet, and the listeners are installed once (review of #1361, r3 #1). It stops being quiet
 * when focus moves to anything else, on a click or tap anywhere else, or on Tab: Tab means the keyboard is in use,
 * and the element Tab lands on (possibly this one again, after a click on blank space) must show the ring. Switching
 * windows (blur, then focus again on return) and Alt-Tab keep it quiet.
 */
let pointer = false;
let quietEl: HTMLElement | null = null;
const clear = () => { quietEl?.classList.remove("quiet"); quietEl = null; };

addEventListener("pointerdown", (e) => {
  pointer = true;
  if (quietEl && !(e.target instanceof Node && quietEl.contains(e.target))) clear();
}, true);
addEventListener("click", (e) => { pointer = e.detail > 0; }, true);
addEventListener("keydown", (e) => {
  if (e.key === "Tab" && !e.altKey && !e.ctrlKey && !e.metaKey) { pointer = false; clear(); }
}, true);
addEventListener("focusin", (e) => { if (quietEl && e.target !== quietEl) clear(); }, true);

export function refocus(target: HTMLElement): void {
  clear();
  if (pointer) { quietEl = target; target.classList.add("quiet"); }
  target.focus({ preventScroll: true });
  if (document.activeElement !== target) clear();     // hidden or unfocusable: nothing to keep quiet (r4 #1)
}
