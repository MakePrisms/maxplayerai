# Community challenges v1 coordinator + local E2E

Run from the repository root (Node **24+**, Python 3, Cargo toolchain used by this checkout,
PostgreSQL 17 and Redis installed):

```sh
bash scripts/challenges/run-e2e.sh
```

The runner builds **this integration checkout** with wallet+ACP and Buzz in dedicated
`target/challenges-e2e` and `target/challenges-buzz` directories, installs the community
service's locked Node dependencies, and runs the assertions. On this Nix host it discovers
installed Postgres/Redis in `/nix/store` if absent from PATH. Else supply them on PATH;
`nix shell --extra-experimental-features 'nix-command flakes' nixpkgs#postgresql_17 nixpkgs#redis`
provides those dependencies. Use the repository devshell if the native build needs OpenSSL.
No Docker, global DB, production config, real LLM or mint is used.
Buzz’s Git object-store startup probe is disabled (`BUZZ_GIT_CONFORMANCE_PROBE=false`)
because this is an inline-delivery test with no Git/media object store. S3 points at closed
loopback port 1. This harness does not verify Git/S3 delivery or its conformance gate.

```sh
# Reuse binaries built above after no Rust changes:
bash scripts/challenges/run-e2e.sh --skip-build
# Existing kinds only; useful before challenge-core is integrated:
bash scripts/challenges/run-e2e.sh --skip-build --baseline
# Deterministically exercise valid duplicate claims before EOSE:
bash scripts/challenges/run-e2e.sh --skip-build --duplicate-history
node --test scripts/challenges/coordinator.test.mjs
```

The fixture uses a private `.scratch/ce-*` directory containing logs, homes and DBs (ignored
by Git). Each run has fresh random identities and ports. Keep logs for diagnosis; **do not
publish the directory or its key files**. SIGTERM/SIGKILL cleanup targets only recorded
process groups; services are stopped even on assertion failures. Runtime settings are built
from a restricted environment, not inherited `MAXPLAYER_*`, Buzz or app configuration.
The buyer and two sellers point only to the local Buzz WebSocket; sellers have both open
surfaces disabled and `accept_offers_only_from=[buyer]`, `takes_no_payment=true`, rate zero.
Mint lists are empty, real mints and fee auto-remit disabled. Only the local buyer is allowed
through the harness's `post_job` guard, with `payment=none`, amount/max zero and public
seller-targeting. The fixture uses a minimal ACP stub (adapted from the existing mint E2E),
not an installed model harness. External offer/delivery reviewer services are disabled in
these isolated homes; the runner reviews the exact stub answer before calling collect. This
proves the trade integrity/collect path, not a deployed reviewer service. It holds delivery behind a bounded local file gate until the runner has observed the
running HTTP/SSE state, then waits two seconds to make positive minute usage observable. No new money path is introduced.

Assertions: publish → 60/30-minute memberships → accept/deny → authenticated HTTP tally →
coordinator selects only accepted seller → real MCP post → seller claim / automatic buyer award →
running tally and no concurrent assignment → delivered result / free collect → exact minute
tally and done → denied seller idle → leave blocks new assignment while preserving work.
`--baseline` tests only the existing free-job trade loop and must not be cited as full challenge
coverage. Failed assertions exit nonzero and print the private artifact location. Successful
runs end in `PASS ALL community challenge E2E`.

The coordinator itself prints JSON and does not post: [buyer runbook](../../docs/challenge-coordinate.md).
It shares the app tally by consuming the snapshot, not by copying its event projection.
Paid assignment requires an explicit reconciled buyer budget input because the public app
cannot prove settlement. Serialise assignment and track pending offers as the runbook describes.

The runner never calls `award_claim`. Once a live claim is observed, the daemon must
commit its own award within a bounded 60-second polling window. The optional
`--duplicate-history` loopback proxy replays signed claim frames before EOSE to
exercise live/history overlap without relying on scheduler timing. See
[INTEGRATION.md](INTEGRATION.md) for the failing base log and root cause.

Known `lifecycle wire history incomplete` failures get at most three identical attempts
per `get_job` or `collect` invocation. Each retry is printed and captured. Polling
continues through this error only within the enclosing bounded wait; other errors
fail immediately. The daemon's independent retry loop, not these read retries,
is responsible for recovering an interrupted automatic award.

Membership coverage: join at any time with an exact work-start eligibility boundary; one join and acceptance across two published days; per-day minute reset; leave blocks assignments; late join waits until next day; changing minutes preserves since.
