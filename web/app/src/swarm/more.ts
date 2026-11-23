// The job section is capped at about one screen with a fade and a "See more" button (bob).
// The cap is a class JS adds, so without JS the whole job is readable. The button only shows
// when the capped text actually overflows; expanded, it reads "See less".
export interface More { update(): void; expanded(): boolean; reset(): void }

export function wireMore(body: HTMLElement, button: HTMLButtonElement, onCollapse: () => void = () => {}): More {
  let open = false;
  const paint = () => {
    button.setAttribute("aria-expanded", String(open));
    button.textContent = open ? "See less" : "See more";
  };
  const update = () => {
    if (open) { body.classList.remove("is-clamped"); button.hidden = false; paint(); return; }
    body.classList.add("is-clamped");
    const overflows = body.scrollHeight > body.clientHeight + 1;
    if (!overflows) body.classList.remove("is-clamped");
    button.hidden = !overflows;
    paint();
  };
  button.addEventListener("click", () => {
    open = !open;
    update();
    if (!open) onCollapse();
  });
  update();
  // a new job in the panel starts collapsed (review of #1361, r2 #4)
  return { update, expanded: () => open, reset: () => { open = false; update(); } };
}
