{ pkgs, lib, config, ... }:

{
  packages = with pkgs; [
    pkg-config
    openssl
    libgit2
    sqlite
    cargo-edit
  ];

  languages.rust = {
    enable = true;
    components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-analyzer" ];
  };

  enterShell = ''
    export PATH="./target/debug:./target/release:$PATH"
  '';
}
