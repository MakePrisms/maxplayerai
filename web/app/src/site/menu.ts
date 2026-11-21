/**
 * The phone-size menu of the pages in the new look that have no page script of their own for it (the legal pages,
 * /swarm). The same behaviour as /market (market.js setMenu): the button opens the tabs as a panel under the nav; a
 * link in the panel, a tap outside it, a scroll or Escape closes it.
 */
export function wireMenu(): (open: boolean) => void {
  const nav = document.querySelector<HTMLElement>(".nav");
  const menuBtn = document.getElementById("menu-btn");
  if (!nav || !menuBtn) return () => {};
  let menuY = 0;
  const setMenu = (open: boolean) => {
    if (open === nav.classList.contains("open")) return;
    nav.classList.toggle("open", open);
    menuBtn.setAttribute("aria-expanded", String(open));
    menuBtn.setAttribute("aria-label", open ? "Close menu" : "Menu");
    menuY = scrollY;
  };
  menuBtn.addEventListener("click", () => setMenu(!nav.classList.contains("open")));
  document.addEventListener("click", (e) => {
    const t = e.target as HTMLElement;
    if (!nav.classList.contains("open") || t.closest("#menu-btn")) return;
    if (!t.closest("#tabs") || t.closest("a")) setMenu(false);
  });
  window.addEventListener("scroll", () => { if (nav.classList.contains("open") && Math.abs(scrollY - menuY) > 24) setMenu(false); }, { passive: true });
  window.addEventListener("keydown", (e) => { if (e.key === "Escape") setMenu(false); });
  return setMenu;
}
