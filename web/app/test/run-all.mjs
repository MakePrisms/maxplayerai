/**
 * Test entry point for `node --test test/`.
 *
 * Node treats a positional directory as a single file spec rather than
 * recursing, so it resolves this directory to package.json "main". Importing
 * every suite here registers all of their cases under one run — a suite missing
 * from this list silently never runs, so add new ones here.
 */
import "./kinds.test.mjs";
import "./cache.test.mjs";
import "./model.test.mjs";
import "./trades.test.mjs";
import "./relay.test.mjs";
