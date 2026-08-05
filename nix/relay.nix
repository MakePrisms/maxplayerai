# Launch relay: `services.maxplayer.relay` — the buzz-derived relay (crate `buzz-relay`) that backs
# relay.maxplayer.ai. One binary serving events + git + payments over a closed, compiled kind
# allowlist: the mobee namespace IS the allowlist, so there is no write-policy plugin (the strfry
# plugin retires with the strfry relay it fronted). Born empty.
#
# TLS lives in the host (nix/relay-host.nix): nginx terminates and proxies wss -> 127.0.0.1:3000, so
# this module binds loopback only and never touches ACME. Postgres + Redis are provisioned here; the
# relay's stable identity key is supplied by the host as a file PATH (never in the repo).
#
# The package is wired from the flake as an option (`services.maxplayer.relay.package`): `.#relay`
# injects the in-tree vendored `buzz-relay` crate. Git-CAS + media objects live in S3 (the box's
# instance IAM role authorizes the bucket), so the box itself keeps only Postgres, Redis, the relay
# identity key, and a rehydratable local working cache.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.maxplayer.relay;

  # Environment the buzz-relay binary reads. Loopback bind (nginx fronts). Open membership: a public
  # marketplace where any NIP-42-authed key may write — buzz still MANDATES a signed NIP-42 handshake
  # on every write, so "open" means "not membership-gated", never "anonymous". The compiled kind
  # allowlist is what scopes the namespace to mobee.
  # The S3 endpoint the git-CAS + media client talks to. Region::Custom uses it verbatim
  # (store.rs GitStore::new), so default to the AWS regional endpoint derived from s3Region; an
  # explicit s3Endpoint overrides it (an S3-compatible store on another host).
  s3Endpoint =
    if cfg.s3Endpoint != null then cfg.s3Endpoint else "https://s3.${cfg.s3Region}.amazonaws.com";

  baseEnv = {
    BUZZ_BIND_ADDR = cfg.bindAddr;
    DATABASE_URL = "postgresql:///${cfg.database}?host=/run/postgresql&user=${cfg.user}";

    # Schema: buzz applies its OWN, via the embedded sqlx migrator (sqlx::migrate!(migrations/), 24
    # files 0001_initial_schema..0024) when this is enabled — main.rs gates db.migrate() on it. The
    # migration set is the COMPLETE source of truth (git_* is 0002, the replica fence floor is 0021);
    # schema/schema.sql is only the initial snapshot and is deliberately NOT applied. "1" = on.
    BUZZ_AUTO_MIGRATE = "1";
    REDIS_URL = "redis://127.0.0.1:6379";
    RELAY_URL = cfg.relayUrl;
    BUZZ_HEALTH_PORT = toString cfg.healthPort;
    BUZZ_METRICS_PORT = toString cfg.metricsPort;
    BUZZ_GIT_REPO_PATH = "${cfg.dataDir}/repos";

    # Git-on-object-storage: buzz-relay stores the git-CAS (delivered-repo packs + manifests) AND
    # media in S3, and runs a FATAL linearizable conditional-write conformance probe (the A3 gate,
    # main.rs) against it AT BOOT, before it binds. The git store borrows this same media S3 config
    # (state.rs GitStore::new), so one bucket backs both. Real AWS S3 via the instance IAM role:
    # access/secret are the EMPTY STRING on purpose — an UNSET key defaults to buzz's "buzz_dev" dev
    # value (config.rs), which takes the static-credential branch and 403s against real S3; EMPTY
    # selects the AWS credential chain → the IMDS instance role (store.rs GitStore::new). No
    # credential ever enters the repo or the world-readable nix store.
    BUZZ_S3_ENDPOINT = s3Endpoint;
    BUZZ_S3_BUCKET = cfg.s3Bucket;
    BUZZ_S3_REGION = cfg.s3Region;
    BUZZ_S3_ACCESS_KEY = "";
    BUZZ_S3_SECRET_KEY = "";

    BUZZ_REQUIRE_AUTH_TOKEN = lib.boolToString cfg.requireAuthToken;
    BUZZ_REQUIRE_RELAY_MEMBERSHIP = lib.boolToString cfg.requireRelayMembership;
    BUZZ_OPEN_READ = lib.boolToString cfg.openRead;
  }
  // lib.optionalAttrs (cfg.ownerPubkey != null) { RELAY_OWNER_PUBKEY = cfg.ownerPubkey; };
