{pkgs}:
pkgs.runCommand "harbor-db-postgres-lifecycle-oracle-test" {
  nativeBuildInputs = [pkgs.python3 pkgs.gitMinimal pkgs.postgresql_18];
  HARBOR_DB_TEST_POSTGRES = pkgs.postgresql_18;
} ''
  # Original runtime/test digests stay in the immutable PR14 contract. This is
  # execution of that byte-identical oracle, separate from the extended adapter.
  python3 -B - ${../tests/pr14-baseline.toml} ${../tests/oracles/pr14} <<'PY'
  import hashlib, pathlib, sys, tomllib
  baseline = tomllib.loads(pathlib.Path(sys.argv[1]).read_text())
  root = pathlib.Path(sys.argv[2])
  runtime = [entry for entry in baseline['files'] if entry['role'] == 'runtime']
  assert len(runtime) == 16
  for entry in runtime:
      path = root / entry['path']
      assert path.is_file() and not path.is_symlink(), path
      assert hashlib.sha256(path.read_bytes()).hexdigest() == entry['sha256'], path
  PY
  mkdir "$out"
  export PYTHONPATH=${../tests/oracles/pr14/python}
  python3 -B - ${../tests} "$out" <<'PY'
  import hashlib, json, pathlib, sys, unittest, xml.etree.ElementTree as xml
  suite = unittest.defaultTestLoader.discover(sys.argv[1], pattern='test_*.py')
  assert suite.countTestCases() == 174
  class Recorded(unittest.TextTestResult):
      def __init__(self, *args):
          super().__init__(*args)
          self.passed = []
          self.executed = []
      def startTest(self, test):
          self.executed.append(test.id())
          super().startTest(test)
      def addSuccess(self, test):
          self.passed.append(test.id())
          super().addSuccess(test)
  result = unittest.TextTestRunner(verbosity=2, resultclass=Recorded).run(suite)
  document = xml.Element('testsuite', name='PR14 runtime oracle', tests=str(result.testsRun),
      failures=str(len(result.failures)), errors=str(len(result.errors)), skipped=str(len(result.skipped)))
  details = {test.id(): ('failure', text) for test, text in result.failures}
  details.update({test.id(): ('error', text) for test, text in result.errors})
  details.update({test.id(): ('skipped', reason) for test, reason in result.skipped})
  for name in result.executed:
      case = xml.SubElement(document, 'testcase', name=name, classname=name.rsplit('.', 1)[0])
      if name in details:
          kind, text = details[name]
          xml.SubElement(case, kind).text = text
  out = pathlib.Path(sys.argv[2])
  xml.ElementTree(document).write(out/'junit.xml', encoding='utf-8', xml_declaration=True)
  public_id = 'nix.${pkgs.system}.postgres-lifecycle-oracle-test'
  acceptance = {'schema': 1, 'case_id': 'case-'+hashlib.sha256(public_id.encode()).hexdigest(),
      'assertions': [{'name': name, 'passed': name in result.passed} for name in result.executed]}
  (out/'oracle-acceptance.json').write_text(json.dumps(acceptance)+'\n')
  assert result.testsRun == 174 and len(set(result.executed)) == 174
  assert result.wasSuccessful() and len(result.passed) == 174 and not result.skipped
  PY
''
