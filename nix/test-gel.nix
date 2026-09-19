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
  badCredsFile = pkgs.writeText "gel-test-bad-creds.json" (builtins.toJSON {
    host = "127.0.0.1";
    port = 5656;
    user = "admin";
    password = "wrong-pw";
    branch = "main";
    tls_security = "insecure";
  });
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
      # The preloaded server image unpacks ~1.5GB into /var/lib/docker on
      # top of the system closure; the test-VM default disk is too small.
      virtualisation.diskSize = 10240;
      virtualisation.docker.enable = true;
      virtualisation.oci-containers.backend = "docker";
      # Preload the server image through the Nix store instead of pulling
      # inside the VM: test guests have no working registry egress
      # ("failed to do request" from dockerd), while the CI host pulls
      # fine. oci-containers then loads from file and runs with
      # --pull missing, so the registry is never contacted at test time.
      # Product wiring is untouched: real hosts pull normally.
      virtualisation.oci-containers.containers."harbor-db-gel-test".imageFile = pkgs.dockerTools.pullImage {
        imageName = "geldata/gel";
        imageDigest = "sha256:b7270b0973da6950d01ae0d578c6d38cd8d87fabdd6c4b75a09b74291ad6f3a8";
        finalImageName = "geldata/gel";
        finalImageTag = "7.1";
        sha256 = "sha256-LldSfgB6p/cFRcmyE+XFGDL5U/vhPchIEv7AadizzO4=";
      };

      environment.etc."gel-test-password".text = adminPassword;

      # Tag form, deliberately: `docker load` drops RepoDigests, so the
      # module-default digest ref cannot resolve against the preloaded
      # image and the daemon falls back to the (unreachable) registry.
      # The tag resolves to identical bits; digest-pin enforcement stays
      # covered by gel-eval (default image contains @sha256:) and by
      # live.sh, which pulls the digest ref from a real registry.
      services.harbor-db.gel.image = "docker.io/geldata/gel:7.1";

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
          # Single boot run even though the unit is wanted directly and also
          # started by the migration's runtime-activation helper: without
          # this, app-events would count 2 and the blocking assertions below
          # could not use an exact count.
          RemainAfterExit = true;
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
        # The toy shell fixture needs the CLI plus text tools on PATH;
        # generated units otherwise run with a minimal default PATH.
        path = [
          config.services.harbor-db.gel.cliPackage
          pkgs.coreutils
          pkgs.gnugrep
        ];
        # Non-secret per-command configuration reaches generated apply/check
        # units through project-level environment (there is no runner-level
        # environment passthrough; the plan's CommandSpec.environment stays
        # empty and the unit Environment= carries the values).
        environment = {
          TOY_STATE_DIR = "/var/lib/gel-test/state";
          TOY_MIGRATIONS_DIR = "/var/lib/gel-test/migrations";
        };
        # Server availability is its own gate: the container unit being
        # started does not mean Gel accepts connections yet (bootstrap takes
        # tens of seconds), and neither the toy nor harbor-db retries a
        # command on its own. The schema migration must wait for this probe.
        operations.ready = {
          enable = true;
          backend = "gel";
          credentials.gel-admin-pw = "/etc/gel-test-password";
          runner = {
            command = ''${config.services.harbor-db.gel.readyCheck}/bin/harbor-db-gel-ready --host 127.0.0.1 --port 5656 --user admin --password-file "$READY_PW_FILE" --timeout 300s'';
            checkCommand = ''${config.services.harbor-db.gel.readyCheck}/bin/harbor-db-gel-ready --host 127.0.0.1 --port 5656 --user admin --password-file "$READY_PW_FILE" --timeout 60s'';
            credentialEnvironment.READY_PW_FILE = "gel-admin-pw";
          };
          after = ["docker-harbor-db-gel-test.service"];
          requires = ["docker-harbor-db-gel-test.service"];
        };
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
          dependsOn = ["ready"];
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
        path = [
          config.services.harbor-db.gel.cliPackage
          pkgs.coreutils
          pkgs.gnugrep
        ];
        environment = {
          TOY_STATE_DIR = "/var/lib/gel-test/state";
          TOY_MIGRATIONS_DIR = "/var/lib/gel-test/migrations";
        };
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
      import time

      def run(cmd):
          status, out = machine.execute(cmd + " 2>&1 || true")
          return status, out.strip()

      # Environment sanity for the record: without KVM the VM runs under
      # TCG emulation and every budget below must be read accordingly.
      print("kvm available: " + run("test -e /dev/kvm && echo yes || echo no")[1])

      # Poll the boot chain in Python (not blind driver waits): every poll
      # is logged, and on timeout the failure evidence prints AT THE END of
      # the log, where CI tails survive. The app only runs after a
      # successful migration (Requires/After via requiredByUnits) and
      # appends exactly once (RemainAfterExit), so one line proves the chain.
      deadline = time.time() + 900
      while True:
          _, count = run("wc -l < /var/lib/gel-test/app-events 2>/dev/null || echo 0")
          if count == "1":
              break
          _, failed = run("systemctl --no-pager --failed --plain | head -8")
          print(f"waiting: app-events={count} failed-units={failed!r}")
          if "harbor-db-geltoy.service" in failed or "geltoy-app.service" in failed or "docker-harbor-db-gel-test.service" in failed:
              # Evidence MUST fit ~12 short lines: nix only shows the last
              # 25 build-log lines on failure, so few giant lines get cut.
              # The container unit journal carries the pull/run error.
              _, journal = run("journalctl -u docker-harbor-db-gel-test.service --no-pager --lines 10")
              raise AssertionError("boot chain unit failed: " + failed + "\nCONTAINER-JOURNAL:\n" + journal)
          if time.time() > deadline:
              raise AssertionError("boot chain did not complete in 900s")
          time.sleep(10)
      print("boot chain complete")
      machine.succeed("test ! -e /var/lib/gel-test/wiped")
      machine.succeed("test $(wc -l < /var/lib/gel-test/app-events) -eq 1")
      machine.succeed("systemctl start harbor-db-geltoy.service")
      machine.succeed("systemctl start harbor-db-geltoy-check.service")

      # A broken committed migration fails the apply and never reaches the app.
      machine.succeed("printf 'create !!! broken !!!\\n' > /var/lib/gel-test/migrations/0004-broken.edgeql")
      machine.fail("systemctl start harbor-db-geltoy.service")
      machine.succeed("test $(wc -l < /var/lib/gel-test/app-events) -eq 1")
      machine.succeed("rm /var/lib/gel-test/migrations/0004-broken.edgeql")
      machine.succeed("systemctl reset-failed harbor-db-geltoy.service")
      machine.succeed("systemctl start harbor-db-geltoy.service")

      # A migration requiring a newer server is incompatible (not pending) and
      # blocks the dependent without applying anything.
      machine.succeed("printf -- '-- REQUIRES-MAJOR: 99\\ncreate type ToyFuture { create required property name -> str; };\\n' > /var/lib/gel-test/migrations/0004-future.edgeql")
      machine.fail("systemctl start harbor-db-geltoy.service")
      machine.succeed("test $(wc -l < /var/lib/gel-test/app-events) -eq 1")
      machine.succeed("rm /var/lib/gel-test/migrations/0004-future.edgeql")
      machine.succeed("systemctl reset-failed harbor-db-geltoy.service")
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

      # A wrong password is an authenticated error, never ready/pending.
      status, output = machine.execute(
        "CHAOSBOX_GEL_CREDENTIALS_FILE=${badCredsFile} "
        "TOY_MIGRATIONS_DIR=/var/lib/gel-test/migrations TOY_STATE_DIR=/tmp/gel-bad-state "
        "PATH=${pkgs.gel}/bin:$PATH ${toyPackage}/bin/toy-chaosbox db check --json 2>&1")
      assert status == 1 and '"status":"error"' in output, f"bad password must be an error: {output!r}"

      # Secrets stay out of rendered plans and unit output.
      machine.succeed("! grep -R -n gel-test-admin-pw /nix/store/*geltoy-plan.json /nix/store/*geltoy-reader-plan.json")
      machine.succeed("! systemctl show harbor-db-geltoy.service | grep -F gel-test-admin-pw")
    '';
  }
