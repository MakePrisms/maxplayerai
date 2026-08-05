# Deployment host config for the launch relay — the concrete box that `nixosModules.relay` is designed to be
# imported into. Deploy, building ON the target so nothing cross-compiles from an x86 workstation:
#
#   nixos-rebuild switch --flake .#relay --target-host root@34.225.223.145 --build-host root@34.225.223.145
#
# Target: AWS EC2 t3.small-class (x86_64, 2 vCPU / 2 GB), first booted from a NixOS 26.05 x86_64 AMI. The AMI
# is only the bootstrap — after the first switch the running system is whatever this flake's nixpkgs (25.11)
# builds, so the box converges to 25.11 regardless of the AMI channel.
#
# FIRST switch on a fresh AMI needs a reboot to finish, and `nixos-rebuild` will NOT tell you: the 26.05 AMI
# runs dbus-broker while 25.11 uses dbus-daemon, so activation stops the very bus `switch-to-configuration` is
# talking to and then spins at 100% CPU on "Transport endpoint is not connected". It exited 0 while
# /run/current-system still pointed at the old 26.05 generation — a green deploy that changed nothing. The
# profile and GRUB are already correct at that point, so `reboot` activates the new generation cleanly.
# Verify the switch actually took by comparing, never by exit code:
#   readlink -f /run/current-system   # must equal /nix/var/nix/profiles/system
# Later switches within 25.11 do not cross the bus implementation and activate in place.
#
# Secrets are referenced by PATH only; gudnuf places the material on the box. Nothing here is a credential.
{
  config,
  lib,
  pkgs,
  modulesPath,
  ...
}:

{
  # Bootloader, root-growth, SSH host keys, cloud-init. gudnuf's SSH key arrives via the instance's
  # authorized_keys (EC2 injects it), so `root@34.225.223.145` works for the deploy without anything wired here.
  imports = [ "${modulesPath}/virtualisation/amazon-image.nix" ];

  services.maxplayer.relay = {
    enable = true;

    # The namespace this relay accepts. Every real market event carries ["t","maxplayer"]; a relay set
    # to any other value would reject every real event while looking like a healthy quiet relay.
    namespaceTag = "maxplayer";

    # NIP-11 identity every client reads (drafted — correct freely). `contact` is optional.
    info = {
      name = "maxplayer launch relay";
      description = "Launch relay for the maxplayer agent-hiring market. Single-namespace, born empty.";
      contact = ""; # optional: admin contact — an email or npub, or leave empty.
    };

    # Off-box backup — the module mandates it (a launch relay holding trade history must ship dumps
    # off-box), so there is no "skip backup" here by design. Dumps land in the S3 bucket below via the
    # instance's IAM role; no credentials live on the box.
    backup = {
      destination = "s3://maxplayer-relay-backup/launch";
      uploadCommand = ''${pkgs.awscli2}/bin/aws s3 cp "$DUMP" "$DESTINATION/$STAMP.jsonl"'';
      # null by design: the EC2 instance's IAM role (maxplayer-relay-backups) grants S3 write, so there is
      # no static credentials file on the box.
      environmentFile = null;
    };

    # volumeDevice left unset: relay data lives on the root EBS volume (durable across reboots, backed up
    # off-box to S3 by the unit above — that is the durability story). A dedicated EBS data volume mounted
    # at the module's dataDir /var/lib/strfry would additionally survive instance *replacement*; not wired
    # for launch (gudnuf's call — root EBS + S3 backups).
  };

  # The relay binds 127.0.0.1:7777 by construction (single-namespace box, no public bind), so something must
  # terminate TLS and proxy wss -> the relay or it is unreachable. This is the batteries-included default.
  # Public domain relay.maxplayer.ai, ACME contact below. If TLS is terminated elsewhere (Cloudflare, an
  # ALB), drop this nginx/acme/firewall trio and point that proxy at 127.0.0.1:7777 instead.
  services.nginx = {
    enable = true;
    recommendedProxySettings = true; # sets X-Forwarded-For, which the relay reads as realIpHeader.
    recommendedTlsSettings = true;
    virtualHosts."relay.maxplayer.ai" = {
      # DNS: relay.maxplayer.ai → 34.225.223.145 (live); ACME issues on first switch.
      enableACME = true;
      forceSSL = true;
      locations."/" = {
        proxyPass = "http://127.0.0.1:7777";
        proxyWebsockets = true;
      };
    };
  };

  security.acme = {
    acceptTerms = true;
    defaults.email = "contact@agi.cash";
  };

  networking.firewall.allowedTCPPorts = [
    80
    443
  ];

  # A t3.small has 2 GB and no swap, but `nixos-rebuild --build-host` builds this box's own closure ON the box:
  # the coordinating `nix` process alone peaked at 1.57 GB RSS and was OOM-killed, taking the deploy with it
  # (ssh exit 255). Swap is what makes the box able to build itself. 4 GB on a 59 GB root volume is free real
  # estate; without it every future switch is a coin flip against the OOM killer.
  swapDevices = [
    {
      device = "/var/lib/swapfile";
      size = 4096; # MiB
    }
  ];

  # Default is 60 — too eager to page out a live relay's LMDB working set. Swap here is headroom for build
  # spikes, not a place to keep the running relay, so only lean on it under real pressure.
  boot.kernel.sysctl."vm.swappiness" = 10;

  # The release this box's stateful defaults are pinned to. It is the flake's nixpkgs (25.11), NOT the AMI's
  # channel — the system converges to what this flake builds on first switch.
  system.stateVersion = "25.11";
}
