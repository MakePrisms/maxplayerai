# Separate, supervised relay-owner review worker. Secret contents never enter Nix.
{ config, lib, pkgs, ... }:
let
  cfg = config.services.maxplayer.reviewer;
  settings = pkgs.writeText "maxplayer-reviewer-settings.json" (builtins.toJSON {
    relay = cfg.relayUrl;
    model = cfg.model;
    database = "/var/lib/maxplayer-reviewer/reviews.sqlite";
    private_git_base = cfg.privateGitBase;
    accepted_mints = cfg.acceptedMints;
  });
  start = pkgs.writeShellScript "maxplayer-reviewer-start" ''
    set -eu
    umask 077
    # LoadCredential may expose read-only 0440 files in a protected mount.
    # The reviewer requires owner-only regular files, so stage private copies
    # in RuntimeDirectory (0700, service-owned, removed when the unit stops).
    ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/signer" /run/maxplayer-reviewer/signer
    ${pkgs.coreutils}/bin/install -m 0600 "$CREDENTIALS_DIRECTORY/typesafe" /run/maxplayer-reviewer/typesafe
    ${pkgs.jq}/bin/jq \
      --arg signer /run/maxplayer-reviewer/signer \
      --arg provider /run/maxplayer-reviewer/typesafe \
      '. + {signer_file: $signer, provider_key_file: $provider}' \
      ${settings} > /run/maxplayer-reviewer/reviewer.json
    exec ${lib.getExe cfg.package} reviewer serve /run/maxplayer-reviewer/reviewer.json
  '';
in
{
  options.services.maxplayer.reviewer = {
    enable = lib.mkEnableOption "the Maxplayer execution reviewer";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Maxplayer binary built with the wallet feature (includes reviewer serve).";
    };
    relayUrl = lib.mkOption {
      type = lib.types.str;
      description = "WSS relay URL; Git deliveries are fetched from the corresponding HTTPS origin.";
      example = "wss://relay.maxplayer.ai";
    };
    privateGitBase = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Trusted private Git prefix; null uses the relay HTTPS /git/ origin.";
    };
    acceptedMints = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "https://testnut.cashudevkit.org" ];
      description = "Trusted mint allowlist for validating private lifecycle evidence; match client policy.";
    };
    signerFile = lib.mkOption {
      type = lib.types.str;
      description = "Absolute runtime path to the reviewer signing key. For private jobs this must be the configured private-content service identity, not the relay key.";
    };
    providerKeyFile = lib.mkOption {
      type = lib.types.str;
      description = "Absolute runtime path to the raw TypeSafe API key, never the key value or a Nix store path.";
    };
    model = lib.mkOption {
      type = lib.types.str;
      default = "jev-latest";
      description = "TypeSafe model identifier.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.hasPrefix "wss://" cfg.relayUrl;
        message = "Maxplayer reviewer relayUrl must use wss:// for authenticated HTTPS Git fetching.";
      }
      {
        assertion = builtins.all (path: lib.hasPrefix "/" path && !(lib.hasPrefix "/nix/store/" path)) [ cfg.signerFile cfg.providerKeyFile ];
        message = "Maxplayer reviewer secrets must be runtime absolute paths outside the Nix store.";
      }
    ];

    systemd.services.maxplayer-reviewer = {
      description = "Maxplayer execution reviewer";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      # Ordering only: a reviewer failure must not stop the relay. If the relay
      # is remote, this absent local unit does not become a requirement.
      after = [ "network-online.target" "buzz-relay.service" ];
      environment.RUST_LOG = "info";
      serviceConfig = {
        Type = "simple";
        ExecStart = start;
        DynamicUser = true;
        User = "maxplayer-reviewer";
        StateDirectory = "maxplayer-reviewer";
        StateDirectoryMode = "0700";
        RuntimeDirectory = "maxplayer-reviewer";
        RuntimeDirectoryMode = "0700";
        WorkingDirectory = "/var/lib/maxplayer-reviewer";
        LoadCredential = [ "signer:${cfg.signerFile}" "typesafe:${cfg.providerKeyFile}" ];
        Restart = "on-failure";
        RestartSec = "5s";
        KillSignal = "SIGINT";
        TimeoutStopSec = "45s";
        UMask = "0077";
        NoNewPrivileges = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
      };
    };
  };
}
