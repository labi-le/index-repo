self:
{
  config,
  lib,
  pkgs,
  osConfig ? null,
  ...
}:

let
  cfg = config.services.index-repo;
  ocfg = cfg.opencode;
  # Single source of truth for the ChromaDB endpoint: read the NixOS index-repo
  # service when it is in scope (integrated home-manager), otherwise fall back to
  # the nixos-module defaults. Lets `chromaMcp` track the indexer's host/port/ssl
  # without restating them.
  osIndex = if osConfig != null then (osConfig.services.index-repo or { }) else { };
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

      chromaGate.enable = lib.mkEnableOption ''
        the chroma-gate opencode plugin. It injects a system rule telling agents to
        call `chroma_query_documents` first and blocks unscoped grep/glob (for a
        fixed set of agents) until a chroma query has run in the session. The
        enforced collection name is derived at runtime as
        `code-<owner>-<repo>` (from the git remote, else `code-<basename>-<hash8>`), matching this indexer's naming'';

      chromaMcp = {
        enable = lib.mkEnableOption ''
          the `chroma` MCP server in opencode, pointed at the ChromaDB that backs
          this indexer. Requires `uvx` (from the `uv` package) on PATH to run
          `chroma-mcp`'';

        host = lib.mkOption {
          type = lib.types.str;
          default = osIndex.host or "127.0.0.1";
          defaultText = lib.literalExpression ''osConfig.services.index-repo.host or "127.0.0.1"'';
          description = "ChromaDB host the chroma MCP connects to (defaults to the NixOS index-repo service host).";
        };

        port = lib.mkOption {
          type = lib.types.port;
          default = osIndex.port or 8000;
          defaultText = lib.literalExpression "osConfig.services.index-repo.port or 8000";
          description = "ChromaDB port the chroma MCP connects to (defaults to the NixOS index-repo service port).";
        };

        ssl = lib.mkOption {
          type = lib.types.bool;
          default = osIndex.ssl or false;
          defaultText = lib.literalExpression "osConfig.services.index-repo.ssl or false";
          description = "Whether the chroma MCP connects to ChromaDB over TLS (defaults to the NixOS index-repo service ssl).";
        };
      };
    };
  };

  config = lib.mkMerge [
    (lib.mkIf ocfg.chromaGate.enable {
      xdg.configFile."opencode/plugins/chroma-gate.ts".source =
        "${self}/hooks/opencode/chroma-gate.ts";
    })

    (lib.mkIf ocfg.chromaMcp.enable {
      programs.opencode.settings.mcp.chroma = {
        type = "local";
        command = [
          "uvx"
          "chroma-mcp"
          "--client-type"
          "http"
          "--host"
          ocfg.chromaMcp.host
          "--port"
          (toString ocfg.chromaMcp.port)
          "--ssl"
          (lib.boolToString ocfg.chromaMcp.ssl)
        ];
        timeout = 30000;
        enabled = true;
      };
    })
  ];
}
