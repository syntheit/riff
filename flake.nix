{
  description = "riff — native Spotify client (syntheit mobile-first fork): dev environment";

  # Pinned to the SAME nixpkgs as the fajita host so dev/test builds use the
  # exact libadwaita (1.8) / gtk4 (4.20) / blueprint-compiler the phone runs.
  # Avoids "worked on mantle, broke on fajita" version drift.
  inputs.nixpkgs.url = "github:nixos/nixpkgs/bb39d8133e1b525230b72ec50862b193882cc910";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          # Build toolchain (meson wraps cargo; blueprint compiles the .blp UI).
          nativeBuildInputs = with pkgs; [
            meson
            ninja
            pkg-config
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            blueprint-compiler
            gettext
            desktop-file-utils
            appstream-glib
            libxml2 # xmllint, for gresource xml-stripblanks
            glib # glib-compile-schemas
            gtk4 # gtk4-builder-tool / update-icon-cache
            wrapGAppsHook4 # wires GSettings/GST env for `meson devenv` runs
          ];
          # Native libraries riff links against.
          buildInputs = with pkgs; [
            gtk4
            libadwaita
            glib
            gst_all_1.gstreamer
            gst_all_1.gst-plugins-base
            alsa-lib
            libpulseaudio
            openssl
          ];
        };
      });
    };
}
