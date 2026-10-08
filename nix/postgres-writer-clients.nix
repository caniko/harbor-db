{
  config,
  lib,
  ...
}: let
  cfg = config.services.harbor-db.postgresql;
in {
  options.services.harbor-db.postgresql.writerFence.blockedUnits = lib.mkOption {
    type = lib.types.listOf (lib.types.strMatching "[A-Za-z0-9_@.:-]+\\.service");
    default = [];
    description = ''
      Explicit application/migration services skipped while the durable writer
      fence journal exists. Existing service conditions are retained. This is
      startup policy; the coordinator still stops existing clients, and the HBA
      excludes reconnecting clients across older generations. Thaw never starts
      these units; restart and application health verification are explicit.
    '';
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.all (unit: !(builtins.elem unit ["postgresql.service" "postgresql-setup.service"])) cfg.writerFence.blockedUnits;
        message = "PostgreSQL writer-fence client gates must leave primary startup and control setup available.";
      }
    ];
    systemd.services = lib.genAttrs (map (lib.removeSuffix ".service") cfg.writerFence.blockedUnits) (_: {
      unitConfig.ConditionPathExists = lib.mkAfter ["!${cfg.stateDir}/writer-fence.json"];
    });
  };
}
