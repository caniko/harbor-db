{
  pkgs,
  module,
}: let
  evaluate = additional:
    import "${pkgs.path}/nixos/lib/eval-config.nix" {
      inherit (pkgs) system;
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
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-application-provision-eval";
    resultMessage = "Dedicated roles, peer mappings, provisioning order and conflict assertions are retained";
    assertions = [
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
