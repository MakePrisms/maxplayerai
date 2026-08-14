/**
 * Keyed list reconciler — why the boards never flicker.
 *
 * The old site replaced each column's innerHTML wholesale on every tick, which
 * resets scroll, restarts animations, and repaints rows that did not change.
 * This reconciler diffs by key: a row whose content signature is unchanged is
 * not touched at all, a changed row is patched in place, and only genuinely
 * new/removed/reordered rows move. Scroll position survives because the
 * scroller's children are edited, never thrown away.
 */

export interface KeyedItem {
  key: string;
  /** Full inner HTML for the row. Also the change signature. */
  html: string;
  className: string;
  /** data-* attributes (values pre-escaped by the caller where needed). */
  data?: Record<string, string>;
  tabIndex?: number;
}

export function reconcileList(container: HTMLElement, items: KeyedItem[], tag = "li"): void {
  const existing = new Map<string, HTMLElement>();
  for (const child of Array.from(container.children)) {
    const key = (child as HTMLElement).dataset.key;
    if (key != null) existing.set(key, child as HTMLElement);
    else child.remove(); // skeletons and strays make way for real rows
  }

  let cursor: Element | null = container.firstElementChild;
  for (const item of items) {
    let node = existing.get(item.key);
    if (node) {
      existing.delete(item.key);
      // Patch only when the signature moved — untouched rows keep their DOM,
      // their animations, and their focus.
      if (node.dataset.sig !== item.html) {
        node.innerHTML = item.html;
        node.dataset.sig = item.html;
      }
      if (node.className !== item.className) node.className = item.className;
      syncData(node, item);
    } else {
      node = document.createElement(tag);
      node.dataset.key = item.key;
      node.dataset.sig = item.html;
      node.className = item.className;
      node.innerHTML = item.html;
      syncData(node, item);
    }
    // Keep document order aligned with items order with minimal moves.
    if (node !== cursor) container.insertBefore(node, cursor);
    else cursor = cursor.nextElementSibling;
    if (node === cursor) cursor = node.nextElementSibling;
  }

  for (const leftover of existing.values()) leftover.remove();
}

function syncData(node: HTMLElement, item: KeyedItem): void {
  if (item.tabIndex != null && node.tabIndex !== item.tabIndex) node.tabIndex = item.tabIndex;
  for (const [name, value] of Object.entries(item.data || {})) {
    if (node.dataset[name] !== value) node.dataset[name] = value;
  }
}

/**
 * Refresh only the relative-time labels inside a container. Runs on a slow
 * timer; it touches text nodes, never structure, so it can never disturb
 * scroll, focus, or animation.
 */
export function refreshAges(root: ParentNode, t: number, format: (ts: number, t: number) => string): void {
  for (const node of root.querySelectorAll<HTMLElement>("[data-ts]")) {
    const ts = Number(node.dataset.ts);
    if (!Number.isFinite(ts)) continue;
    const text = format(ts, t);
    if (node.textContent !== text) node.textContent = text;
  }
}
