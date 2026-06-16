self:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.index-repo;
in
{
  options.services.index-repo = {
    enable = lib.mkEnableOption "index-repo shared semantic code indexer service";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "index-repo.packages.\${system}.default";
      description = "The index-repo package to run (already wrapped with ORT_DYLIB_PATH + INDEX_REPO_MODEL_DIR).";
    };

    host = lib.mkOption {
      type = lib.types.str;
      default = "192.168.1.2";
      description = "ChromaDB host the indexer writes to.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 8000;
      description = "ChromaDB port.";
    };

    ssl = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Use HTTPS to reach ChromaDB.";
    };

    debounce = lib.mkOption {
      type = lib.types.ints.unsigned;
      default = 800;
      description = "Filesystem-event debounce window in milliseconds.";
    };

    tuneInotify = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Raise fs.inotify limits for watching many large repositories.
        Leave off on NixOS-unstable, which already ships
        fs.inotify.max_user_watches = 524288 and max_user_instances = 524288.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.index-repo = {
      description = "Code indexer (shared singleton)";
      after = [ "basic.target" ];
      wantedBy = [ "default.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = lib.concatStringsSep " " (
          [
            "${cfg.package}/bin/index-repo serve"
            "--host ${cfg.host}"
            "--port ${toString cfg.port}"
            "--debounce ${toString cfg.debounce}"
          ]
          ++ lib.optional cfg.ssl "--ssl"
        );
        Restart = "on-failure";
        RestartSec = 3;
      };
    };

    boot.kernel.sysctl = lib.mkIf cfg.tuneInotify {
      "fs.inotify.max_user_watches" = lib.mkDefault 524288;
      "fs.inotify.max_user_instances" = lib.mkDefault 1024;
    };
  };
}
