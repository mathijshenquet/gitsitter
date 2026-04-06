{ self }:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.gitsitter;
  tomlFormat = pkgs.formats.toml { };
  bashHook = builtins.readFile ../src/embed/bash_hook.sh;
  zshHook = builtins.readFile ../src/embed/zsh_hook.zsh;
  fishHook = builtins.readFile ../src/embed/fish_hook.fish;
  defaultSettings = builtins.fromTOML (builtins.readFile ../config/default-config.toml);
  resolveAgentSettings = lib.optionalAttrs cfg.resolveAgent.enable {
    global = {
      on_conflict = "auto";
      resolve_agent = cfg.resolveAgent.tool;
    };
  };
  renderedSettings = lib.recursiveUpdate
    (lib.recursiveUpdate defaultSettings resolveAgentSettings)
    cfg.settings;
  daemonPathPackages = [ pkgs.git ]
    ++ lib.optional cfg.githubIntegration.enable pkgs.gh
    ++ lib.optional (cfg.resolveAgent.enable && cfg.resolveAgent.package != null) cfg.resolveAgent.package;
  daemonPath = lib.makeBinPath daemonPathPackages;
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

    launchd.enable = lib.mkOption {
      type = lib.types.bool;
      default = pkgs.stdenv.isDarwin;
      description = "Whether to run gitsitter as a launchd user agent.";
    };

    githubIntegration.enable = lib.mkEnableOption "GitHub integration (relaxed ownership via gh CLI)";

    resolveAgent = {
      enable = lib.mkEnableOption "AI-assisted conflict resolution";
      tool = lib.mkOption {
        type = lib.types.str;
        default = "claude";
        description = "Which resolve agent to use (e.g. \"claude\").";
      };
      package = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Resolve agent package. If null, assumes the tool binary is on PATH.";
      };
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."gitsitter/config.toml" = {
      source = tomlFormat.generate "gitsitter-config.toml" renderedSettings;
    };

    # Keep in sync with src/embed/gitsitter.service
    systemd.user.services.gitsitter = lib.mkIf cfg.systemd.enable {
      Unit = {
        Description = "gitsitter daemon";
      };
      Service = {
        Type = "exec";
        ExecStart = "${cfg.package}/bin/gitsitter daemon run";
        Restart = "on-failure";
        RestartSec = 5;
        Environment = [
          "PATH=${daemonPath}"
        ];
      };
      Install = {
        WantedBy = [ "default.target" ];
      };
    };

    # Keep in sync with src/embed/com.gitsitter.daemon.plist
    launchd.agents.gitsitter = lib.mkIf cfg.launchd.enable {
      enable = true;
      config = {
        Label = "com.gitsitter.daemon";
        ProgramArguments = [ "${cfg.package}/bin/gitsitter" "daemon" "run" ];
        RunAtLoad = true;
        KeepAlive = {
          SuccessfulExit = false;
        };
        StandardOutPath = "${config.xdg.stateHome}/gitsitter/gitsitter.out.log";
        StandardErrorPath = "${config.xdg.stateHome}/gitsitter/gitsitter.err.log";
        EnvironmentVariables = {
          PATH = daemonPath;
        };
      };
    };

    programs.bash.initExtra = lib.mkIf (cfg.shellIntegration.bash && config.programs.bash.enable) bashHook;

    programs.zsh.initExtra = lib.mkIf (cfg.shellIntegration.zsh && config.programs.zsh.enable) zshHook;

    programs.fish.interactiveShellInit = lib.mkIf (cfg.shellIntegration.fish && config.programs.fish.enable) fishHook;
  };
}
