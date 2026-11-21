declare const WEB_ANALYTICS_ENABLED: boolean;
/**
 * The legal pages (/terms/<date>, /privacy/<date>, /fees/<date>), in the look of / and /market. All the script does
 * is the phone-size menu (as on /market) and page-view analytics on production builds. The text is static HTML.
 */
import { startAnalytics } from "./analytics.js";

const nav = document.querySelector<HTMLElement>(".nav");
const menuBtn = document.getElementById("menu-btn");
if (nav && menuBtn) {
  let menuY = 0;
  const setMenu = (open: boolean) => {
    if (open === nav.classList.contains("open")) return;
    nav.classList.toggle("open", open);
    menuBtn.setAttribute("aria-expanded", String(open));
    menuBtn.setAttribute("aria-label", open ? "Close menu" : "Menu");
    menuY = scrollY;
  };
  menuBtn.addEventListener("click", () => setMenu(!nav.classList.contains("open")));
  document.addEventListener("click", (e) => {      // a link in the panel, or anywhere outside it, closes it
    const t = e.target as HTMLElement;
    if (!nav.classList.contains("open") || t.closest("#menu-btn")) return;
    if (!t.closest("#tabs") || t.closest("a")) setMenu(false);
  });
  window.addEventListener("scroll", () => { if (nav.classList.contains("open") && Math.abs(scrollY - menuY) > 24) setMenu(false); }, { passive: true });
  window.addEventListener("keydown", (e) => { if (e.key === "Escape") setMenu(false); });
}

if (WEB_ANALYTICS_ENABLED) startAnalytics();
