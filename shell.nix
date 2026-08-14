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

  # The software renderer is the reliable default in a NixOS development
  # shell: it avoids mixing the shell's Vulkan loader with the host's graphics
  # driver stack. Override this when explicitly testing another renderer.
  SLINT_BACKEND = "winit-software";

  RUST_BACKTRACE = "1";
}
