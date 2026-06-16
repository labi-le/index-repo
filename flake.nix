{
  description = "index-repo: fast semantic code indexer for ChromaDB";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };

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
      # Rust package
      # ---------------------------------------------------------------------------
      indexRepo = pkgs.rustPlatform.buildRustPackage {
        pname = "index-repo";
        version = "0.1.0";
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;

        # ort uses load-dynamic — no onnxruntime link at build time.
        # tree-sitter grammar crates compile C via cc (provided by stdenv.cc).
        nativeBuildInputs = [ pkgs.makeWrapper ];

        # Wrap the binary so it finds onnxruntime and the model at runtime.
        postInstall = ''
          wrapProgram $out/bin/index-repo \
            --set ORT_DYLIB_PATH ${pkgs.onnxruntime}/lib/libonnxruntime.so \
            --set INDEX_REPO_MODEL_DIR ${model}
        '';

        # cargo test passes in the sandbox:
        #   - embed test skips without ORT_DYLIB_PATH / INDEX_REPO_MODEL_DIR
        #   - all other tests use only tmpfs + in-memory state
        doCheck = true;

        meta = with pkgs.lib; {
          description = "Fast semantic code indexer for ChromaDB (tree-sitter + fastembed)";
          license = licenses.mit;
          platforms = [ "x86_64-linux" ];
        };
      };

    in {
      packages.${system} = {
        default    = indexRepo;
        index-repo = indexRepo;
        model      = model;
      };

      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = [
          pkgs.cargo
          pkgs.rustc
          pkgs.rustfmt
          pkgs.clippy
          pkgs.pkg-config
        ];
        # Runtime env so `cargo test embed::` and the binary work from the devShell.
        ORT_DYLIB_PATH     = "${pkgs.onnxruntime}/lib/libonnxruntime.so";
        INDEX_REPO_MODEL_DIR = "${model}";
      };
    };
}
