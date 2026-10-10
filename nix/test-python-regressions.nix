{
  pkgs,
  pythonSource,
  checkName,
  oracleRoot ? null,
  testSource ? ../tests,
}:
pkgs.runCommand "harbor-db-${checkName}" {
  nativeBuildInputs = [pkgs.python3 pkgs.gitMinimal pkgs.postgresql_18];
  HARBOR_DB_TEST_POSTGRES = pkgs.postgresql_18;
} ''
  ${pkgs.lib.optionalString (oracleRoot != null) ''
    python3 -B - ${../tests/pr14-baseline.toml} ${oracleRoot} ${testSource} <<'PY'
    import hashlib, pathlib, sys, tomllib
    baseline = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
    root = pathlib.Path(sys.argv[2])
    runtime = [entry for entry in baseline['files'] if entry['role'] == 'runtime']
    assert len(runtime) == 16
    for entry in runtime:
        path = root / entry['path']
        assert path.is_file() and not path.is_symlink(), path
        assert hashlib.sha256(path.read_bytes()).hexdigest() == entry['sha256'], path
    for entry in baseline['files']:
        if entry['role'] == 'test':
            path = pathlib.Path(sys.argv[3]) / pathlib.Path(entry['path']).relative_to('tests')
            assert path.is_file() and not path.is_symlink(), path
            assert hashlib.sha256(path.read_bytes()).hexdigest() == entry['sha256'], path
    PY
  ''}
  mkdir "$out"
  PYTHONPATH=${pythonSource} python3 -B ${../tests/run_python_regressions.py} \
    --tests ${testSource} --output "$out" \
    --case-id nix.${pkgs.system}.${checkName}
''
