{pkgs}:
# Test-only peer for cross-language continuation of retained receipts. Never
# exported as a production package or used by module defaults.
pkgs.runCommand "harbor-db-test-python-peer" {
  nativeBuildInputs = [pkgs.makeWrapper];
  passthru.harborDbRuntime = "python";
  meta = {
    description = "Fail-closed PostgreSQL identity and crash-safe staged upgrades";
    license = pkgs.lib.licenses.asl20;
    mainProgram = "harbor-db-postgres";
    platforms = pkgs.lib.platforms.linux;
  };
} ''
  mkdir -p "$out/lib" "$out/bin"
  cp -r ${../python/harbor_db} "$out/lib/harbor_db"
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-postgres" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.postgres'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-durable" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.durable'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-backup-prune" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.backup'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-resource" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.resource'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-provision" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.provision'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-application-backup" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.application_backup'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-postgres-drill" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.postgres_drill'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-transition" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.application_transition'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-transition-start" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.transition_manifest'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-cutover" \
    --set PYTHONPATH "$out/lib" --prefix PATH : ${pkgs.systemd}/bin --add-flags '-B -m harbor_db.cutover'
  makeWrapper ${pkgs.python3}/bin/python3 "$out/bin/harbor-db-cutover-shell" \
    --set PYTHONPATH "$out/lib" --add-flags '-B -m harbor_db.login_shell'
''
