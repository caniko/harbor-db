{
  lib,
  harborModule,
  gelModule,
  pkgs,
}: let
  toyPackage = pkgs.writeShellScriptBin "toy-chaosbox" (builtins.readFile ../tests/gel/toy-chaosbox);
  migrations = pkgs.runCommand "gel-toy-migrations" {} ''
    mkdir -p "$out"
    cp ${../tests/gel/migrations}/*.edgeql "$out/"
  '';
  adminPassword = "gel-test-admin-pw";
  credsFile = pkgs.writeText "gel-test-creds.json" (builtins.toJSON {
    host = "127.0.0.1";
    port = 5656;
    user = "admin";
    password = adminPassword;
    branch = "main";
    tls_security = "insecure";
  });
  readerCredsFile = pkgs.writeText "gel-test-reader-creds.json" (builtins.toJSON {
    host = "127.0.0.1";
    port = 5656;
    user = "toy_reader";
    password = "toy-reader-pw";
    branch = "main";
    tls_security = "insecure";
  });
in
  pkgs.testers.nixosTest {
    name = "harbor-db-gel";

    nodes.machine = {config, ...}: {
      imports = [harborModule gelModule];
      virtualisation.memorySize = 4096;
      virtualisation.cores = 4;
      virtualisation.docker.enable = true;
      virtualisation.oci-containers.backend = "docker";

      environment.etc."gel-test-password".text = "${adminPassword}\n";

      services.harbor-db.gel.instances.test = {
        enable = true;
        passwordFile = "/etc/gel-test-password";
      };
      services.harbor-db.dataDirectories = [
        {
          path = "/var/lib/harbor-db-gel/test";
          user = "root";
          group = "root";
          mode = "0700";
        }
      ];

      systemd.services.gel-test-setup = {
        description = "Stage disposable Gel toy fixture state";
        wantedBy = ["multi-user.target"];
        before = ["harbor-db-geltoy.service"];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          mkdir -p /var/lib/gel-test/migrations
          cp ${migrations}/*.edgeql /var/lib/gel-test/migrations/
          chmod -R u+rw /var/lib/gel-test
        '';
      };

      systemd.services.geltoy-app = {
        wantedBy = ["multi-user.target"];
        serviceConfig = {
          Type = "oneshot";
        };
        script = ''
          set -eu
          count="$(${config.services.harbor-db.gel.cliPackage}/bin/gel \
            --credentials-file "${credsFile}" \
            --connect-timeout 10s --wait-until-available 60s \
            query --output-format json "select count((select ToyItem))")"
          test "$count" = "[1]"
          echo app >> /var/lib/gel-test/app-events
        '';
      };

      services.harbor-db.projects.geltoy = {
        enable = true;
        description = "Gel toy migration";
        operations.schema = {
          enable = true;
          backend = "gel";
          credentials.gel-creds = credsFile;
          runner = {
            package = toyPackage;
            executable = "bin/toy-chaosbox";
            args = ["db" "migrate" "--json"];
            checkArgs = ["db" "check" "--json"];
            credentialEnvironment.CHAOSBOX_GEL_CREDENTIALS_FILE = "gel-creds";
          };
          after = ["docker-harbor-db-gel-test.service" "gel-test-setup.service"];
          requires = ["docker-harbor-db-gel-test.service" "gel-test-setup.service"];
          runtimeUnits = ["geltoy-app.service"];
        };
        operations.wipe = {
          enable = true;
          backend = "gel";
          phase = "operational";
          safety = "operator_confirmed";
          dependsOn = ["schema"];
          runner = {
            command = "touch /var/lib/gel-test/wiped";
            # Fixture polarity: the full readiness check passes while no wipe
            # is outstanding. Ordinary activation still skips this operation
            # (operator_confirmed); only explicit --confirm runs the apply.
            checkCommand = "test ! -e /var/lib/gel-test/wiped";
          };
        };
        serviceConfig.ReadWritePaths = ["/var/lib/gel-test"];
      };

      # No runtimeUnits, so neither unit auto-starts: the permission-denied
      # apply can be triggered on demand without failing the boot.
      services.harbor-db.projects.geltoy-reader = {
        enable = true;
        description = "Gel toy reader credentials";
        operations.schema = {
          enable = true;
          backend = "gel";
          credentials.gel-reader-creds = readerCredsFile;
          runner = {
            package = toyPackage;
            executable = "bin/toy-chaosbox";
            args = ["db" "migrate" "--json"];
            checkArgs = ["db" "check" "--json"];
            credentialEnvironment.CHAOSBOX_GEL_CREDENTIALS_FILE = "gel-reader-creds";
          };
          after = ["docker-harbor-db-gel-test.service"];
          requires = ["docker-harbor-db-gel-test.service"];
        };
        serviceConfig.ReadWritePaths = ["/var/lib/gel-test"];
      };
    };

    testScript = ''
      machine.wait_until_succeeds("systemctl show harbor-db-geltoy.service -p Result --value | grep -Fx success", timeout=600)
      machine.wait_until_succeeds("systemctl show geltoy-app.service -p Result --value | grep -Fx success")
      machine.succeed("test ! -e /var/lib/gel-test/wiped")
      machine.succeed("test $(wc -l < /var/lib/gel-test/app-events) -eq 1")
      machine.succeed("systemctl start harbor-db-geltoy.service")
      machine.succeed("systemctl start harbor-db-geltoy-check.service")

      # A broken committed migration fails the apply and never reaches the app.
      machine.succeed("printf 'create !!! broken !!!\\n' > /var/lib/gel-test/migrations/0004-broken.edgeql")
      machine.fail("systemctl start harbor-db-geltoy.service")
      machine.succeed("test $(wc -l < /var/lib/gel-test/app-events) -eq 1")
      machine.succeed("rm /var/lib/gel-test/migrations/0004-broken.edgeql")
      machine.succeed("systemctl start harbor-db-geltoy.service")

      # Reader credentials read but cannot migrate (Gel permission model).
      machine.succeed("systemctl start harbor-db-geltoy-reader-check.service")
      machine.succeed("printf 'create type ToyNope { create required property name -> str; };\\n' > /var/lib/gel-test/migrations/0004-nope.edgeql")
      machine.fail("systemctl start harbor-db-geltoy-reader.service")
      status, output = machine.execute(
        "${pkgs.gel}/bin/gel --credentials-file ${readerCredsFile} "
        "query 'create type ToyNope { create required property name -> str; }' 2>&1")
      assert status != 0 and "permission" in output, f"reader DDL must be denied: {output!r}"
      machine.succeed("rm /var/lib/gel-test/migrations/0004-nope.edgeql")

      # Secrets stay out of rendered plans and unit output.
      machine.succeed("! grep -R -n gel-test-admin-pw /nix/store/*geltoy-plan.json /nix/store/*geltoy-reader-plan.json")
      machine.succeed("! systemctl show harbor-db-geltoy.service | grep -F gel-test-admin-pw")
    '';
  }
