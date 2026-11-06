#!/usr/bin/env node
/**
 * preview.mjs — a dependency-free static file server rooted at the PACKAGE directory, so a sample
 * page can link `/design-identity/tokens.css` exactly as a real app would link a token bundle.
 *
 * Binds 127.0.0.1 on an ephemeral port. Loopback only, no directory traversal above the root, no
 * upload path, no write path — that is the whole "least privilege" story for the preview leg.
 *
 * Used as a module (`startPreview()`), or standalone: `node tools/preview.mjs [--port N]`.
 */
import { createServer } from "node:http";
import { createReadStream } from "node:fs";
import { stat } from "node:fs/promises";
import { join, extname, normalize, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { dirname } from "node:path";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".webp": "image/webp",
  ".woff2": "font/woff2",
};

/** Resolve a URL path inside ROOT, or null if it escapes. */
function resolveInRoot(urlPath) {
  const decoded = decodeURIComponent(urlPath.split("?")[0]);
  const rel = normalize(decoded).replace(/^(\.\.[/\\])+/, "").replace(/^[/\\]+/, "");
  const abs = join(ROOT, rel);
  if (abs !== ROOT && !abs.startsWith(ROOT + sep)) return null;
  return abs;
}

export function startPreview({ port = 0 } = {}) {
  const server = createServer(async (req, res) => {
    if (req.method !== "GET" && req.method !== "HEAD") {
      res.writeHead(405, { allow: "GET, HEAD" });
      return res.end("method not allowed");
    }
    let abs = resolveInRoot(req.url || "/");
    if (!abs) {
      res.writeHead(403);
      return res.end("forbidden");
    }
    try {
      let info = await stat(abs);
      if (info.isDirectory()) {
        abs = join(abs, "index.html");
        info = await stat(abs);
      }
      res.writeHead(200, {
        "content-type": TYPES[extname(abs).toLowerCase()] || "application/octet-stream",
        "content-length": info.size,
        "cache-control": "no-store",
      });
      if (req.method === "HEAD") return res.end();
      createReadStream(abs).pipe(res);
    } catch {
      res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
      res.end("not found");
    }
  });

  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => {
      const actual = server.address().port;
      resolve({
        port: actual,
        origin: `http://127.0.0.1:${actual}`,
        url: (p) => `http://127.0.0.1:${actual}${p.startsWith("/") ? p : "/" + p}`,
        close: () => new Promise((r) => server.close(r)),
      });
    });
  });
}

if (process.argv[1] && process.argv[1].endsWith("preview.mjs")) {
  const i = process.argv.indexOf("--port");
  const port = i > -1 ? Number(process.argv[i + 1]) : 0;
  const s = await startPreview({ port });
  console.log(`preview serving ${ROOT}`);
  console.log(`origin ${s.origin}`);
  console.log(`before ${s.url("/samples/redesign/before/")}`);
  console.log(`after  ${s.url("/samples/redesign/after/")}`);
}
