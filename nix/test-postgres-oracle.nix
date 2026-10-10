{pkgs}:
import ./test-python-regressions.nix {
  inherit pkgs;
  pythonSource = ../tests/oracles/pr14/python;
  oracleRoot = ../tests/oracles/pr14;
  checkName = "postgres-lifecycle-oracle-test";
  testSource = pkgs.runCommand "harbor-db-pr14-frozen-tests" {} ''
    cp -r ${../tests} "$out"
    chmod -R u+w "$out"
    cp ${../tests/oracles/pr14/tests/test_application_provision.py} "$out/test_application_provision.py"
  '';
}
