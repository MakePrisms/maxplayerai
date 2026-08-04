# Launch relay: `services.maxplayer.relay` — the buzz-derived relay (crate `buzz-relay`) that backs
# relay.maxplayer.ai. One binary serving events + git + payments over a closed, compiled kind
# allowlist: the mobee namespace IS the allowlist, so there is no write-policy plugin (the strfry
# plugin retires with the strfry relay it fronted). Born empty.
#
# TLS lives in the host (nix/relay-host.nix): nginx terminates and proxies wss -> 127.0.0.1:3000, so
# this module binds loopback only and never touches ACME. Postgres + Redis are provisioned here; the
# relay's stable identity key is supplied by the host as a file PATH (never in the repo).
#
# The package is wired from the flake as an option (`services.maxplayer.relay.package`), the same way
# the strfry module took `writePolicyPackage` — so `.#relay` (in-tree vendored crate) and
# `.#relay-forkpin` (pinned fork) share this one module and differ only in the package they inject.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.maxplayer.relay;

  # pgschema: pinned release binary (not in nixpkgs). buzz applies its schema declaratively at
  # ExecStartPre; pinning the migrator means a nixpkgs bump cannot silently change how the schema is
  # applied. Same version buzz-nix proved (1.7.4).
  pgschemaVersion = "1.7.4";
  pgschemaBin = pkgs.stdenvNoCC.mkDerivation {
    pname = "pgschema";
    version = pgschemaVersion;
    src =
      let
        plat =
          {
            x86_64-linux = {
              suffix = "linux-amd64";
              hash = "sha256-aCaRotGGmbYzvskE5lJulMPfVrSzZtrxJVRresad5Mg=";
            };
            aarch64-linux = {
              suffix = "linux-arm64";
              hash = "sha256-LYwV87bxHc1g0vhIV8rWU3TaF2EoHpcqrSHX32SjQKw=";
            };
          }
          .${pkgs.stdenv.hostPlatform.system}
            or (throw "pgschema: unsupported system ${pkgs.stdenv.hostPlatform.system}");
      in
      pkgs.fetchurl {
        url = "https://github.com/pgplex/pgschema/releases/download/v${pgschemaVersion}/pgschema-${pgschemaVersion}-${plat.suffix}";
        inherit (plat) hash;
      };
    dontUnpack = true;
    nativeBuildInputs = [ pkgs.autoPatchelfHook ];
    buildInputs = [ pkgs.stdenv.cc.cc.lib ];
    installPhase = ''
      runHook preInstall
      install -Dm755 "$src" "$out/bin/pgschema"
      runHook postInstall
    '';
    meta.mainProgram = "pgschema";
  };

  # Environment the buzz-relay binary reads. Loopback bind (nginx fronts). Open membership: a public
  # marketplace where any NIP-42-authed key may write — buzz still MANDATES a signed NIP-42 handshake
  # on every write, so "open" means "not membership-gated", never "anonymous". The compiled kind
  # allowlist is what scopes the namespace to mobee.
  baseEnv = {
    BUZZ_BIND_ADDR = cfg.bindAddr;
    DATABASE_URL = "postgresql:///${cfg.database}?host=/run/postgresql&user=${cfg.user}";
    REDIS_URL = "redis://127.0.0.1:6379";
    RELAY_URL = cfg.relayUrl;
    BUZZ_HEALTH_PORT = toString cfg.healthPort;
    BUZZ_METRICS_PORT = toString cfg.metricsPort;
    BUZZ_GIT_REPO_PATH = "${cfg.dataDir}/repos";
    BUZZ_REQUIRE_AUTH_TOKEN = lib.boolToString cfg.requireAuthToken;
    BUZZ_REQUIRE_RELAY_MEMBERSHIP = lib.boolToString cfg.requireRelayMembership;
  }
  // lib.optionalAttrs (cfg.ownerPubkey != null) { RELAY_OWNER_PUBKEY = cfg.ownerPubkey; };
