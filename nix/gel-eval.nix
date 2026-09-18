{
  lib,
  module,
  gelModule,
  pkgs,
}: let
  secretValue = "gel-eval-admin-password";
  secretFile = pkgs.writeText "harbor-db-gel-eval-password" secretValue;
  runner = pkgs.writeShellScriptBin "gel-eval-runner" "exit 0";
  eval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    system = pkgs.system;
    modules = [
      module
      gelModule
      {
        system.stateVersion = "24.11";
        services.harbor-db.package = pkgs.writeShellScriptBin "harbor-db" "exit 0";
        services.harbor-db.gel.instances.demo = {
          enable = true;
          port = 56561;
          bindAddress = "127.0.0.1";
          dataDir = "/var/lib/harbor-db-gel/demo";
          passwordFile = secretFile;
        };
        services.harbor-db.projects.gel-app = {
          enable = true;
          operations.schema = {
            enable = true;
            backend = "gel";
            credentials.gel-creds = secretFile;
            runner = {
              package = runner;
              executable = "bin/gel-eval-runner";
              args = ["db" "migrate" "--json"];
              checkArgs = ["db" "check" "--json"];
              credentialEnvironment.CHAOSBOX_GEL_CREDENTIALS_FILE = "gel-creds";
            };
            after = ["podman-harbor-db-gel-demo.service"];
            requires = ["podman-harbor-db-gel-demo.service"];
            runtimeUnits = ["gel-app.service"];
          };
          serviceConfig.ReadWritePaths = ["/var/lib/gel-app"];
        };
        services.harbor-db.dataDirectories = [
          {
            path = "/var/lib/harbor-db-gel/demo";
            user = "root";
            group = "root";
            mode = "0700";
          }
        ];
      }
    ];
  };
  container = eval.config.virtualisation.oci-containers.containers.harbor-db-gel-demo;
  containerUnit = eval.config.systemd.services.podman-harbor-db-gel-demo;
  service = eval.config.systemd.services.harbor-db-gel-app;
  applyScript = builtins.replaceStrings ["\n"] [" "] (builtins.readFile service.serviceConfig.ExecStart);
  manifest = builtins.elemAt (builtins.match ".*--manifest ([^ ]+).*" applyScript) 0;
  plan = builtins.readFile manifest;
  # Only plain string fields are serialized: environment/volumes/ports contain
  # no derivations, so this cannot fail on unserializable values.
  containerStrings = builtins.toJSON {
    environment = container.environment;
    volumes = container.volumes;
    ports = container.ports;
  };
  readyScript = builtins.readFile "${eval.config.services.harbor-db.gel.readyCheck}/bin/harbor-db-gel-ready";
  negative = builtins.tryEval (
    let
      badConfig = import "${pkgs.path}/nixos/lib/eval-config.nix" {
        system = pkgs.system;
        modules = [
          module
          {
            system.stateVersion = "24.11";
            services.harbor-db.package = pkgs.writeShellScriptBin "harbor-db" "exit 0";
            services.harbor-db.projects.bad-grants = {
              enable = true;
              operations.default = {
                enable = true;
                backend = "gel";
                runner = {
                  package = runner;
                  executable = "bin/gel-eval-runner";
                  args = ["db" "migrate"];
                };
              };
              postgres = {
                enable = true;
                databaseUrl = "postgres:///bad?host=/run/postgresql";
                grants = {
                  enable = true;
                  runtimeRole = "bad";
                };
              };
            };
          }
        ];
      };
    in
      if lib.all (assertion: assertion.assertion) badConfig.config.assertions
      then true
      else throw "postgres grants with a gel default operation must fail evaluation"
  );
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-gel-eval";
    resultMessage = "harbor-db Gel support keeps secrets out of plans and pins the server image";
    assertions = [
      {
        name = "server-image-is-digest-pinned";
        assertion = lib.hasInfix "@sha256:" container.image && lib.hasPrefix "docker.io/geldata/gel:" container.image;
        message = "the Gel server image must be digest-pinned to the official image";
      }
      {
        name = "listener-is-loopback";
        assertion = container.ports == ["127.0.0.1:56561:5656"];
        message = "the Gel port must be published on loopback by default";
      }
      {
        name = "password-reaches-server-as-file";
        assertion = container.environment.GEL_SERVER_PASSWORD_FILE == "/run/secrets/gel-server-password";
        message = "the admin password must reach the server through a mounted file";
      }
      {
        name = "password-value-is-not-in-unit";
        assertion = !(lib.hasInfix secretValue containerStrings);
        message = "the password value must not appear in the rendered container unit";
      }
      {
        name = "migrations-are-project-owned";
        assertion = container.environment.GEL_DOCKER_APPLY_MIGRATIONS == "never";
        message = "container startup must never apply migrations implicitly";
      }
      {
        name = "gel-backend-plan-renders";
        assertion = lib.hasInfix "\"backend\":\"gel\"" plan;
        message = "gel backend operations must render backend gel into the plan";
      }
      {
        name = "credential-value-is-not-in-plan";
        assertion = !(lib.hasInfix secretValue plan);
        message = "credential contents must not be serialized into the generated plan";
      }
      {
        name = "gel-ordering-gates-runtime";
        assertion =
          lib.elem "podman-harbor-db-gel-demo.service" service.after
          && lib.elem "podman-harbor-db-gel-demo.service" service.requires
          && lib.elem "gel-app.service" service.requiredBy;
        message = "Gel server readiness must gate migration, which gates runtime";
      }
      {
        name = "systemd-unit-is-exposed";
        assertion = eval.config.services.harbor-db.gel.instances.demo.systemdUnit == "podman-harbor-db-gel-demo.service" && containerUnit.serviceConfig.ExecStart != null;
        message = "the instance must expose its real container unit for project ordering";
      }
      {
        name = "ready-check-is-authenticated-and-bounded";
        assertion = lib.hasInfix "--password-from-stdin" readyScript && lib.hasInfix "--wait-until-available" readyScript && lib.hasInfix "select 1" readyScript;
        message = "readyCheck must authenticate with the password file and bound its wait with a real query";
      }
      {
        name = "postgres-grants-reject-gel-default";
        assertion = !negative.success;
        message = "postgres grants must fail evaluation when the default operation uses the gel backend";
      }
    ];
  }
