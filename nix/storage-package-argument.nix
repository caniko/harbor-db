{
  lib,
  pkgs,
  ...
}: {
  _module.args.harborDbStoragePackage = lib.mkDefault (import ./native-package.nix {inherit pkgs;});
}
