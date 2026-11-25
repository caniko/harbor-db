{
  pkgs,
  nativePackage,
  testPackage,
}: let
  inherit (pkgs) lib;
  bridge = import ./test-native-bridge.nix {inherit pkgs;};
  postgres = pkgs.postgresql_18;
in
  pkgs.testers.runNixOSTest {
    name = "harbor-db-native-postgres-observer";
    extraDriverArgs = ["--junit-xml" "junit.xml"];
    extraPythonPackages = ps: [(bridge.package ps)];
    nodes.machine = {
      imports = [./postgres-observer.nix];
      virtualisation.cores = 2;
      environment.systemPackages = [nativePackage postgres pkgs.util-linux];
      services.harbor-db.postgresql.observer.enable = true;
      services.postgresql = {
        enable = true;
        package = postgres;
        ensureDatabases = ["appdb"];
        settings.compute_query_id = "on";
        authentication = lib.mkAfter ''
          host all all 127.0.0.1/32 trust
          local all all trust
        '';
      };
    };
    testScript = ''
      machine.wait_for_unit("postgresql-setup.service")
      ${bridge.script {
        fixture = "${testPackage}/bin/harbor-db-postgres-observer-fixture";
        arguments = [
          "--shell"
          "${pkgs.bash}/bin/bash"
          "--psql"
          "${postgres}/bin/psql"
          "--cat"
          "${pkgs.coreutils}/bin/cat"
          "--sleep"
          "${pkgs.coreutils}/bin/sleep"
        ];
        nodes = ["machine"];
        artifact = "postgres-observer-acceptance.json";
      }}
    '';
  }
