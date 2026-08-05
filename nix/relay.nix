# Launch-relay deployment, exported as `nixosModules.relay`.
#
# The host repo imports this and supplies hostname, hardware and secrets. Everything here versions with the
# protocol, which is the point: the relay's write policy and the wire format ship as one artifact.
#
# Built on nixpkgs' `services.strfry`, whose `settings` option is freeform JSON — that is what lets the write
# policy be expressed at all. The `orveth/strfry-nix` flake cannot express `relay.writePolicy.plugin`: it
# renders strfry.conf from a fixed template, and its only escape hatch appends outside the `relay { }` block.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.maxplayer.relay;

  # The data directory is deliberately strfry's default. nixpkgs' module derives three settings from it
  # (`StateDirectory`, `WorkingDirectory`, `ReadWritePaths`), so pointing strfry at a foreign mount makes
  # those three name different places for no gain. Mount the persistent volume AT this path instead.
  dataDir = "/var/lib/strfry";

  # nixpkgs' module renders strfry.conf into the store and passes it on the ExecStart line, so there is no
  # path on disk for another unit to reuse. Regenerating from the resolved `settings` gives the same content:
  # the option's `apply = recursiveUpdate defaultSettings` merges on read, so this is the full effective
  # config and not just our overrides.
  strfryConfigFile = (pkgs.formats.json { }).generate "config.json" config.services.strfry.settings;
