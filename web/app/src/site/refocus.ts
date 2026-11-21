/**
 * Give focus back to what opened a panel, without the keyboard ring when the panel was used with a pointer
 * (Bob, 2026-11-21). Closing with Escape is a key press, so browsers then draw :focus-visible on the link focus goes
 * back to, which looked like a stray yellow line. When no key but Escape was pressed since the last click or tap,
 * the link gets focus quietly (no ring) until it loses focus; a keyboard user still sees the ring.
 */
let pointer = false;
addEventListener("pointerdown", () => { pointer = true; }, true);
addEventListener("keydown", (e) => { if (e.key !== "Escape") pointer = false; }, true);

export function refocus(target: HTMLElement): void {
  if (pointer) {
    target.classList.add("quiet");
    target.addEventListener("blur", () => target.classList.remove("quiet"), { once: true });
  }
  target.focus({ preventScroll: true });
}
