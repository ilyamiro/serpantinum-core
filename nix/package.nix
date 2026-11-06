# serpantinum-core built from this checkout
{ pkgs, lib ? pkgs.lib }:

pkgs.rustPlatform.buildRustPackage {
  pname = "serpantinum-core";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ../assets ../LICENSE ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  postInstall = ''
    install -Dm644 assets/org.serpantinum.Core.service $out/share/dbus-1/services/org.serpantinum.Core.service
    substituteInPlace $out/share/dbus-1/services/org.serpantinum.Core.service --replace-fail @out@ $out
  '';

  meta = {
    description = "A core of services for Serpantinum shell in Rust";
    homepage = "https://github.com/ilyamiro/serpantinum-core";
    license = lib.licenses.agpl3Plus;
    mainProgram = "serpantinum-core";
    platforms = lib.platforms.linux;
  };
}