in
{
  options.services.maxplayer.relay = {
    enable = lib.mkEnableOption "the maxplayer launch relay";

    namespaceTag = lib.mkOption {
      type = lib.types.str;
      default = "maxplayer";
      description = ''
        Value of the `t` tag the relay accepts, and the ONLY namespace it accepts.

        Parameterised on the value on purpose: it is configuration, not a recompile. Every real market
        event carries `["t","maxplayer"]`; a relay set to any other value would reject every real event
        while looking exactly like a healthy quiet relay.
      '';
    };

    writePolicyPackage = lib.mkOption {
      type = lib.types.package;
      description = ''
        Package providing the write-policy plugin as `mainProgram`.

        MUST be a compiled binary, not a script in a JIT runtime. strfry spawns the plugin as a child of its
        own unit, so the plugin inherits the unit's sandbox — and that sandbox sets
        `MemoryDenyWriteExecute = true`, which a JIT allocating W+X pages cannot survive. Rust is fine and is
        what this repo already builds.
      '';
    };

    volumeDevice = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/dev/disk/by-label/relay-data";
      description = ''
        Block device expected to be mounted at ${dataDir}.

        When set, preflight refuses to start the relay unless that path is a real mount point. Left null, the
        relay runs on the root filesystem — acceptable for a scratch dry run, never for the launch VM.
      '';
    };

    info = {
      name = lib.mkOption {
        type = lib.types.str;
        description = ''
          NIP-11 relay name. No default on purpose: this document is what every client reads, and an
          inherited one makes the relay introduce itself as someone else's box.
        '';
      };

      description = lib.mkOption {
        type = lib.types.str;
        description = "NIP-11 relay description. No default, for the same reason as `name`.";
      };

      contact = lib.mkOption {
        type = lib.types.str;
        default = "";
        description = "NIP-11 admin contact. Empty means the key is omitted rather than sent empty.";
      };
    };

    backup = {
      destination = lib.mkOption {
        type = lib.types.str;
        example = "s3://maxplayer-relay-backups/launch";
        description = ''
          Where dumps go. Required — a backup unit with no stated destination is a timer that reports success
          for writing nothing.
        '';
      };

      retentionDays = lib.mkOption {
        type = lib.types.ints.positive;
        default = 30;
        description = ''
          Passed to `uploadCommand` as `$RETENTION_DAYS`. Enforcement belongs to the destination — a bucket
          lifecycle rule, or the command itself. This module cannot verify a remote retention policy, and
          claiming to implement one it cannot check is worse than naming who owns it.
        '';
      };

      uploadCommand = lib.mkOption {
        type = lib.types.str;
        example = "aws s3 cp \"\$DUMP\" \"\$DESTINATION/\$STAMP.jsonl\"";
        description = ''
          Command that moves the dump off the box. Runs with `$DUMP` (path to the JSONL dump), `$DESTINATION`,
          `$RETENTION_DAYS` and `$STAMP` (UTC timestamp) in the environment.

          Required, and deliberately not defaulted, because the transport is the host's infrastructure. A
          default here would ship a unit that exits 0 having written the dump into a runtime directory that is
          then discarded — a backup timer whose green is unrelated to any backup existing.
        '';
      };

      schedule = lib.mkOption {
        type = lib.types.str;
        default = "daily";
        description = "systemd calendar spec for the dump timer.";
      };

      environmentFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = ''
          Path to credentials for the backup destination. A PATH, never a Nix string — a credential passed as
          an option value lands world-readable in /nix/store.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    services.strfry = {
      enable = true;

      settings = {
        db = dataDir;

        relay = {
          # Loopback only; a proxy fronts this and terminates TLS.
          bind = "127.0.0.1";
          port = 7777;
          realIpHeader = "x-forwarded-for";

          # Set unconditionally. The module's defaults supply empty strings for contact/icon/nips/pubkey and
          # `recursiveUpdate` merges them in regardless, so omitting a key does not make it absent — measured:
          # NIP-11 renders `contact = ""` either way. An `optionalAttrs` guard here would be dead code that
          # reads like a decision, which is worse than no guard.
          info = {
            inherit (cfg.info) name description contact;
          };

          # The relay accepts one namespace and nothing else. This is the mechanism that makes "born
          # v1/maxplayer-only" a property we assert rather than a state we hope holds: born-empty is about
          # what is in the database on day one, and says nothing about what day two accepts.
          writePolicy.plugin = lib.getExe cfg.writePolicyPackage;
        };
      };
    };

    # There is deliberately no strfry-router here, and its absence is load-bearing rather than incidental.
    # A router stream with `dir = "down"` from public relays imports foreign events, which contradicts the
    # single-namespace guarantee the write policy exists to provide. nixpkgs' strfry module has no router
    # option at all, so this is enforced by construction — do not add one to reach parity with another box.

    systemd.services.strfry = {
      # The write-policy plugin reads the namespace tag from MAXPLAYER_RELAY_TAG. strfry spawns the plugin as
      # a child and it inherits this unit's environment, so this is where the module→plugin contract is wired.
      # Without it the plugin refuses to run (it will not default the tag), strfry's write policy fails, and
      # the relay is broken — the module evaluated green for weeks with this missing because the plugin did not
      # yet exist to have a contract. The scratch dry run is what surfaced it.
      environment.MAXPLAYER_RELAY_TAG = cfg.namespaceTag;

      # strfry does NOT exec the plugin directly — it runs `posix_spawnp("sh", {"/bin/sh","-c",<plugin>})`,
      # resolving `sh` through this unit's PATH. nixpkgs' hardened strfry unit gives the service a PATH of
      # coreutils/findutils/grep/sed/systemd with NO shell, so `posix_spawnp` cannot find `sh` and fails with
      # ENOENT — which strfry logs against the PLUGIN path, not sh, so the error misdirects the diagnosis
      # entirely. A shell on the unit PATH is what actually launches the plugin. Measured via the scratch dry
      # run: the plugin runs fine under `systemd-run` with every sandbox directive, yet never launches here,
      # because the launcher is sh and sh was unreachable.
      path = [ pkgs.bash ];

      # nixpkgs' module hardcodes `Restart = "on-failure"` and does not expose it. A clean exit is then never
      # restarted, and the unit reads *inactive/success* rather than *failed* — so a state column shows
      # nothing wrong while the relay is off.
      #
      # mkForce is required, not stylistic: the module's definition is at normal priority, so a plain
      # assignment here is a CONFLICTING definition and the whole system stops evaluating. Measured — an
      # earlier draft used a plain assignment, parsed clean, and failed to evaluate.
      serviceConfig.Restart = lib.mkForce "always";

      # The module sets `wants = [ "network.target" ]` with no ordering. Not network-online.target: waiting on
      # it is one of the reboot traps that fails as success.
      after = [ "network.target" ];

      # The preflight dependency is declared once, on the preflight unit (`before` + `requiredBy`). Wiring it
      # from both sides describes one edge twice and invites a cycle nobody can find.
    };

    # Preflight declares the relay INCAPABLE and refuses to start it, rather than letting it half-start into a
    # state that serves and looks healthy. Every check names the property it failed on.
    systemd.services.maxplayer-relay-preflight = {
      description = "maxplayer relay preflight — refuse to start rather than half-start";
      before = [ "strfry.service" ];
      requiredBy = [ "strfry.service" ];

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };

      script = ''
        fail () { echo "INCAPABLE: $1" >&2; exit 1; }

        plugin=${lib.escapeShellArg (lib.getExe cfg.writePolicyPackage)}
        [ -x "$plugin" ] || fail "write-policy plugin is not executable at $plugin — strfry would start and \
          accept everything, because a plugin it cannot run is indistinguishable from no plugin configured"

        ${lib.optionalString (cfg.volumeDevice != null) ''
          ${pkgs.util-linux}/bin/mountpoint -q ${lib.escapeShellArg dataDir} \
            || fail "${dataDir} is not a mount point — expected ${cfg.volumeDevice}. Relay data would land on \
              the root filesystem and be lost with the instance"
        ''}

        echo "preflight ok: plugin executable${lib.optionalString (cfg.volumeDevice != null) ", data volume mounted"}"
      '';
    };

    systemd.services.maxplayer-relay-backup = {
      description = "maxplayer relay backup — strfry export to ${cfg.backup.destination}";

      serviceConfig = {
        Type = "oneshot";
        User = "strfry";
        Group = "strfry";

        # strfry calls setrlimit(NOFILE) to `relay.nofiles` on EVERY subcommand, `export` included. This
        # oneshot otherwise inherits systemd's default hard cap and `export` dies "Unable to set NOFILES limit
        # to N, exceeds max". The nixpkgs module raises it for the main relay unit but not for ours; mirror it.
        LimitNOFILE = config.services.strfry.settings.relay.nofiles;

        # $RUNTIME_DIRECTORY only exists because this is set. The dump is written here and discarded on exit,
        # which is why `uploadCommand` is mandatory rather than defaulted.
        RuntimeDirectory = "maxplayer-relay-backup";
      }
      // lib.optionalAttrs (cfg.backup.environmentFile != null) {
        EnvironmentFile = cfg.backup.environmentFile;
      };

      environment = {
        DESTINATION = cfg.backup.destination;
        RETENTION_DAYS = toString cfg.backup.retentionDays;
      };

      # `strfry export` rather than a file copy: the database is LMDB and live, so copying its files under a
      # running relay yields a torn snapshot that restores as a subtly corrupt database. An export is a JSONL
      # dump taken through strfry itself.
      #
      # The config is regenerated here rather than read from a path. nixpkgs' module renders it into the store
      # and passes it via ExecStart, so there is no file on disk to point at — an earlier draft of this unit
      # said `--config=/etc/strfry.conf`, which does not exist and would have failed only when the timer fired.
      script = ''
        set -euo pipefail

        STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
        DUMP="$RUNTIME_DIRECTORY/dump-$STAMP.jsonl"
        export STAMP DUMP

        ${lib.getExe config.services.strfry.package} --config=${strfryConfigFile} export > "$DUMP"

        # Report the denominator, never gate on it. A born-empty relay legitimately exports zero events on day
        # one, so "refuse to upload an empty dump" would fire on the correct state — the number belongs in the
        # log, not in a conditional.
        echo "export ok: $(wc -l < "$DUMP") events, $(wc -c < "$DUMP") bytes → $DESTINATION"

        ${cfg.backup.uploadCommand}
      '';
    };

    systemd.timers.maxplayer-relay-backup = {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.backup.schedule;
        Persistent = true;
      };
    };
  };
}
