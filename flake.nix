{
  description = "Maxplayer";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  # Scoped toolchain source for the vendored buzz relay only (rustc >= 1.94, see maxplayer-relay).
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { self, nixpkgs, rust-overlay }:
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

          # rustc 1.96 for the vendored buzz relay: the recarry branch's floor is >= 1.94 (sqlx 0.9),
          # and stock nixpkgs-25.11 ships ~1.91 which will NOT compile it. Scoped to the relay package
          # via its own rust-overlay toolchain so the other packages keep stock nixpkgs rust untouched.
          buzzRust =
            (import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }).rust-bin.stable."1.96.0".default;
          buzzRustPlatform = pkgs.makeRustPlatform {
            cargo = buzzRust;
            rustc = buzzRust;
          };

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

          # buzz-derived launch relay (crate `buzz-relay`), built from the in-tree vendored source
          # (PR #402). One binary: events + git + payments, mobee-scoped by a compiled kind allowlist.
          # `--bin buzz-relay` guards against a second maintenance bin the crate may ship.
          #
          # Buzz is vendored as an ISOLATED nested workspace under crates/buzz/ (its OWN Cargo.toml +
          # Cargo.lock, resolver 2 / edition 2021, EXCLUDED from the mp workspace), so build from that
          # subtree with buzz's own lock. Nested isolation leaves the mp lock untouched and sidesteps
          # cross-workspace dep reconciliation.
          #
          # Toolchain = buzzRustPlatform (rustc 1.96): the recarry branch needs >= 1.94 (sqlx 0.9) and
          # stock nixpkgs-25.11 (~1.91) will not compile it. nativeBuildInputs = pkg-config + cmake
          # (cmake builds aws-lc-sys, pulled by buzz's [patch.crates-io] aws-creds fork that the vendor
          # preserves; the recarry branch links aws-lc, so there is no openssl input).
          #
          # cargoHash (FOD vendor) rather than cargoLock.lockFile: buzz's own lock carries 40 git deps
          # — the aws-creds [patch.crates-io] fork (a real dep) + 39 mesh-llm/skippy AI crates (dev-deps,
          # vendored but NOT compiled: doCheck is off and they are off buzz-relay's build graph).
          # importCargoLock would need one outputHash per git dep (40 entries); the FOD needs a single
          # hash and is proven to vendor buzz's deps (all PUBLIC). If the vendor later strips the mesh-llm
          # dev-deps from the lock, this can return to hermetic cargoLock + one aws-creds outputHash.
          maxplayer-relay = buzzRustPlatform.buildRustPackage {
            pname = "maxplayer-relay";
            version = "0.1.0";
            src = ./crates/buzz;
            # cargoHash of buzz's full vendored dep set (crates.io + the 2 public git sources:
            # rust-s3/aws-creds build dep + mesh-llm dev-deps). Recompute if crates/buzz/Cargo.lock changes.
            cargoHash = "sha256-C6rquKLY2I2QMRmH9+x4sh5+Eejitjfm8+XdtOaxhi4=";
            cargoBuildFlags = [
              "-p"
              "buzz-relay"
              "--bin"
              "buzz-relay"
            ];
            doCheck = false;
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
            ];
            meta.mainProgram = "buzz-relay";
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

      # NixOS module for the launch relay: `services.maxplayer.relay`, the buzz-derived relay (crate
      # `buzz-relay`) — events + git + payments over a compiled, mobee-scoped kind allowlist, born
      # empty. The host repo imports this and supplies hostname/hardware/secrets. Dot-form on purpose:
      # a sibling `nixosModules.runner` (#280) then merges as its own line rather than a conflicting
      # `nixosModules` block.
      nixosModules.relay = import ./nix/relay.nix;

      # The launch relay as a deployable box: EC2 t3.small (x86_64) built from `nixosModules.relay`.
      # Deploy, building ON the target so nothing cross-compiles from the workstation:
      #   nixos-rebuild switch --flake .#relay --target-host root@<IP> --build-host root@<IP>
      # `nix/relay-host.nix` carries the human-owned config (identity URL, key path, backup, TLS); the
      # flake-local references (the module + the relay package + its schema) are wired here where
      # `self` is in scope.
      # One deployable configuration `.#relay`, built from the in-tree vendored `buzz-relay` crate.
      # (There is deliberately NO fork-pin fallback config: the raw gudnuf/buzz fork carries the older
      # DVM kinds, not the mobee-core set, so it would fail the 3401 verify battery — a path that must
      # never be used should not exist in a runbook. If the vendor PR is not ready, deploy NOTHING; the
      # swap waits and strfry keeps serving — the tag never depended on the swap.)
      nixosConfigurations.relay = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.relay
          ./nix/relay-host.nix
          {
            services.maxplayer.relay.package = self.packages.x86_64-linux.maxplayer-relay;
          }
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
