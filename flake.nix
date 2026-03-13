{
  description = "Giggity tmux developer dashboard";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.05";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        lib = pkgs.lib;
        rustToolchain = pkgs.rust-bin.stable.latest.default;
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };
        src = lib.cleanSourceWith {
          src = ./.;
          filter =
            path: type:
            let
              root = toString ./.;
              pathString = toString path;
              base = baseNameOf pathString;
            in
            !(lib.hasPrefix "${root}/target" pathString)
            && !(lib.hasPrefix "${root}/mutants.out" pathString)
            && pathString != "${root}/scripts/giggity"
            && base != ".DS_Store";
        };

        giggity = rustPlatform.buildRustPackage {
          pname = "giggity";
          version = "0.1.0";
          inherit src;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          cargoBuildFlags = [ "-p" "giggity" ];
          cargoTestFlags = [ "--workspace" ];

          nativeBuildInputs = with pkgs; [
            installShellFiles
            pkg-config
          ];

          buildInputs = lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];

          doCheck = true;

          postInstall = ''
            install -Dm755 giggity.tmux $out/share/tmux-plugins/giggity/giggity.tmux
            cp -R scripts $out/share/tmux-plugins/giggity/
            cp -R examples $out/share/tmux-plugins/giggity/
            cp README.md $out/share/tmux-plugins/giggity/README.md
          '';

          meta = with lib; {
            description = "System-wide tmux dashboard for containers and local services";
            homepage = "https://github.com/jahquan/giggity";
            license = licenses.mit;
            mainProgram = "giggity";
            platforms = platforms.unix;
          };
        };
      in
      {
        packages.default = giggity;
        packages.giggity = giggity;

        apps.default = {
          type = "app";
          program = "${giggity}/bin/giggity";
        };

        checks = {
          build = giggity;

          shell = pkgs.runCommand "giggity-shellcheck"
            {
              nativeBuildInputs = with pkgs; [
                shellcheck
                shfmt
              ];
            }
            ''
              cd ${src}
              shellcheck giggity.tmux scripts/*.sh
              shfmt -d giggity.tmux scripts/*.sh
              touch $out
            '';
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            cargo-llvm-cov
            cargo-mutants
            clippy
            pkg-config
            rust-analyzer
            shellcheck
            shfmt
            rustToolchain
          ];

          inputsFrom = [ giggity ];
        };
      }
    );
}
