/**
 * The maxplayer version the website names: the latest STABLE release, never a release candidate.
 *
 * An RC bump leaves the website alone; a stable bump moves it. So the pages pin:
 *
 * - `Cargo.toml`'s version, when that version is stable (no `-`): the stable cut moves the pins;
 * - otherwise the newest stable `## vX.Y.Z` section in `RELEASE_NOTES.md`, which must be older
 *   than the version the RC builds toward (`0.8.2-rc1` builds toward `0.8.2`, so `0.8.1`).
 *
 * The release notes are the source because every release, stable or RC, adds its own section there
 * (`scripts/verify-release-version.sh` requires it), so this needs no extra constant to bump.
 *
 * Not a test file (no `.test.` in the name): the suites import it.
 */
import { readFileSync } from "node:fs";
import { join } from "node:path";

const VERSION = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?$/;

/** `[major, minor, patch]` of a version string, prerelease suffix ignored. */
function core(version) {
  const match = VERSION.exec(version);
  if (!match) throw new Error(`not a version: ${version}`);
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

function compareCore(a, b) {
  const [x, y] = [core(a), core(b)];
  for (let i = 0; i < 3; i += 1) if (x[i] !== y[i]) return x[i] - y[i];
  return 0;
}

export const isStable = (version) => VERSION.test(version) && !version.includes("-");

/** The workspace version declared in `Cargo.toml`'s text. */
export function cargoVersion(cargoToml) {
  const match = /^version\s*=\s*"(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)"/m.exec(cargoToml);
  if (!match) throw new Error("Cargo.toml declares no version");
  return match[1];
}

/** The newest stable version with a `## vX.Y.Z` section in the release notes, or null. */
export function latestStable(releaseNotes) {
  let best = null;
  for (const match of releaseNotes.matchAll(/^## v(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\s*$/gm)) {
    const version = match[1];
    if (isStable(version) && (best === null || compareCore(version, best) > 0)) best = version;
  }
  return best;
}

/** The version the website must name, for a tree at `cargo` with these release notes. */
export function expectedSiteVersion(cargo, releaseNotes) {
  if (isStable(cargo)) return cargo;
  const stable = latestStable(releaseNotes);
  if (stable === null) {
    throw new Error(`Cargo.toml is the prerelease ${cargo} and RELEASE_NOTES.md has no stable `
      + "section; the website names stable releases only");
  }
  if (compareCore(stable, cargo) >= 0) {
    throw new Error(`Cargo.toml is the prerelease ${cargo}, but RELEASE_NOTES.md already has the `
      + `stable section v${stable}; a prerelease must build toward an unreleased version`);
  }
  return stable;
}

/** The version the website must name, for the repository at `repo`. */
export function siteVersion(repo) {
  return expectedSiteVersion(
    cargoVersion(readFileSync(join(repo, "Cargo.toml"), "utf8")),
    readFileSync(join(repo, "RELEASE_NOTES.md"), "utf8"),
  );
}
