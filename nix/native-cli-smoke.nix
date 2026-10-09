{
  pkgs,
  package,
  testPackage,
}: let
  binaries = [
    "harbor-db"
    "harbor-db-postgres"
    "harbor-db-durable"
    "harbor-db-backup-prune"
    "harbor-db-resource"
    "harbor-db-provision"
    "harbor-db-application-backup"
    "harbor-db-postgres-drill"
    "harbor-db-transition"
    "harbor-db-transition-start"
    "harbor-db-cutover"
    "harbor-db-cutover-shell"
  ];
in
  pkgs.runCommand "harbor-db-native-cli-smoke" {nativeBuildInputs = [pkgs.python3];} ''
    test ! -e ${package}/bin/harbor-db-test
    test ! -e ${package}/bin/harbor-db-native-supervisor-fixture
    test ! -e ${package}/bin/harbor-db-writer-fence-fixture
    test ! -e ${package}/bin/harbor-db-backend-transition-fixture
    test ! -e ${package}/bin/harbor-db-provision-fixture
    test "$(find ${package}/bin -maxdepth 1 -type f | wc -l)" -eq ${toString (builtins.length binaries + 1)}
    ${pkgs.lib.concatMapStringsSep "\n" (binary: ''
        test -x ${package}/bin/${binary}
      '')
      binaries}
    # The login shell forwards its argv to the declared shell, including
    # --help. It requires custody rather than exposing an operator parser.
    ${pkgs.lib.concatMapStringsSep "\n" (binary: ''
        ${package}/bin/${binary} --help > ${binary}.help
        test -s ${binary}.help
      '')
      (pkgs.lib.remove "harbor-db-cutover-shell" binaries)}
    if ${package}/bin/harbor-db-cutover-shell -c 'touch login-shell-unexpected-execution' > login-shell.stdout 2> login-shell.stderr; then
      exit 1
    fi
    test ! -e login-shell-unexpected-execution
    grep -F 'harbor-db-cutover-shell:' login-shell.stderr
    # The legacy Home Manager helper takes a target path, not --help.
    test -x ${package}/bin/home-manager-backup
    printf current > collision
    printf previous > collision.bak
    HOME_MANAGER_BACKUP_EXT=bak ${package}/bin/home-manager-backup "$PWD/collision"
    test ! -e collision
    test "$(cat collision.bak)" = current
    test "$(cat collision.bak.canix-*)" = previous
    ${testPackage}/bin/harbor-db-test --help > harbor-db-test.help
    test -s harbor-db-test.help
    test -x ${testPackage}/bin/harbor-db-native-supervisor-fixture
    test -x ${testPackage}/bin/harbor-db-writer-fence-fixture
    test -x ${testPackage}/bin/harbor-db-backend-transition-fixture
    test -x ${testPackage}/bin/harbor-db-provision-fixture
    test -f ${testPackage}/share/harbor-db-test/suite.toml
    test -f ${testPackage}/share/harbor-db-test/python/harbor_db/test_bridge.py
    ${testPackage}/bin/harbor-db-test catalog --suite ${testPackage}/share/harbor-db-test/suite.toml > catalog.json
    # Registered Cargo execution must work with the packaged toolchain and
    # linker, without an inherited development shell or user Cargo setup.
    mkdir -m 0700 packaged-cargo packaged-cargo/work packaged-cargo/home
    printf '%s\n' '[package]' 'name = "packaged-cargo"' 'version = "0.0.0"' 'edition = "2024"' '[lib]' 'path = "lib.rs"' > packaged-cargo/Cargo.toml
    printf '%s\n' 'version = 4' '[[package]]' 'name = "packaged-cargo"' 'version = "0.0.0"' > packaged-cargo/Cargo.lock
    printf '%s\n' '#[test]' 'fn packaged_pass() { assert_eq!(2 + 3, 5); }' > packaged-cargo/lib.rs
    python3 - <<'PY'
    import json
    from pathlib import Path
    root = Path("packaged-cargo").resolve()
    spec = {
        "schema": 1, "case_id": "packaged-cargo", "env": {},
        "argv": ["cargo", "test", "--locked", "--offline", "--lib", "packaged_pass", "--", "--exact"],
        "workspace": str(root / "work"), "selector": "packaged_pass", "prerequisite": None,
    }
    (root / "executor.json").write_text(json.dumps(spec))
    PY
    (
      cd packaged-cargo
      env -i HOME="$PWD/home" CARGO_HOME="$PWD/home/cargo" PATH=${pkgs.lib.makeBinPath [pkgs.coreutils pkgs.bash]} \
        ${testPackage}/bin/harbor-db-test execute --spec "$PWD/executor.json"
    )
    python3 - <<'PY'
    import json
    from pathlib import Path
    receipt = json.loads(Path("packaged-cargo/work/acceptance.json").read_text())
    assert receipt == {
        "schema": 1, "case_id": "packaged-cargo",
        "assertions": [{"name": "Rust harness executed packaged_pass", "passed": True}],
    }, receipt
    PY
    mkdir "$out"
    cp *.help catalog.json "$out/"
    cp packaged-cargo/work/acceptance.json "$out/packaged-cargo.json"
  ''
