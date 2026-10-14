/**
 * FAQ sections are centred in a full-height section, so opening an answer used
 * to re-centre the block and push the heading up (bob: "the title of the
 * section should not move"). Instead, measure where the centred layout puts the
 * block with every answer closed, then pin it there: the section switches to
 * top alignment with that offset as a margin, and answers only grow downward.
 * Without JS the section stays centred as before.
 */
export function pinFaqs(root: ParentNode = document): void {
  const sections = [...root.querySelectorAll<HTMLElement>(".h-faq")];
  if (!sections.length) return;
  const pin = () => {
    for (const section of sections) {
      const inner = section.querySelector<HTMLElement>(".h-in");
      if (!inner) continue;
      // Measure the closed, centred layout; reopen in the same task (no paint between).
      const open = [...section.querySelectorAll("details")].filter((d) => d.open);
      for (const d of open) d.open = false;
      section.classList.remove("faq-pinned");
      const lift = inner.getBoundingClientRect().top - section.getBoundingClientRect().top;
      section.style.setProperty("--faq-lift", `${Math.max(0, Math.round(lift))}px`);
      section.classList.add("faq-pinned");
      for (const d of open) d.open = true;
    }
  };
  pin();
  let frame = 0;
  addEventListener("resize", () => {
    cancelAnimationFrame(frame);
    frame = requestAnimationFrame(pin);
  });
  // Web fonts change line heights; re-measure once they are in.
  void document.fonts?.ready.then(pin);
}
