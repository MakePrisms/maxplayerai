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
  # authorized_keys (EC2 injects it), so `root@34.225.223.145` works for the deploy out of the box.
  imports = [ "${modulesPath}/virtualisation/amazon-image.nix" ];

  # Deploy/relay access (#399): petar's + jbojcic's keys alongside gudnuf's EC2-injected one. PUBLIC
  # keys (safe in-repo); root because the deploy runs as root (nixos-rebuild --target-host root@).
  # Ships in the PR gudnuf deploys, so his review of the grant is inherent.
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCoY4sE+HgKK8L2+1oWgnmynmtXCgyv9nNetJNDdmnUOS5YSEurmB/YSqcUdz1BISvM8ibyuwU1HAEJWID6+PpxYm3dPmFxUiKijwqAdVnw9Yb9UZLs8NpDglBDb416M5a+PY1wHtEFr3PwSiTvIllXXu3Xm6nXvMuoxSTYwlXLSy6P74/Bh5JbjNK57/LQ7lKJ9mCjobo4nm1ODlN7LL/DWEvXWEo9YQ8fjUaEigGz68zQe/tIGHItGB7xNFnOelp1QGr4zdcEvc0Fjs5WmqCgrkEQ6aJ6QKAY4UEjjGndhwkXZglC/ZN2AFdIij0Cl0hx+o5daMckVsQo5jB7BBgv pmilic@Petars-MacBook-Pro.local"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAID9ekhrL1FzCFemwd4g7J199V6cM4kf5FCGZ09txRQEV josip@agi.cash"
  ];

  services.maxplayer.relay = {
    enable = true;

    # NIP-11 / RELAY_URL the relay advertises. buzz derives its NIP-11 document from this URL; the old
    # strfry `info.{name,description}` fields have no buzz-module equivalent yet — if a custom relay
    # name matters, wiring it is a buzz-source ask (see relay OPEN ITEMS in the deploy block).
    #
    # NOTE (merge of #402 into the post-rename main): main's strfry module took a `namespaceTag`
    # ("mobee"→"maxplayer", #464/#467) to filter writes by the #t tag. buzz has NO such option — it
    # admits by KIND, not by t tag — so `namespaceTag` is deliberately dropped here rather than
    # carried over. It is not an oversight and it does not weaken acceptance: the t-tag flip is now
    # purely a client-side concern. Setting it would be a nix eval error against the buzz module.
    relayUrl = "wss://relay.maxplayer.ai";

    # Public marketplace posture: allow UNAUTHENTICATED reads so the keyless web observatory and any
    # account-less client can read events (matches skill.md's "readable by anyone without an account").
    # This opens the READ path only — writes stay gated by NIP-42. Wires BUZZ_OPEN_READ=true.
    openRead = true;

    # Relay identity key. Referenced by PATH only — gudnuf places a file here containing
    #   BUZZ_RELAY_PRIVATE_KEY=<64-hex>
    # before the first switch. It must PERSIST across reboots (the relay's stable NIP-42/NIP-11
    # identity), so it lives on the root EBS volume, not tmpfs. The preflight refuses to start the
    # relay if this file is missing or empty. Nothing here is a credential; the material is on the box.
    privateKeyFile = "/var/lib/secrets/buzz-relay.env";

    # Git-CAS + media object store: REAL AWS S3, reached via the box's instance IAM role — no static
    # creds (the module sets BUZZ_S3_ACCESS_KEY/SECRET_KEY empty to select the AWS credential chain →
    # IMDS). The role MUST grant Get/Put/List/DeleteObject on this bucket (buzz's storage_sweep prunes,
    # hence Delete); s3Region MUST match the bucket's real region. buzz-relay runs a FATAL git
    # object-store conformance probe against this bucket at boot, so the bucket must exist and the role
    # be attached BEFORE the first switch (the preflight HeadBuckets it and fails loud otherwise).
    s3Bucket = "maxplayer-relay-media";
    s3Region = "us-east-1";

    # Off-box backup — the Postgres event log only, shipped to S3 by the instance IAM role
    # (maxplayer-relay-backups). The git-CAS + media objects are already in S3 (s3Bucket above),
    # versioned and off-box, so they are not re-dumped. No static credentials on the box; the module
    # mandates a destination + uploadCommand, so there is no "skip backup" here.
    backup = {
      destination = "s3://maxplayer-relay-backup/launch";
      # Invoked once per artifact with $FILE (local path) and $KEY (destination suffix) in env.
      uploadCommand = ''${pkgs.awscli2}/bin/aws s3 cp "$FILE" "$DESTINATION/$KEY"'';
      # null by design: the EC2 instance's IAM role (maxplayer-relay-backups) grants S3 write, so there
      # is no static credentials file on the box.
      environmentFile = null;
    };
  };

  # The relay binds 127.0.0.1:3000 (loopback, no public bind), so something must terminate TLS and proxy
  # wss -> the relay or it is unreachable. This is the batteries-included default, and it deliberately
  # reuses the existing nginx + ACME cert for relay.maxplayer.ai across the strfry->buzz swap — only the
  # upstream port changes (7777 -> 3000), so there is no cert re-issue or TLS cutover. Public domain
  # relay.maxplayer.ai, ACME contact below. If TLS is terminated elsewhere (Cloudflare, an ALB), drop
  # this nginx/acme/firewall trio and point that proxy at 127.0.0.1:3000 instead.
  services.nginx = {
    enable = true;
    recommendedProxySettings = true; # sets X-Forwarded-For, which the relay reads as realIpHeader.
    recommendedTlsSettings = true;
    virtualHosts."relay.maxplayer.ai" = {
      # DNS: relay.maxplayer.ai → 34.225.223.145 (live); ACME issues on first switch.
      enableACME = true;
      forceSSL = true;
      locations."/" = {
        proxyPass = "http://127.0.0.1:3000";
        proxyWebsockets = true;
      };

      # Large-body paths. The vhost otherwise inherits the nixpkgs `clientMaxBodySize` default of
      # 10m, which silently 413s git pushes and media uploads that the relay itself would accept
      # (BUZZ_GIT_MAX_PACK_BYTES defaults to 500 MiB in crates/buzz .../config.rs). Raised
      # PER-LOCATION, deliberately not vhost-wide: with proxy_request_buffering on, nginx spools the
      # entire body to disk before it consults the relay, so a global raise costs 512 MiB of disk
      # write per anonymous request on ANY path — measured: a 9 MiB POST to a nonexistent route
      # uploaded all 9,437,184 bytes.
      #
      # 512m is intentional and is NOT a round-up to "just over 500": 512 MiB = 536,870,912 B sits
      # 12 MiB above the relay's own 524,288,000 B ceiling so the RELAY's limit binds first. That
      # matters because the relay logs and meters its own rejections and nginx does not — an nginx
      # 413 is nearly invisible, a relay rejection is observable. Do not "tidy" this to 500m.
      #
      # proxy_request_buffering off is the load-bearing line, not the cap: without it nginx buffers
      # the whole pack to disk before the relay sees a byte.
      #
      # Regex locations must re-declare proxyPass — Nix does not inherit it from locations."/", and a
      # regex location outranks the "/" prefix match, so omitting it would stop proxying these paths
      # entirely. Both blocks keep proxyPass so recommendedProxySettings still injects the
      # proxy_set_header lines the relay reads as realIpHeader.
      locations."~ ^/git/" = {
        proxyPass = "http://127.0.0.1:3000";
        extraConfig = ''
          client_max_body_size 512m;
          proxy_request_buffering off;
          proxy_read_timeout 3600s;
          proxy_send_timeout 3600s;
        '';
      };
      # Matches the relay's two upload routes exactly (see upload_route_mode in
      # crates/buzz/crates/buzz-relay/src/api/media.rs): /upload and the legacy /media/upload.
      locations."~ ^/(upload|media/upload)$" = {
        proxyPass = "http://127.0.0.1:3000";
        extraConfig = ''
          client_max_body_size 512m;
          proxy_request_buffering off;
          proxy_read_timeout 3600s;
        '';
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
