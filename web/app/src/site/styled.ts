/**
 * Resolves once every stylesheet the page links has loaded (or failed to), or after `maxMs` at the latest.
 *
 * The page scripts of / and /market measure the layout as soon as they start: how many rows fit a list, where the
 * phone sits, what has to be hidden until it is placed. A classic script waits for the stylesheets before it runs.
 * A module script does too in Chromium, but NOT in WebKit: there the bundle ran against an unstyled page (measured
 * 2026-11-20: no stylesheet applied, the nav still `position: static`), every element it created was first styled
 * with the defaults, and the stylesheet's arrival then played as a transition (the homepage's 24 agent marks showed
 * sharp at full strength and faded for 2.6 s on every load in Safari). So the entries wait for this first.
 *
 * The cap matters: a stylesheet that has already FAILED has no `sheet` and will never fire `load` or `error` again
 * (its `error` fired before this ran), so without it the promise never resolved, the page script never ran, and the
 * page stayed blank (its nav and content are hidden until the script marks it ready). Late beats never.
 */
export function stylesLoaded(maxMs = 1500): Promise<void> {
  const pending = [...document.querySelectorAll<HTMLLinkElement>('link[rel="stylesheet"]')].filter((link) => !link.sheet);
  if (!pending.length) return Promise.resolve();
  const all = Promise.all(pending.map((link) => new Promise<void>((resolve) => {
    link.addEventListener("load", () => resolve(), { once: true });
    link.addEventListener("error", () => resolve(), { once: true });
  }))).then(() => undefined);
  return Promise.race([all, new Promise<void>((resolve) => setTimeout(resolve, maxMs))]);
}
