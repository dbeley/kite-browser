{ lib
, rustPlatform
, makeWrapper
, ungoogled-chromium
, xclip
, wl-clipboard
, xdotool
, xprop
, installShellFiles
, engine ? null
}:

let
  # ungoogled-chromium by default: same engine, no Google services, and it
  # passes kite's whole smoke suite (see the README). Override with
  # `pkgs.kite.override { engine = pkgs.chromium; }`.
  enginePackage = if engine != null then engine else ungoogled-chromium;
in
rustPlatform.buildRustPackage {
  pname = "kite";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      let base = baseNameOf path; in
        !(builtins.elem base [ "target" "result" "flake.lock" ".git" "docs" ]);
  };

  cargoLock.lockFile = ../Cargo.lock;
  doCheck = false;

  nativeBuildInputs = [ makeWrapper installShellFiles ];

  # The engine, the clipboard helpers and the X11 frame tools are runtime deps:
  # kite talks to the engine as a separate process, so they must be on PATH.
  # `KITE_ENGINE` is pinned to an absolute path because every Nix Chromium build
  # installs itself as `bin/chromium`, so a PATH lookup cannot tell them apart.
  # `--set-default` leaves a user's own KITE_ENGINE alone.
  postInstall = ''
    wrapProgram $out/bin/kite \
      --set-default KITE_ENGINE "${lib.getExe enginePackage}" \
      --suffix PATH : ${lib.makeBinPath [ enginePackage xclip wl-clipboard xdotool xprop ]}

    install -Dm644 ${../nix/kite.desktop} $out/share/applications/kite.desktop
    install -Dm644 ${../nix/kite.svg} $out/share/icons/hicolor/scalable/apps/kite.svg
  '';

  meta = with lib; {
    description = "Keyboard-first browser shell on the Chromium engine";
    longDescription = ''
      kite runs the system Chromium as its engine and adds a vim-style keyboard
      layer, tabs, history, bookmarks and session restore from a ~10 MB Rust
      controller. No Python, no Qt, no Electron: the UI process is native code
      and the page is a stock Chromium renderer.

      The engine defaults to ungoogled-chromium; `programs.kite.engine` (or
      `pkgs.kite.override { engine = pkgs.chromium; }`) switches back.
    '';
    homepage = "https://github.com/david/kite";
    license = licenses.mit;
    maintainers = [ ];
    platforms = platforms.linux;
    mainProgram = "kite";
  };
}
