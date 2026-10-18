import assert from "node:assert/strict";
import test from "node:test";
import { wireMore } from "../src/challenge/more.js";

// DOM double: the body's visible height is capped only while .is-clamped is set (as in CSS).
function fixture(contentHeight: number, cap = 500) {
  const classes = new Set<string>();
  const body = {
    contentHeight,
    classList: { add: (c: string) => classes.add(c), remove: (c: string) => classes.delete(c), contains: (c: string) => classes.has(c) },
    get scrollHeight() { return this.contentHeight; },
    get clientHeight() { return classes.has("is-clamped") ? Math.min(this.contentHeight, cap) : this.contentHeight; },
  };
  let onClick = () => {};
  const attrs: Record<string, string> = {};
  const button = {
    hidden: true, textContent: "",
    setAttribute: (k: string, v: string) => { attrs[k] = v; },
    getAttribute: (k: string) => attrs[k] ?? null,
    addEventListener: (_: string, f: () => void) => { onClick = f; },
    click: () => onClick(),
  };
  return { body, button, classes, attrs };
}

test("short jobs get no button and no clamp", () => {
  const { body, button, classes } = fixture(300);
  wireMore(body as unknown as HTMLElement, button as unknown as HTMLButtonElement);
  assert.equal(button.hidden, true);
  assert.equal(classes.has("is-clamped"), false, "nothing to fade");
});

test("a job taller than one screen is clamped with a See more button that expands and collapses", () => {
  const { body, button, classes, attrs } = fixture(1400);
  let collapsed = 0;
  wireMore(body as unknown as HTMLElement, button as unknown as HTMLButtonElement, () => collapsed++);
  assert.equal(button.hidden, false);
  assert.equal(classes.has("is-clamped"), true);
  assert.equal(attrs["aria-expanded"], "false");
  assert.equal(button.textContent, "See more");

  button.click();
  assert.equal(classes.has("is-clamped"), false, "expanded shows everything");
  assert.equal(attrs["aria-expanded"], "true");
  assert.equal(button.textContent, "See less");
  assert.equal(button.hidden, false);

  button.click();
  assert.equal(classes.has("is-clamped"), true);
  assert.equal(attrs["aria-expanded"], "false");
  assert.equal(button.textContent, "See more");
  assert.equal(collapsed, 1, "collapsing hands back so the page can scroll to the section");
});

test("update() re-measures when the job text arrives or the window resizes", () => {
  const { body, button, classes } = fixture(200);
  const more = wireMore(body as unknown as HTMLElement, button as unknown as HTMLButtonElement);
  assert.equal(button.hidden, true);
  body.contentHeight = 2000; // the signed job replaced the placeholder
  more.update();
  assert.equal(button.hidden, false);
  assert.equal(classes.has("is-clamped"), true);
  body.contentHeight = 400; // e.g. a wider window
  more.update();
  assert.equal(button.hidden, true);
  assert.equal(classes.has("is-clamped"), false);
});
