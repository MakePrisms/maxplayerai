# DRAFT NixOS module for the registration service. NOT imported by flake.nix or
# nix/relay-host.nix, and not evaluated in CI: wiring it in (a flake package for this crate,
# `nixosModules.registration`, the host settings) is a separate, reviewed deploy change.
#
# What it sets up on the relay box:
#   - Postgres database `maxplayer_registration` owned by role `maxplayer_registration`
#     (ensureDBOwnership; the role is NOT a superuser), peer auth over /run/postgresql.
#   - the service on 127.0.0.1:3100, secrets via LoadCredential re-installed 0600.
#   - nginx `location /account/` on relay.maxplayer.ai -> 127.0.0.1:3100.
#   - the database in the nightly backup (see README "Backups" for the pg_dump lines).
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.maxplayer.registration;
  runDir = "/run/maxplayer-registration";
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
    package = lib.mkOption { type = lib.types.package; };
    port = lib.mkOption {
      type = lib.types.port;
      default = 3100;
    };
    publicBaseUrl = lib.mkOption {
      type = lib.types.str;
      default = "https://relay.maxplayer.ai";
    };
    relayPubkey = lib.mkOption {
      type = lib.types.str;
      description = "The relay's own pubkey (signer of the kind-13534 member list).";
    };
    user = lib.mkOption {
      type = lib.types.str;
      default = "maxplayer_registration";
    };
    database = lib.mkOption {
      type = lib.types.str;
      default = "maxplayer_registration";
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
    settings = lib.mkOption {
      type = lib.types.attrs;
      default = { };
      description = "Extra config.toml values (legal versions, limits, sync).";
    };
  };

  config = lib.mkIf cfg.enable {
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

    systemd.services.maxplayer-registration = {
      description = "Maxplayer registration service";
      wantedBy = [ "multi-user.target" ];
      after = [
        "network.target"
        "postgresql.service"
        "buzz-relay.service"
      ];
      requires = [ "postgresql.service" ];
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
        ExecStartPre = map (n: "${pkgs.coreutils}/bin/install -m 0600 \${CREDENTIALS_DIRECTORY}/${n} ${runDir}/${n}") [
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

    # /account/ on the relay vhost -> this service. The relay itself keeps "/".
    services.nginx.virtualHosts."relay.maxplayer.ai".locations."/account/" = {
      proxyPass = "http://127.0.0.1:${toString cfg.port}";
      extraConfig = ''
        client_max_body_size 64k;
        proxy_set_header X-Real-IP $remote_addr;
      '';
    };

    # Spec §11: enable swap as a safety net on the 2 GB box.
    swapDevices = lib.mkDefault [
      {
        device = "/var/lib/swapfile";
        size = 2048;
      }
    ];
  };
}
