{ config, lib, pkgs, ... }:

let
  cfg = config.programs.kite;
  toml = pkgs.formats.toml { };

  defaultPackage =
    if cfg.package != null then cfg.package
    else if (builtins.hasAttr "kite" pkgs) then
    # Rebuild the package around the chosen engine; the wrapper pins
    # KITE_ENGINE to it, which is the only reliable way to pick between two
    # Chromium builds that both install as `bin/chromium`.
      pkgs.kite.override { engine = enginePackage; }
    else throw "programs.kite: set `programs.kite.package` (the flake module does this for you) or add the kite overlay";

  enginePackage =
    if cfg.engine == "chrome" then pkgs.google-chrome
    else if cfg.engine == "ungoogled" then pkgs.ungoogled-chromium
    else pkgs.chromium;
in
{
  options.programs.kite = {
    enable = lib.mkEnableOption "kite, a keyboard-first browser on the Chromium engine";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "The kite package to install. Defaults to pkgs.kite from the overlay.";
    };

    engine = lib.mkOption {
      type = lib.types.enum [ "chromium" "ungoogled" "chrome" ];
      default = "ungoogled";
      description = ''
        Which Chromium build to use as the engine. kite does not link against
        it: the binary is resolved at runtime, and this option decides what the
        wrapper points at.
      '';
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      example = lib.literalExpression ''
        {
          general.search_engine = "wikipedia";
          general.hints_chars = "jkl;fdsa";
          keys.normal = { "<C-S-h>" = "history-list"; };
          search.nix = "https://search.nixos.org/packages?query={}";
        }
      '';
      description = ''
        Declarative kite configuration, written to
        {file}`/etc/xdg/kite/config.toml`. Settings here merge with, and are
        overridden by, the user's own
        {file}`$XDG_CONFIG_HOME/kite/config.toml`.
      '';
    };

    defaultBrowser = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Register kite as the handler for http/https links.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ defaultPackage enginePackage ];

    environment.etc."xdg/kite/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = toml.generate "kite-config.toml" cfg.settings;
    };

    # Keep the engine's own sandbox intact; kite just spawns it.
    xdg.mime.defaultApplications = lib.mkIf cfg.defaultBrowser {
      "x-scheme-handler/http" = "kite.desktop";
      "x-scheme-handler/https" = "kite.desktop";
      "text/html" = "kite.desktop";
    };
  };
}
