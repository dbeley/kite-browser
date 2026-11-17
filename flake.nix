{
  description = "kite — a keyboard-first browser shell on the Chromium engine, built for slow machines";

  # Deliberately follows the local nixpkgs registry entry so `nix build` works
  # offline and on machines without flake-lock refreshes. Pin it for CI:
  #   nix flake lock --update-input nixpkgs
  inputs.nixpkgs.url = "nixpkgs";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/package.nix { };
        kite = self.packages.${pkgs.system}.default;
      });

      overlays.default = final: prev: {
        kite = final.callPackage ./nix/package.nix { };
      };

      nixosModules.default = {
        imports = [ ./nix/module.nix ];
        # The flake knows where its own package lives, and the module stays
        # generic. An overlay (not a pinned `programs.kite.package`) is what lets
        # `programs.kite.engine` rebuild the package around another engine.
        nixpkgs.overlays = [ self.overlays.default ];
      };
      nixosModules.kite = self.nixosModules.default;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          name = "kite-dev";
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            pkg-config
            # runtime + verification tooling. Both engines: ungoogled-chromium is
            # what the package ships, chromium is the fallback / A/B baseline.
            chromium
            ungoogled-chromium
            xvfb
            xdotool
            imagemagick
            i3
            wmctrl
            xprop
            xclip
            jq
          ];
          RUST_BACKTRACE = "1";
          # Exercise the shipped default in the harness and in CI.
          KITE_ENGINE = pkgs.lib.getExe pkgs.ungoogled-chromium;
          shellHook = ''
            echo "kite dev shell"
            echo "  cargo build --release        # build the controller"
            echo "  ./dev/harness.sh             # headless GUI smoke test (Xvfb + i3)"
            echo "  KITE_ENGINE=$(command -v chromium) ./dev/harness.sh   # A/B on plain chromium"
          '';
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixpkgs-fmt);
    };
}
