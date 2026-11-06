{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    rustc
    cargo
    rustfmt
    clippy
    rust-analyzer
  ];

  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
  RUST_BACKTRACE = 1;
}
