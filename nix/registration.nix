# `services.maxplayer.registration`: the email registration service (crate
# `crates/maxplayer-registration`, email-signup spec v2.12) on the relay box. Wired into `.#relay` by
# flake.nix (`nixosModules.registration`, package `packages.maxplayer-registration`); the host values
# (enable, relayPubkey) live in nix/relay-host.nix.
#
# What it sets up:
#   - Postgres database `maxplayer_registration` owned by role `maxplayer_registration`
#     (ensureDBOwnership; the role is NOT a superuser), peer auth over /run/postgresql.
#   - the service on 127.0.0.1:3100, secrets via LoadCredential re-installed 0600.
#   - nginx `location /account/` on relay.maxplayer.ai -> 127.0.0.1:3100.
#   - the database in the relay's nightly backup (`services.maxplayer.relay.backup.extraDatabases`),
#     dumped as its own role, so the relay's `buzz` user never gets access to it.
#
# No swap here: the host (nix/relay-host.nix) already defines the box's swapfile.
#
# Secrets are referenced by PATH only and placed on the box by an operator before the first switch:
#   /var/lib/secrets/maxplayer-registration-admin-key     64-hex or nsec1 (the relay admin key)
#   /var/lib/secrets/maxplayer-registration-resend-key    re_...
#   /var/lib/secrets/maxplayer-registration-resend-webhook whsec_...
# A missing file fails only this unit (LoadCredential), never the relay.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.maxplayer.registration;
  runDir = "/run/maxplayer-registration";
  hexKey = lib.types.strMatching "[0-9a-f]{64}";
  configFile = (pkgs.formats.toml { }).generate "maxplayer-registration.toml" (
    lib.recursiveUpdate {
      bind_addr = "127.0.0.1:${toString cfg.port}";
      public_base_url = cfg.publicBaseUrl;
      database_url = "postgresql:///${cfg.database}?host=/run/postgresql&user=${cfg.user}";
      trust_x_real_ip = true;
      relay = {
        base_url = "http://127.0.0.1:${toString config.services.maxplayer.relay.port}";
        auth_base_url = cfg.publicBaseUrl;
        admin_key_file = "${runDir}/admin-key";
        relay_pubkey = cfg.relayPubkey;
      };
      email = {
        api_key_file = "${runDir}/resend-key";
        webhook_secret_file = "${runDir}/resend-webhook";
      };
    } cfg.settings
  );
in
{
  options.services.maxplayer.registration = {
    enable = lib.mkEnableOption "the Maxplayer email registration service";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Wired from the flake (`packages.maxplayer-registration`).";
    };
    port = lib.mkOption {
      type = lib.types.port;
      default = 3100;
    };
    publicBaseUrl = lib.mkOption {
      type = lib.types.str;
      default = "https://relay.maxplayer.ai";
      description = ''
        Public origin. NIP-98 `u` tags from clients must be <this>/account/..., and the service signs
        its own 9030/9031 for <this>/events (sent to the relay's loopback port with this Host header).
      '';
    };
    relayPubkey = lib.mkOption {
      type = hexKey;
      description = ''
        The relay's own pubkey, 64 lowercase hex: the signer of the kind-13534 member list (NIP-11
        `self`). Not the owner key and not the service's admin key.
      '';
    };
    user = lib.mkOption {
      type = lib.types.str;
      default = "maxplayer_registration";
    };
    database = lib.mkOption {
      type = lib.types.str;
      default = "maxplayer_registration";
      description = "Must equal `user` (ensureDBOwnership).";
    };
    # Paths only; the files are placed on the box by an operator, never in the store.
    adminKeyFile = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/secrets/maxplayer-registration-admin-key";
    };
    resendKeyFile = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/secrets/maxplayer-registration-resend-key";
    };
    resendWebhookSecretFile = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/secrets/maxplayer-registration-resend-webhook";
    };
    backup = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Add the database to the relay's nightly buzz-relay-backup (needs the relay module enabled).";
    };
    settings = lib.mkOption {
      type = lib.types.attrs;
      default = { };
      description = "Extra config.toml values (legal versions, limits, sync).";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.user == cfg.database;
        message = "services.maxplayer.registration: user and database must match (ensureDBOwnership).";
      }
      {
        assertion = config.services.maxplayer.relay.enable;
        message = "services.maxplayer.registration needs services.maxplayer.relay (its Postgres, port and nginx vhost).";
      }
    ];

    users.users.${cfg.user} = {
      isSystemUser = true;
      group = cfg.user;
    };
    users.groups.${cfg.user} = { };

    services.postgresql = {
      ensureDatabases = [ cfg.database ];
      ensureUsers = [
        {
          name = cfg.user;
          ensureDBOwnership = true;
        }
      ];
    };

    services.maxplayer.relay.backup.extraDatabases = lib.mkIf cfg.backup [
      {
        database = cfg.database;
        user = cfg.user;
      }
    ];

    systemd.services.maxplayer-registration = {
      description = "Maxplayer registration service";
      wantedBy = [ "multi-user.target" ];
      # postgresql.target = postgresql.service + postgresql-setup.service, which creates the role and
      # database (ensureUsers/ensureDatabases). Ordering on postgresql.service alone can start the
      # service before its role exists on the first switch.
      after = [
        "network.target"
        "postgresql.target"
        "buzz-relay.service"
      ];
      requires = [ "postgresql.target" ];
      environment.RUST_LOG = "info";
      serviceConfig = {
        User = cfg.user;
        Group = cfg.user;
        RuntimeDirectory = "maxplayer-registration";
        RuntimeDirectoryMode = "0700";
        LoadCredential = [
          "admin-key:${cfg.adminKeyFile}"
          "resend-key:${cfg.resendKeyFile}"
          "resend-webhook:${cfg.resendWebhookSecretFile}"
        ];
        # LoadCredential copies are 0440; the service refuses group-readable secrets
        # (secret_file_must_be_private), so re-install them 0600 into the 0700 RuntimeDirectory.
        ExecStartPre =
          map (n: "${pkgs.coreutils}/bin/install -m 0600 \${CREDENTIALS_DIRECTORY}/${n} ${runDir}/${n}")
            [
              "admin-key"
              "resend-key"
              "resend-webhook"
            ];
        ExecStart = "${lib.getExe cfg.package} serve --config ${configFile}";
        Restart = "on-failure";
        RestartSec = 5;
        MemoryMax = "128M";
        TasksMax = 64;
        NoNewPrivileges = true;
        UMask = "0077";
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
      };
    };

    # /account/ on the relay vhost -> this service. The relay itself keeps "/". The recommended proxy
    # headers (Host, X-Real-IP, X-Forwarded-*) are what `trust_x_real_ip = true` relies on, so they are
    # forced on here rather than inherited from the host's global setting.
    services.nginx.virtualHosts."relay.maxplayer.ai".locations."/account/" = {
      proxyPass = "http://127.0.0.1:${toString cfg.port}";
      recommendedProxySettings = true;
      extraConfig = ''
        client_max_body_size 64k;
      '';
    };
  };
}
