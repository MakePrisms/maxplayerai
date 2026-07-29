{
  description = "Mobee";

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

          # Args common to every `mobee` build.
          mobeeArgs = {
            pname = "mobee";
            version = "0.1.0";
            src = self;

            # Vendor all dependencies hermetically from the committed
            # Cargo.lock. No network access is needed at build time.
            cargoLock.lockFile = ./Cargo.lock;

            # Workspace repo: build/install only the `mobee` package, whose binary is `maxplayer`.
            cargoBuildFlags = [
              "-p"
              "mobee"
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
              mobeeArgs
              // {
                nativeBuildInputs = [ staticPkgs.pkg-config ];
              }
            );
        in
        {
          default = pkgs.rustPlatform.buildRustPackage (
            mobeeArgs
            // {
              # Enable the `acp` feature (off by default) so the acp-gated
              # `run` subcommand is compiled in. Default features (wallet)
              # are kept.
              buildFeatures = [ "acp" ];

              nativeBuildInputs = [ pkgs.pkg-config ];
            }
          );
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
