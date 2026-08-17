{
  description = "Ale, My Eyes! desktop assistant";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      lib = nixpkgs.lib;
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = lib.genAttrs systems;
      mkPkgs = system: import nixpkgs { inherit system; };
      mkPackage = system:
        let
          pkgs = mkPkgs system;
          runtimeLibraries = with pkgs; [
            alsa-lib
            dbus
            fontconfig
            libGL
            libgbm
            libxkbcommon
            mesa
            pipewire
            speechd
            wayland
            xorg.libX11
            xorg.libXcursor
            xorg.libXext
            xorg.libXfixes
            xorg.libXi
            xorg.libXrandr
            xorg.libxcb
          ];
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "ale-my-eyes";
          version = "0.1.0";
          src = lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                relative = lib.removePrefix (toString ./. + "/") (toString path);
                ignored = [
                  "target"
                  "dist"
                  "release"
                  "Ale, My Eyes!.app"
                  "AleMyEyes.dmg"
                  "ale-my-eyes-linux"
                  "ale-my-eyes-windows"
                ];
              in
              !(lib.any (prefix: relative == prefix || lib.hasPrefix (prefix + "/") relative) ignored)
              && !(lib.hasSuffix ".zip" relative || lib.hasSuffix ".tar.gz" relative);
          };
          cargoLock.lockFile = ./Cargo.lock;
          strictDeps = true;
          nativeBuildInputs = with pkgs; [ clang makeWrapper pkg-config ];
          buildInputs = runtimeLibraries;
          LIBCLANG_PATH = "${lib.getLib pkgs.llvmPackages.libclang}/lib";
          cargoBuildFlags = [ "--workspace" ];
          cargoTestFlags = [ "--workspace" ];
          preCheck = ''
            export HOME="$TMPDIR"
          '';
          installPhase = ''
            runHook preInstall
            install -Dm755 target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release/ale-cli "$out/bin/ale-cli"
            install -Dm755 target/${pkgs.stdenv.hostPlatform.rust.rustcTarget}/release/ale-gui "$out/bin/ale-gui"
            wrapProgram "$out/bin/ale-gui" \
              --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibraries}
            runHook postInstall
          '';
          meta = {
            description = "Accessible voice and visual desktop assistant";
            homepage = "https://github.com/Risaly-Noroki-Dev-Club/ale-my-eyes-desktop";
            license = lib.licenses.mit;
            mainProgram = "ale-gui";
            platforms = systems;
          };
        };
    in
    {
      packages = forAllSystems (system: {
        default = mkPackage system;
        ale-my-eyes = self.packages.${system}.default;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/ale-gui";
        };
        cli = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/ale-cli";
        };
      });

      devShells = forAllSystems (system:
        let pkgs = mkPkgs system;
        in {
          default = pkgs.mkShell {
            packages = with pkgs; [ cargo clang clippy llvmPackages.libclang pkg-config rustc rustfmt ] ++
              self.packages.${system}.default.buildInputs;
            LIBCLANG_PATH = "${lib.getLib pkgs.llvmPackages.libclang}/lib";
          };
        });

      nixosModules.default = { config, lib, pkgs, ... }:
        let cfg = config.programs.ale-my-eyes;
        in {
          options.programs.ale-my-eyes = {
            enable = lib.mkEnableOption "Ale, My Eyes! desktop assistant";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.default;
              defaultText = lib.literalExpression "ale-my-eyes.packages.\${pkgs.system}.default";
              description = "Ale, My Eyes! package to install.";
            };
          };
          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ cfg.package ];
          };
        };

      checks = forAllSystems (system:
        let
          pkgs = mkPkgs system;
          package = self.packages.${system}.default;
          evaluated = lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              { programs.ale-my-eyes.enable = true; }
            ];
          };
          moduleInstallsPackage = builtins.elem package evaluated.config.environment.systemPackages;
        in {
          package = package;
          nixos-module = pkgs.runCommand "ale-my-eyes-nixos-module-check" { } ''
            test "${if moduleInstallsPackage then "yes" else "no"}" = yes
            touch "$out"
          '';
        });
    };
}
