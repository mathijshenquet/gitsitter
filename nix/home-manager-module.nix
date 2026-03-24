{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.gitsitter;
  tomlFormat = pkgs.formats.toml { };
  bashHook = builtins.readFile ../src/embed/bash_hook.sh;
  zshHook = builtins.readFile ../src/embed/zsh_hook.zsh;
  fishHook = builtins.readFile ../src/embed/fish_hook.fish;
  defaultSettings = builtins.fromTOML (builtins.readFile ../config/default-config.toml);
  renderedSettings = lib.recursiveUpdate defaultSettings cfg.settings;
in
{
  options.services.gitsitter = {
    enable = lib.mkEnableOption "gitsitter";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.default;
      defaultText = lib.literalExpression "self.packages.${pkgs.system}.default";
      description = "The gitsitter package to install and run.";
    };

    settings = lib.mkOption {
      type = tomlFormat.type;
      default = { };
      description = ''
        Declarative contents of `~/.config/gitsitter/config.toml`.
        This is merged over the module's default gitsitter settings.
      '';
    };

    shellIntegration = {
      bash = lib.mkEnableOption "bash integration";
      zsh = lib.mkEnableOption "zsh integration";
      fish = lib.mkEnableOption "fish integration";
    };

    systemd.enable = lib.mkOption {
      type = lib.types.bool;
      default = pkgs.stdenv.isLinux;
      description = "Whether to run gitsitter as a user systemd service.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."gitsitter/config.toml" = {
      source = tomlFormat.generate "gitsitter-config.toml" renderedSettings;
    };

    systemd.user.services.gitsitter = lib.mkIf cfg.systemd.enable {
      Unit = {
        Description = "gitsitter daemon";
      };
      Service = {
        Type = "exec";
        ExecStart = "${cfg.package}/bin/gitsitter daemon run";
        Restart = "on-failure";
        RestartSec = 5;
      };
      Install = {
        WantedBy = [ "default.target" ];
      };
    };

    programs.bash.initExtra = lib.mkIf (cfg.shellIntegration.bash && config.programs.bash.enable) bashHook;

    programs.zsh.initExtra = lib.mkIf (cfg.shellIntegration.zsh && config.programs.zsh.enable) zshHook;

    programs.fish.interactiveShellInit = lib.mkIf (cfg.shellIntegration.fish && config.programs.fish.enable) fishHook;
  };
}