in
{
  options.services.maxplayer.relay = {
    enable = lib.mkEnableOption "the maxplayer launch relay (buzz-derived)";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        The buzz-relay package to run. Wired from the flake (`packages.maxplayer-relay`, the in-tree
        vendored crate). `lib.getExe` resolves its `meta.mainProgram` (buzz-relay).
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/buzz";
      description = ''
        Local state directory: the relay's working files + `<dataDir>/repos` git working cache. This
        is scratch, rehydratable from S3 — the durable git-CAS + media objects live in s3Bucket, not here.
      '';
    };

    bindAddr = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:3000";
      description = "Loopback bind. The host's nginx terminates TLS and proxies wss here.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 3000;
      description = "App port (matches bindAddr); the host's nginx proxyPass target.";
    };

    healthPort = lib.mkOption {
      type = lib.types.port;
      default = 8080;
    };

    metricsPort = lib.mkOption {
      type = lib.types.port;
      default = 9102;
    };

    relayUrl = lib.mkOption {
      type = lib.types.str;
      description = "Public wss URL the relay advertises (RELAY_URL / NIP-11), e.g. wss://relay.maxplayer.ai.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "buzz";
      description = "Service user + Postgres role (peer-authed). Kept as `buzz` to match the vendored code's defaults.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "buzz";
    };

    database = lib.mkOption {
      type = lib.types.str;
      default = "buzz";
    };

    requireRelayMembership = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        OFF for a public marketplace: any NIP-42-authed key may write. ON gates writes to an admitted
        member set — wrong for open buyer/seller participation, and it would make the deploy probe
        (a throwaway key writing a kind-3400) fail with "not a member".
      '';
    };

    requireAuthToken = lib.mkOption {
      type = lib.types.bool;
      default = false;
    };

    openRead = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Allow UNAUTHENTICATED (anonymous) relay reads. OFF by default — buzz's own default is that
        reads require NIP-42 auth, so a keyless client (e.g. a web observatory) returns EMPTY. ON for a
        public marketplace whose events are meant to be readable by anyone without an account. Wires
        BUZZ_OPEN_READ (config.rs treats only "true"/"1" as on). READ-only: writes stay gated by
        requireRelayMembership / NIP-42; this opens the read path only.
      '';
    };

    ownerPubkey = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "64-hex relay owner pubkey, bootstrapped as owner on first start. Optional for an open relay.";
    };

    privateKeyFile = lib.mkOption {
      type = lib.types.path;
      description = ''
        Path to an EnvironmentFile containing `BUZZ_RELAY_PRIVATE_KEY=<hex>` — the relay's stable
        identity. Provisioned on the box by gudnuf, never in the repo. Required (no default): a relay
        with no stable key churns its NIP-42 + NIP-11 pubkey on every restart, and the preflight below
        refuses to start without it rather than boot a churning identity that looks healthy.
      '';
    };

    s3Bucket = lib.mkOption {
      type = lib.types.str;
      default = "maxplayer-relay-media";
      description = ''
        S3 bucket holding the git-CAS (delivered-repo packs + manifests) + media objects. buzz-relay
        reads/writes/lists/deletes here (storage_sweep prunes, hence DeleteObject), so the box's
        instance IAM role must grant Get/Put/List/DeleteObject on it. Durability is S3-native
        (versioned, off-box), so there is deliberately no on-box object backup.
      '';
    };

    s3Region = lib.mkOption {
      type = lib.types.str;
      default = "us-east-1";
      description = ''
        AWS region of s3Bucket. Drives the default endpoint AND the SigV4 signing region, so it must
        match the bucket's real region or every request is signed for the wrong one.
      '';
    };

    s3Endpoint = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Override the S3 endpoint URL. null → the AWS regional endpoint https://s3.<s3Region>.amazonaws.com.
        Set only for an S3-compatible backend on another host. buzz uses path-style addressing, which
        real S3 also accepts (store.rs GitStore::new).
      '';
    };

    backup = {
      destination = lib.mkOption {
        type = lib.types.str;
        description = "S3 prefix for off-box backups, e.g. s3://maxplayer-relay-backup/launch.";
      };
      uploadCommand = lib.mkOption {
        type = lib.types.str;
        description = ''
          Command that ships one file to S3. Invoked once per artifact with `$FILE` (local path),
          `$KEY` (destination suffix) and `$DESTINATION` in the environment. Uses the instance IAM
          role — no static credentials on the box.
        '';
      };
      alertCommand = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = ''
          Command the OnFailure alert unit runs when a backup RUN FAILS, so a failed backup PAGES
          instead of failing silently (a silently-failing backup is a durability-layer lie). Runs with
          `$DESTINATION` in the environment. The default ("") still emits a loud emerg-priority journal
          line on every failure; set this to a real out-of-band pager — e.g. an `aws sns publish` to an
          ops topic, or a webhook curl — via the instance role (its transport IAM, like sns:Publish, is
          granted on the box; no static creds).
        '';
      };
      schedule = lib.mkOption {
        type = lib.types.str;
        default = "daily";
      };
      environmentFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Optional secrets for the backup unit. null by design when the instance IAM role authorizes S3.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    users.users = lib.mkIf (cfg.user == "buzz") {
      buzz = {
        isSystemUser = true;
        group = cfg.group;
        home = cfg.dataDir;
        description = "maxplayer relay (buzz) service user";
      };
    };
    users.groups = lib.mkIf (cfg.group == "buzz") { buzz = { }; };

    services.postgresql = {
      enable = true;
      ensureDatabases = [ cfg.database ];
      ensureUsers = [
        {
          name = cfg.user;
          ensureDBOwnership = true;
        }
      ];
    };

    services.redis.servers.buzz = {
      enable = true;
      bind = "127.0.0.1";
      port = 6379;
    };

    # Refuse to start rather than half-start. A buzz relay with no stable identity key is the failure
    # that LOOKS healthy — it boots, answers queries, and quietly churns its pubkey every restart.
    # Assert the key file up front so the failure is loud and pre-start, not silent and in-service.
    systemd.services.buzz-relay-preflight = {
      description = "maxplayer relay preflight — refuse to start rather than half-start";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      before = [ "buzz-relay.service" ];
      requiredBy = [ "buzz-relay.service" ];
      path = [ pkgs.awscli2 ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        fail () { echo "INCAPABLE: $1" >&2; exit 1; }

        keyfile=${lib.escapeShellArg (toString cfg.privateKeyFile)}
        [ -s "$keyfile" ] || fail "relay identity key file is missing or empty at $keyfile — the relay \
          would boot with an unstable identity and churn its NIP-42 + NIP-11 pubkey on every restart"
        ${pkgs.gnugrep}/bin/grep -q '^BUZZ_RELAY_PRIVATE_KEY=' "$keyfile" \
          || fail "$keyfile exists but does not set BUZZ_RELAY_PRIVATE_KEY=<hex>"

        # Object store reachable + authorized BEFORE the relay starts: buzz-relay runs a FATAL
        # git-object-store conformance probe (the A3 gate) against s3Bucket at boot, so a missing
        # bucket or an unattached/under-scoped IAM role should fail loud HERE with a clear message,
        # not as a silent relay crash-loop. HeadBucket exercises the real endpoint + the instance role.
        # Caveat (adjacency): this uses aws-cli's credential resolver, NOT buzz's rust-s3 0.37 one — it
        # proves the bucket + role + network, but the DEFINITIVE proof of buzz's own client is its boot
        # probe. An IMDSv2-only box that starves rust-s3 would still pass this check.
        aws s3api head-bucket --bucket ${lib.escapeShellArg cfg.s3Bucket} --region ${lib.escapeShellArg cfg.s3Region} \
          || fail "cannot HeadBucket s3://${cfg.s3Bucket} in ${cfg.s3Region} via the instance role — \
            missing bucket, insufficient IAM (need Get/Put/List/DeleteObject), or no role attached"

        echo "preflight ok: relay identity key present; s3://${cfg.s3Bucket} reachable via instance role"
      '';
    };

    systemd.services.buzz-relay = {
      description = "maxplayer launch relay (buzz-relay)";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network-online.target"
        "postgresql.service"
        "redis-buzz.service"
      ];
      requires = [
        "postgresql.service"
        "redis-buzz.service"
      ];
      wants = [ "network-online.target" ];

      # buzz shells out to git (receive-pack for the CAS) AND runs the bundled pre-receive hook
      # (crates/buzz .../buzz-relay/src/api/git/hook.rs) on EVERY push. That hook is `#!/usr/bin/env bash`
      # and calls, by bare name: git (merge-base), coreutils (mktemp/date/sort/cat/rm/printf), sed,
      # openssl (HMAC over the ref updates) and curl (POST to the policy endpoint). ALL must be on the
      # unit PATH — a subprocess of buzz-relay inherits this PATH — or the hook fails closed:
      # "pre-receive hook declined", rejecting the push AND its delivery state. bash was the pre-line-1
      # death; openssl + curl are equally load-bearing (hook.rs's runtime_image test enforces them). The
      # schema is applied by buzz's own embedded sqlx migrator (BUZZ_AUTO_MIGRATE=1), no pgschema step.
      path = [
        pkgs.git
        pkgs.bash
        pkgs.coreutils
        pkgs.gnused
        pkgs.openssl
        pkgs.curl
      ];

      environment = baseEnv;

      serviceConfig = {
        User = cfg.user;
        Group = cfg.group;
        ExecStart = lib.getExe cfg.package;
        Restart = "on-failure";
        RestartSec = 5;
        StateDirectory = "buzz";
        StateDirectoryMode = "0750";
        WorkingDirectory = cfg.dataDir;

        # Carries BUZZ_RELAY_PRIVATE_KEY=<hex>. The preflight above has already asserted it is present.
        EnvironmentFile = [ cfg.privateKeyFile ];

        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
        ];
        ReadWritePaths = [ cfg.dataDir ];
      };
    };

    # Off-box backup of the one piece of durable state not ALREADY off-box: the Postgres event log
    # -> pg_dump | gzip -> S3 via the instance IAM role (no creds on the box). The git-CAS
    # (delivered-repo packs + manifests) and media objects live in S3 (s3Bucket) — versioned and
    # off-box by construction — so they need no separate on-box backup; restore = pg_restore, and the
    # objects sit untouched in S3. (The local <dataDir>/repos + pack cache are a rehydratable working
    # cache of those S3 objects, not a source of truth, so they are deliberately not dumped.)
    systemd.services.buzz-relay-backup = {
      description = "maxplayer relay backup — Postgres event log to ${cfg.backup.destination}";
      onFailure = [ "buzz-relay-backup-alert.service" ];
      serviceConfig = {
        Type = "oneshot";
        User = cfg.user;
        Group = cfg.group;
        RuntimeDirectory = "buzz-relay-backup";
      }
      // lib.optionalAttrs (cfg.backup.environmentFile != null) {
        EnvironmentFile = cfg.backup.environmentFile;
      };
      environment = {
        DESTINATION = cfg.backup.destination;
      };
      path = [
        pkgs.postgresql
        pkgs.gzip
        pkgs.coreutils
      ];
      # Report the byte counts, never gate on them: a born-empty relay legitimately dumps a near-empty
      # database and no repos on day one — the numbers belong in the log, not in a conditional.
      script = ''
        set -euo pipefail
        STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
        export DESTINATION STAMP

        # Postgres event log — the only durable state not already in S3.
        DUMP="$RUNTIME_DIRECTORY/buzz-$STAMP.sql.gz"
        PGHOST=/run/postgresql pg_dump ${lib.escapeShellArg cfg.database} | gzip > "$DUMP"
        echo "pg dump ok: $(wc -c < "$DUMP") bytes"
        # $FILE/$KEY set + exported on their OWN lines BEFORE the upload. A `FILE=.. KEY=.. cmd` prefix
        # does NOT bind them for that same command's own arg expansion, so under `set -u` the upload
        # aborted "unbound variable" every run — a backup that silently never ran. uploadCommand's
        # contract is $FILE/$KEY/$DESTINATION in the environment (DESTINATION is exported above).
        FILE="$DUMP"
        KEY="pg/buzz-$STAMP.sql.gz"
        export FILE KEY
        ${cfg.backup.uploadCommand}
      '';
    };

    # A backup that fails silently is a durability-layer lie — tonight's failed at 00:00 and nothing
    # noticed. OnFailure on buzz-relay-backup fires this the instant a run fails (shell error, or a
    # non-zero upload such as an S3 AccessDenied), so the failure PAGES. The guaranteed floor is a loud
    # emerg journal line even with no external pager wired; backup.alertCommand adds real out-of-band paging.
    systemd.services.buzz-relay-backup-alert = {
      description = "ALERT — maxplayer relay backup FAILED (off-box durability at risk)";
      serviceConfig = {
        Type = "oneshot";
        User = cfg.user;
        Group = cfg.group;
      }
      // lib.optionalAttrs (cfg.backup.environmentFile != null) {
        EnvironmentFile = cfg.backup.environmentFile;
      };
      environment = {
        DESTINATION = cfg.backup.destination;
      };
      path = [ pkgs.coreutils ];
      script = ''
        set -uo pipefail
        msg="maxplayer relay NIGHTLY BACKUP FAILED at $(date -u +%Y%m%dT%H%M%SZ) on $(uname -n) — no Postgres event-log object written to ${cfg.backup.destination}. Off-box durable state is now STALE; investigate: journalctl -u buzz-relay-backup.service"
        # Floor: an emerg-priority journal line, unmissable even with no external pager wired.
        printf '%s\n' "$msg" | ${pkgs.systemd}/bin/systemd-cat -t buzz-relay-backup-alert -p emerg || printf '%s\n' "$msg" >&2
        # Real out-of-band pager (host-supplied via backup.alertCommand; empty default = journal-only).
        ${cfg.backup.alertCommand}
      '';
    };

    systemd.timers.buzz-relay-backup = {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.backup.schedule;
        Persistent = true;
      };
    };
  };
}
