{ lib, rustPlatform, makeWrapper, chromium, xclip, wl-clipboard, installShellFiles }:

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

  # Chromium, and the clipboard helpers, are runtime deps: kite talks to the
  # engine as a separate process, so they must be reachable on PATH.
  postInstall = ''
    wrapProgram $out/bin/kite \
      --suffix PATH : ${lib.makeBinPath [ chromium xclip wl-clipboard ]}

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
    '';
    homepage = "https://github.com/david/kite";
    license = licenses.mit;
    maintainers = [ ];
    platforms = platforms.linux;
    mainProgram = "kite";
  };
}
