{
  description = "gitsitter - Keep local branches in sync with remotes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      eachDefaultSystem = flake-utils.lib.eachDefaultSystem;
      mkPackage = pkgs: pkgs.rustPlatform.buildRustPackage {
        pname = "gitsitter";
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
        src = ./.;
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
    in
    eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = mkPackage pkgs;

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
      }) // {
        homeManagerModules.default = import ./nix/home-manager-module.nix { inherit self; };
      };
}