in
{
  options.services.maxplayer.relay = {
    enable = lib.mkEnableOption "the maxplayer launch relay (buzz-derived)";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        The buzz-relay package to run. Wired from the flake:
        `packages.maxplayer-relay` (in-tree vendored crate) or `packages.maxplayer-relay-forkpin`
        (pinned gudnuf/buzz fork). `lib.getExe` resolves its `meta.mainProgram` (buzz-relay).
      '';
    };

    schemaFile = lib.mkOption {
      type = lib.types.path;
      description = ''
        buzz's schema.sql, applied declaratively by pgschema at ExecStartPre. Wired from the flake to
        the vendored package source's schema so it always tracks the code being deployed
        (`''${self}/crates/buzz/schema/schema.sql`).
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/buzz";
      description = "State directory: git name-reservation index + `<dataDir>/repos` content store.";
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
      before = [ "buzz-relay.service" ];
      requiredBy = [ "buzz-relay.service" ];
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

        echo "preflight ok: relay identity key present"
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

      # buzz shells out to git (receive-pack for the CAS) and psql (schema apply).
      path = [
        pkgs.git
        pkgs.postgresql
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

        ExecStartPre = [
          (pkgs.writeShellScript "buzz-relay-migrate" ''
            set -euo pipefail
            export PGHOST=/run/postgresql
            export PGPORT=5432
            export PGUSER=${lib.escapeShellArg cfg.user}
            export PGDATABASE=${lib.escapeShellArg cfg.database}

            # buzz perf index, created idempotently. On a born-empty db the `events` table does not
            # exist yet, so this no-ops (ON_ERROR_STOP=0 || true) and the schema apply below creates
            # it; on later boots it is a cheap IF NOT EXISTS.
            ${pkgs.postgresql}/bin/psql -v ON_ERROR_STOP=0 -q -c \
              "CREATE INDEX IF NOT EXISTS idx_events_parameterized ON events (kind, pubkey, d_tag, deleted_at) WHERE d_tag IS NOT NULL;" \
              2>/dev/null || true

            ${lib.getExe pgschemaBin} apply \
              --host "$PGHOST" --port "$PGPORT" --user "$PGUSER" --db "$PGDATABASE" \
              --plan-host "$PGHOST" --plan-port "$PGPORT" --plan-user "$PGUSER" --plan-db "$PGDATABASE" \
              --file ${cfg.schemaFile} \
              --auto-approve
          '')
        ];

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

    # Off-box backup, two artifacts, both shipped by the instance IAM role (no creds on the box):
    #   1. Postgres  — the event log        -> pg_dump | gzip
    #   2. Git CAS   — delivered-job repos   -> tar | gzip
    # Buzz keeps git objects on the local filesystem (BUZZ_GIT_REPO_PATH), NOT in Postgres, so a
    # pg_dump alone would leave delivered repos un-backed-up and the box only half-disposable. Both
    # dumps together are the durability story: restore = pg_restore + untar into dataDir/repos.
    # (Fast-follow, deferred: point buzz-media at S3 for LIVE off-box object storage — needs a
    # buzz-media instance-role-vs-static-creds answer. See the relay OPEN ITEMS in the deploy block.)
    systemd.services.buzz-relay-backup = {
      description = "maxplayer relay backup — Postgres + git CAS to ${cfg.backup.destination}";
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
        pkgs.gnutar
        pkgs.gzip
        pkgs.coreutils
      ];
      # Report the byte counts, never gate on them: a born-empty relay legitimately dumps a near-empty
      # database and no repos on day one — the numbers belong in the log, not in a conditional.
      script = ''
        set -euo pipefail
        STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
        export DESTINATION STAMP

        # 1. Postgres event log.
        DUMP="$RUNTIME_DIRECTORY/buzz-$STAMP.sql.gz"
        PGHOST=/run/postgresql pg_dump ${lib.escapeShellArg cfg.database} | gzip > "$DUMP"
        echo "pg dump ok: $(wc -c < "$DUMP") bytes"
        FILE="$DUMP" KEY="pg/buzz-$STAMP.sql.gz" ${cfg.backup.uploadCommand}

        # 2. Git CAS (delivered-job repos). Absent on a born-empty relay — skip cleanly until it exists.
        if [ -d ${lib.escapeShellArg "${cfg.dataDir}/repos"} ]; then
          REPOS="$RUNTIME_DIRECTORY/repos-$STAMP.tar.gz"
          tar czf "$REPOS" -C ${lib.escapeShellArg cfg.dataDir} repos
          echo "repos dump ok: $(wc -c < "$REPOS") bytes"
          FILE="$REPOS" KEY="repos/repos-$STAMP.tar.gz" ${cfg.backup.uploadCommand}
        else
          echo "repos dump skipped: ${cfg.dataDir}/repos does not exist yet (born-empty relay)"
        fi
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
