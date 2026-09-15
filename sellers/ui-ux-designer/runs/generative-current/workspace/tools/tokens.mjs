import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const PKG = resolve(dirname(fileURLToPath(import.meta.url)), "..");
export const TOKENS = JSON.parse(
  await readFile(resolve(PKG, "design-identity/tokens.json"), "utf8"),
);
