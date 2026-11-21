/**
 * Resolves once every stylesheet the page links has loaded (or failed to).
 *
 * The page scripts of / and /market measure the layout as soon as they start: how many rows fit a list, where the
 * phone sits, what has to be hidden until it is placed. A classic script waits for the stylesheets before it runs.
 * A module script does too in Chromium, but NOT in WebKit: there the bundle ran against an unstyled page (measured
 * 2026-11-20: no stylesheet applied, the nav still `position: static`), every element it created was first styled
 * with the defaults, and the stylesheet's arrival then played as a transition (the homepage's 24 agent marks showed
 * sharp at full strength and faded for 2.6 s on every load in Safari). So the entries wait for this first.
 */
export function stylesLoaded(): Promise<void> {
  const pending = [...document.querySelectorAll<HTMLLinkElement>('link[rel="stylesheet"]')].filter((link) => !link.sheet);
  return Promise.all(pending.map((link) => new Promise<void>((resolve) => {
    link.addEventListener("load", () => resolve(), { once: true });
    link.addEventListener("error", () => resolve(), { once: true });
  }))).then(() => undefined);
}
