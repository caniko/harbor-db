{pkgs}:
import ./test-python-regressions.nix {
  inherit pkgs;
  pythonSource = ../tests/oracles/pr14/python;
  oracleRoot = ../tests/oracles/pr14;
  checkName = "postgres-lifecycle-oracle-test";
}
