{
  description = "Maxplayer";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};

          # Args common to every `maxplayer` build.
          maxplayerArgs = {
            pname = "maxplayer";
            version = "0.1.1";
            src = self;

            # Vendor all dependencies hermetically from the committed
            # Cargo.lock. No network access is needed at build time.
            cargoLock.lockFile = ./Cargo.lock;

            # Workspace repo: build/install only the `maxplayer` package (its binary is also `maxplayer`).
            cargoBuildFlags = [
              "-p"
              "maxplayer"
            ];

            # The flake's job is packaging the runnable binary, not running
            # the test suite (some tests are heavy / touch the network).
            doCheck = false;

            meta = {
              description = "MAXPLAYER AI";
              mainProgram = "maxplayer";
            };
          };

          # Fully static buyer binary from any static package set. Links against
          # musl with no ELF interpreter, so the artifact runs on any Linux
          # without nix present — the property a downloaded release needs.
          #
          # Buyer surface only: `acp` is left out, so the seller's
          # agent-execution path is not compiled in.
          buyerStatic =
            staticPkgs:
            staticPkgs.rustPlatform.buildRustPackage (
              maxplayerArgs
              // {
                nativeBuildInputs = [ staticPkgs.pkg-config ];
              }
            );
        in
        {
          default = pkgs.rustPlatform.buildRustPackage (
            maxplayerArgs
            // {
              # Enable the `acp` feature (off by default) so the acp-gated
              # `run` subcommand is compiled in. Default features (wallet)
              # are kept.
              buildFeatures = [ "acp" ];

              nativeBuildInputs = [ pkgs.pkg-config ];
            }
          );

          # The strfry write-policy plugin (crate `maxplayer-relay-write-policy`, binary
          # `maxplayer-write-policy`). A separate derivation, not a `maxplayerArgs` variant: it builds only
          # this one workspace crate — serde/serde_json, no C deps — so it needs neither the `acp`
          # feature nor pkg-config, and must not pull in the egui/sqlite/libgit2 members. Deps still
          # vendor from the one workspace `Cargo.lock`; `-p` keeps the compile to this crate.
          # `lib.getExe` in `nixosModules.relay` resolves `meta.mainProgram`.
          relay-write-policy = pkgs.rustPlatform.buildRustPackage {
            pname = "maxplayer-relay-write-policy";
            version = "0.1.1";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;
            cargoBuildFlags = [
              "-p"
              "maxplayer-relay-write-policy"
            ];
            doCheck = false;
            meta.mainProgram = "maxplayer-write-policy";
          };
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # For the system doing the building.
          buyer-static = buyerStatic pkgs.pkgsStatic;

          # aarch64, cross-built. The wrapper here targets aarch64 and carries
          # an aarch64 musl libc, which is what keeps the C dependencies
          # (bundled SQLite, vendored libgit2) building rather than picking up
          # the host's headers.
          buyer-static-aarch64 = buyerStatic pkgs.pkgsCross.aarch64-multiplatform-musl.pkgsStatic;
        }
      );

      # NixOS module for the launch relay: `services.maxplayer.relay`, a strfry relay whose write
      # policy enforces a single namespace and is born empty. The host repo imports this and wires
      # the plugin as `writePolicyPackage = <this flake>.packages.<system>.relay-write-policy`,
      # supplying hostname/hardware/secrets itself. Dot-form on purpose: a sibling `nixosModules.runner`
      # (#280) then merges as its own line rather than a conflicting `nixosModules` block.
      nixosModules.relay = import ./nix/relay.nix;

      # The launch relay as a deployable box: EC2 t3.small (x86_64) built from `nixosModules.relay`.
      # Deploy, building ON the target so nothing cross-compiles from the workstation:
      #   nixos-rebuild switch --flake .#relay --target-host root@<IP> --build-host root@<IP>
      # `nix/relay-host.nix` carries the human-owned config (NIP-11 identity, backup, TLS); the two
      # flake-local references (the module + the write-policy package) are wired here where `self` is in scope.
      nixosConfigurations.relay = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.relay
          ./nix/relay-host.nix
          { services.maxplayer.relay.writePolicyPackage = self.packages.x86_64-linux.relay-write-policy; }
        ];
      };

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/maxplayer";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
            ];
          };
        }
      );
    };
}
