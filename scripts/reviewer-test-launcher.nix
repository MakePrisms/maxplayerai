# Build the real NixOS reviewer launcher with only the worker executable stubbed.
# nix build --extra-experimental-features 'nix-command flakes' --impure \
#   --expr 'import ./scripts/reviewer-test-launcher.nix' --no-link --print-out-paths
# python3 scripts/test-reviewer-state-path.py <printed-launcher-path>
let
  flake = builtins.getFlake (toString ../.);
  host = flake.nixosConfigurations.relay.extendModules {
    modules = [ ({ lib, pkgs, ... }: {
      services.maxplayer.reviewer.package = lib.mkForce
        (pkgs.writeShellScriptBin "maxplayer" "exit 0");
    }) ];
  };
in host.config.systemd.services.maxplayer-reviewer.serviceConfig.ExecStart
