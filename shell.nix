{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    rustup
    pkg-config
    cmake
    clang
    mold
  ];

  buildInputs = with pkgs; [
    alsa-lib
    fontconfig
    freetype
    libxkbcommon
    wayland
    vulkan-loader
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
  ];

  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [
    alsa-lib
    fontconfig
    freetype
    libxkbcommon
    wayland
    vulkan-loader
    xorg.libX11
    xorg.libXcursor
    xorg.libXi
    xorg.libXrandr
  ]);

  RUST_BACKTRACE = "1";
}
