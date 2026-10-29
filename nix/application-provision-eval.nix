{
  pkgs,
  module,
}: let
  evaluate = additional:
    import "${pkgs.path}/nixos/lib/eval-config.nix" {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        module
        {
          system.stateVersion = "26.05";
          boot.loader.grub.devices = ["/dev/vda"];
          fileSystems."/" = {
            device = "/dev/vda";
            fsType = "ext4";
          };
          services.postgresql.enable = true;
          services.harbor-db.projects.demo = {
            enable = true;
            postgres.provision = {
              enable = true;
              database = "demo";
              ownerRole = "demo_owner";
              runtimeRole = "demo_runtime";
              runtimeOsUser = "demo";
              schemaUnits = ["demo-schema.service"];
              runtimeUnits = ["demo.service"];
              tables.history = ["SELECT" "INSERT"];
            };
          };
        }
        additional
      ];
    };
  eval = evaluate {};
  duplicate = evaluate {
    services.harbor-db.projects.other.postgres.provision = {
      enable = true;
      database = "demo";
      ownerRole = "other_owner";
      runtimeRole = "other_runtime";
      runtimeOsUser = "other";
    };
  };
  failed = evaluation: builtins.filter (item: !item.assertion) evaluation.config.assertions;
  rejected = evaluation: fragment: pkgs.lib.any (item: pkgs.lib.hasInfix fragment item.message) (failed evaluation);
  names = ["bad project" "bad\nproject" "bad\tproject" "bad/project" "" (pkgs.lib.concatStrings (pkgs.lib.replicate 129 "a"))];
  provision = {
    enable = true;
    database = "other";
    ownerRole = "other_owner";
    runtimeRole = "other_runtime";
    runtimeOsUser = "other";
  };
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-application-provision-eval";
    resultMessage = "Dedicated roles, peer mappings, provisioning order and conflict assertions are retained";
    assertions = [
      {
        name = "provisioning-default-tracks-effective-primary-port";
        message = "Unspecified provisioning endpoints must follow the local primary's effective default/nondefault port; an explicit endpoint remains selected.";
        assertion = let
          selected = (evaluate {services.postgresql.settings.port = 5544;}).config;
          explicit = (evaluate {services.harbor-db.projects.demo.postgres.provision.port = 5545;}).config;
          manifest = builtins.fromJSON (builtins.unsafeDiscardStringContext selected.environment.etc."harbor-db/demo-provision.json".source.text);
        in
          eval.config.services.harbor-db.projects.demo.postgres.provision.port
          == 5432
          && selected.services.harbor-db.projects.demo.postgres.provision.port == 5544
          && manifest.endpoint.port == 5544
          && explicit.services.harbor-db.projects.demo.postgres.provision.port == 5545;
      }
      {
        name = "database-peer-admission-terminates-before-permissive-global-rules";
        message = "Runtime/owner/control peer mappings must end in scoped local and network denial, including hosts with permissive global fallback.";
        assertion = let
          selected = (evaluate {services.postgresql.authentication = pkgs.lib.mkAfter "local all all trust\nhost all all all trust";}).config;
        in
          pkgs.lib.hasInfix ''
            local demo demo_runtime peer map=harbor-demo-runtime
            local demo demo_owner peer map=harbor-demo-owner
            local demo postgres peer map=harbor-control
            local demo all reject
            host demo all all reject
          ''
          selected.services.postgresql.authentication
          && pkgs.lib.hasInfix "harbor-control postgres postgres" selected.services.postgresql.identMap;
      }
      {
        name = "privileged-runtime-peer-os-users-reject";
        message = "The PostgreSQL control account and root cannot also host restricted application runtime processes.";
        assertion = pkgs.lib.all (user:
          rejected (evaluate {
            services.harbor-db.projects.demo.postgres.provision = {
              ownerOsUser = "schema_owner";
              runtimeOsUser = pkgs.lib.mkForce user;
            };
          }) "runtime OS users must not use root or postgres") ["root" "postgres"];
      }
      {
        name = "runtime-privilege-admission-cannot-skip-behind-fence";
        message = "A skipped provisioning prerequisite must also prevent the dependent runtime unit from starting while writer-fence.json exists.";
        assertion = let
          fenced = evaluate {services.harbor-db.postgresql.enable = true;};
          gate = ["!${fenced.config.services.harbor-db.postgresql.stateDir}/writer-fence.json"];
        in
          fenced.config.systemd.services.harbor-db-demo-provision.unitConfig.ConditionPathExists
          == gate
          && fenced.config.systemd.services.harbor-db-demo-permissions.unitConfig.ConditionPathExists == gate
          && (fenced.config.systemd.services.demo.unitConfig.ConditionPathExists or []) == gate
          && !(eval.config.systemd.services.demo.unitConfig ? ConditionPathExists);
      }
      {
        name = "valid";
        assertion = failed eval == [];
        message = "provision-only projects must evaluate without a dummy migration";
      }
      {
        name = "duplicate";
        assertion = failed duplicate != [];
        message = "database reuse must be rejected";
      }
      {
        name = "provision-only-name-rejection";
        assertion = pkgs.lib.all (name: let
          e = evaluate {
            services.harbor-db.projects.${name} = {
              enable = true;
              postgres.provision = provision;
            };
          };
        in
          rejected e "provision project name")
        names;
        message = "Provision-only unsafe project names must reject even without generated migration units.";
      }
      {
        name = "provision-only-name-boundaries";
        assertion = pkgs.lib.all (name: failed (evaluate {services.harbor-db.projects.${name}.postgres.provision = provision;}) == []) ["a" "project_09-v1" (pkgs.lib.concatStrings (pkgs.lib.replicate 128 "a"))];
        message = "Valid provision-only project names must admit the exact 1..128-byte boundaries.";
      }
      {
        name = "schema-runtime-unit-overlap-rejection";
        assertion = rejected (evaluate {services.harbor-db.projects.demo.postgres.provision.runtimeUnits = ["demo-schema.service"];}) "schemaUnits and runtimeUnits must be disjoint";
        message = "A service cannot be ordered both before and after privilege reconciliation.";
      }
      {
        name = "cross-project-ordering-cycle-rejection";
        assertion = rejected (evaluate {
          services.harbor-db.projects.other.postgres.provision =
            provision
            // {
              schemaUnits = ["demo.service"];
              runtimeUnits = ["demo-schema.service"];
            };
        }) "provisioning ordering must be acyclic";
        message = "Schema and runtime dependencies must be checked as one graph across enabled projects.";
      }
      {
        name = "cross-project-shared-acyclic-unit-admission";
        assertion =
          failed (evaluate {
            services.harbor-db.projects.other.postgres.provision =
              provision
              // {
                schemaUnits = ["demo-schema.service"];
                runtimeUnits = ["demo.service"];
              };
          })
          == [];
        message = "Shared units remain valid when their combined ordering graph has no cycle.";
      }
      {
        name = "shared-owner-runtime-os-user-rejection";
        assertion = rejected (evaluate {services.harbor-db.projects.demo.postgres.provision.ownerOsUser = "demo";}) "ownerOsUser and runtimeOsUser must differ";
        message = "One peer OS account must never match both schema-owner and restricted-runtime HBA rules.";
      }
      {
        name = "cross-project-runtime-owner-user-rejection";
        assertion = pkgs.lib.all (overrides:
          rejected (evaluate {
            services.harbor-db.projects.other.postgres.provision =
              provision
              // {
                ownerOsUser = "other_owner_os";
                runtimeOsUser = "other_runtime_os";
              }
              // overrides;
          }) "runtime OS users must not map to any application owner role") [{ownerOsUser = "demo";} {runtimeOsUser = "root";}];
        message = "Both cross-project owner/runtime collision directions reject before peer authentication can grant schema-owner privileges.";
      }
      {
        name = "cross-project-disjoint-user-admission";
        assertion =
          failed (evaluate {
            services.harbor-db.projects.other.postgres.provision =
              provision
              // {
                ownerOsUser = "other_owner_os";
                runtimeOsUser = "other_runtime_os";
              };
          })
          == [];
        message = "Distinct enabled-project owner and runtime OS accounts remain valid.";
      }
      {
        name = "disabled-project-owner-collision-admission";
        assertion =
          failed (evaluate {
            services.harbor-db.projects.other.postgres.provision =
              provision
              // {
                enable = false;
                ownerOsUser = "demo";
              };
          })
          == [];
        message = "Disabled projects cannot reserve owner peer mappings or reject an enabled runtime account.";
      }
      {
        name = "schema-order";
        assertion = builtins.elem "demo-schema.service" eval.config.systemd.services.harbor-db-demo-provision.requiredBy;
        message = "schema must require provisioning";
      }
      {
        name = "runtime-order";
        assertion = builtins.elem "demo.service" eval.config.systemd.services.harbor-db-demo-permissions.requiredBy && builtins.elem "demo-schema.service" eval.config.systemd.services.harbor-db-demo-permissions.after;
        message = "runtime privileges must be reconciled after schema";
      }
      {
        name = "provisioning-phase-commands";
        assertion = pkgs.lib.hasSuffix " apply" eval.config.systemd.services.harbor-db-demo-provision.serviceConfig.ExecStart && pkgs.lib.hasSuffix " reconcile" eval.config.systemd.services.harbor-db-demo-permissions.serviceConfig.ExecStart && pkgs.lib.hasSuffix " check" eval.config.systemd.services.harbor-db-demo-permissions.serviceConfig.ExecStartPost;
        message = "Pre-schema setup must use apply; post-schema runtime admission must use strict reconcile and check.";
      }
      {
        name = "peer";
        assertion = pkgs.lib.hasInfix "harbor-demo-runtime demo demo_runtime" eval.config.services.postgresql.identMap;
        message = "runtime must map only to its dedicated role";
      }
    ];
  }
