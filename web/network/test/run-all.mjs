/**
 * Test entry point for `node --test test/`.
 *
 * Node 24's runner treats a positional directory as a single file spec (it does not
 * recurse), so `node --test test/` resolves this directory to its package.json "main".
 * Importing every suite here registers all of their `node:test` cases under one run.
 * (The `npm test` glob and bare `node --test` autodiscovery run the files directly.)
 */
import "./parse.test.mjs";
import "./jobs.test.mjs";
import "./kinds.test.mjs";
import "./profiles.test.mjs";
