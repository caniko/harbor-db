{
  pkgs,
  module,
  nativePackage,
}: let
  eval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    inherit (pkgs) system;
    modules = [
      module
      {
        system.stateVersion = "26.05";
        services.postgresql = {
          enable = true;
          package = pkgs.postgresql_18;
          dataDir = "/var/lib/postgresql/18";
        };
        services.harbor-db.postgresql.enable = true;
      }
    ];
  };
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-native-storage-eval";
    resultMessage = "Native storage package is selected through the module argument";
    assertions = [
      {
        name = "postgres-native-package";
        message = "PostgreSQL lifecycle selects the native package";
        assertion = eval.config.services.harbor-db.postgresql.package == nativePackage;
      }
      {
        name = "cutover-native-package";
        message = "Cutover dispatch selects the native package";
        assertion = eval.config.services.harbor-db.cutover.package == nativePackage;
      }
      {
        name = "postgres-native-prestart";
        message = "The managed PostgreSQL pre-start hook executes the native adapter";
        assertion =
          pkgs.lib.hasInfix
          (builtins.unsafeDiscardStringContext "${nativePackage}/bin/harbor-db-postgres")
          (builtins.unsafeDiscardStringContext eval.config.systemd.services.postgresql.preStart);
      }
    ];
  }
