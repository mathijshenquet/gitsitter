{
  description = "gitsitter - Keep local branches in sync with remotes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "gitsitter";
          version = "0.1.0";
          src = ./.;
          # cargoHash = "sha256-b3rxa+O0n1ClrLrjQoDYjWFup0uhkmMHQjh9/GXJzOk="; # TODO: update after first cargo build generates Cargo.lock
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          buildInputs = with pkgs; [
            openssl
            libgit2
            sqlite
          ];
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
            pkg-config
            openssl
            libgit2
            sqlite
          ];

          shellHook = ''
            export PATH="./target/debug:./target/release:$PATH"
          '';
        };
      }
    );
}
