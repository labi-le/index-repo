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
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "index-repo.packages.\${system}.default";
      description = "The index-repo package providing the register/unregister CLI.";
    };

    opencode = {
      hook = lib.mkOption {
        type = lib.types.str;
        readOnly = true;
        description = ''
          Shell snippet to splice into an opencode launcher before exec.
          When the cwd is a git repo (and not opted out), it starts the
          shared index-repo user service, registers the cwd, and installs a
          trap to unregister it on exit. Reference it as
          `config.services.index-repo.opencode.hook`.
        '';
        default = ''
          if [ -z "''${CODE_INDEXER_ACTIVE:-}" ] && [ -z "''${CODE_INDEXER_DISABLE:-}" ] && [ -d "$PWD/.git" ] && [ ! -e "$PWD/.no-code-index" ]; then
            export CODE_INDEXER_ACTIVE=1
            ${pkgs.systemd}/bin/systemctl --user start --no-block index-repo.service 2>/dev/null || true
            ${cfg.package}/bin/index-repo register "$PWD" --pid $$ 2>/dev/null || true
            trap '${cfg.package}/bin/index-repo unregister "$PWD" --pid $$ 2>/dev/null || true' EXIT INT TERM
          fi
        '';
      };
    };
  };
}
