# Usage in configuration.nix:
#   environment.systemPackages = [ (pkgs.callPackage /path/to/serpantinum-core { }) ];
#   services.dbus.packages = [ that same package ];   # lets D-Bus start it on first use
#
# By default this is the binary of the latest release (nix/release.json, written by the release
# workflow). `{ fromSource = true; }` compiles this checkout instead (nix/package.nix), and so does
# a checkout with no release yet.
{ pkgs ? import <nixpkgs> { }, fromSource ? false }:

if fromSource
  || !builtins.pathExists ./nix/release.json
  || pkgs.stdenv.hostPlatform.system != "x86_64-linux"
then import ./nix/package.nix { inherit pkgs; }
else import ./nix/bin.nix { inherit pkgs; }
