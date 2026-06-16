{
  description = "index-repo: fast semantic code indexer for ChromaDB";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };

      version = "0.1.0";
      pname = "index-repo";

      # ---------------------------------------------------------------------------
      # Model FOD — 5 files from Qdrant/all-MiniLM-L6-v2-onnx on HuggingFace
      # ---------------------------------------------------------------------------
      model = pkgs.runCommand "all-MiniLM-L6-v2-onnx" { } ''
        mkdir -p $out
        cp ${pkgs.fetchurl {
          url = "https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/resolve/main/model.onnx";
          hash = "sha256-u9e0ZvbVjmRv3CvV/Wey9ek8C2hwEb1FSMQg971G8MU=";
        }} $out/model.onnx
        cp ${pkgs.fetchurl {
          url = "https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/resolve/main/tokenizer.json";
          hash = "sha256-2g55kzue1ReYo64niT08X6SiARJs73VYYpbfm00sYqA=";
        }} $out/tokenizer.json
        cp ${pkgs.fetchurl {
          url = "https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/resolve/main/config.json";
          hash = "sha256-G02OKjmIN37YtRmjHY0xAlol8cX4YGmY6AFBEUOO/Nc=";
        }} $out/config.json
        cp ${pkgs.fetchurl {
          url = "https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/resolve/main/special_tokens_map.json";
          hash = "sha256-XVtmLkIeqfrAdRdLsGiO4NlDFpmQC5BmKs1EsqNQUDo=";
        }} $out/special_tokens_map.json
        cp ${pkgs.fetchurl {
          url = "https://huggingface.co/Qdrant/all-MiniLM-L6-v2-onnx/resolve/main/tokenizer_config.json";
          hash = "sha256-vS4GpbIP0bE8qYi+3Idj0zLSQjgbT7yY+P6tRSQVj3k=";
        }} $out/tokenizer_config.json
      '';

      # ---------------------------------------------------------------------------
      # Pre-built binary fetched from GitHub Releases
      # update-flake workflow patches version + hash automatically on each release
      # ---------------------------------------------------------------------------
      indexRepo = pkgs.stdenv.mkDerivation {
        inherit pname version;

        src = pkgs.fetchurl {
          url = "https://github.com/labi-le/index-repo/releases/download/v${version}/index-repo_linux_amd64";
          hash = "sha256-VOBD4fVE4gOpUYZDDa1FR3k1Nb3nzQdiIrh0lgk/BDA="; # x86_64-linux
        };

        dontUnpack = true;

        nativeBuildInputs = [ pkgs.makeWrapper ];

        installPhase = ''
          mkdir -p $out/bin
          cp $src $out/bin/${pname}
          chmod +x $out/bin/${pname}
        '';

        # Wrap the binary so it finds onnxruntime and the model at runtime.
        postFixup = ''
          wrapProgram $out/bin/${pname} \
            --set ORT_DYLIB_PATH ${pkgs.onnxruntime}/lib/libonnxruntime.so \
            --set INDEX_REPO_MODEL_DIR ${model}
        '';

        meta = with pkgs.lib; {
          description = "Fast semantic code indexer for ChromaDB (tree-sitter + fastembed)";
          license = licenses.mit;
          platforms = [ "x86_64-linux" ];
        };
      };

      # ---------------------------------------------------------------------------
      # Source build — compiles the raw binary. Used by CI (release.yaml) to
      # produce the release artifact that `indexRepo` (above) later fetches.
      # Intentionally UNWRAPPED: the artifact must be a portable ELF; the
      # prebuilt `index-repo` package wraps it with ORT_DYLIB_PATH + model.
      # ---------------------------------------------------------------------------
      fromSource = pkgs.rustPlatform.buildRustPackage {
        inherit pname version;
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        doCheck = true;

        meta = with pkgs.lib; {
          description = "Fast semantic code indexer for ChromaDB (built from source)";
          license = licenses.mit;
          platforms = [ "x86_64-linux" ];
        };
      };

    in {
      packages.${system} = {
        default    = indexRepo;
        index-repo = indexRepo;
        fromSource = fromSource;
        model      = model;
      };

      # NixOS module: services.index-repo.{enable,package,host,port,ssl,debounce,tuneInotify}
      nixosModules.default = import ./nix/nixos-module.nix self;

      # Home Manager module: opencode <-> index-repo glue (services.index-repo.opencode.*)
      homeManagerModules.default = import ./nix/hm-module.nix self;

      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = [
          pkgs.cargo
          pkgs.rustc
          pkgs.rustfmt
          pkgs.clippy
          pkgs.pkg-config
        ];
        # Runtime env so `cargo test embed::` and the binary work from the devShell.
        ORT_DYLIB_PATH       = "${pkgs.onnxruntime}/lib/libonnxruntime.so";
        INDEX_REPO_MODEL_DIR = "${model}";
      };
    };
}
