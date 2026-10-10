"""Record complete execution of the unchanged 174-method regression corpus."""
import argparse
import hashlib
import json
import pathlib
import unittest
import xml.etree.ElementTree as xml


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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--case-id", required=True)
    args = parser.parse_args()
    suite = unittest.defaultTestLoader.discover(str(args.tests), pattern="test_*.py")
    assert suite.countTestCases() == 174
    result = unittest.TextTestRunner(verbosity=2, resultclass=Recorded).run(suite)
    document = xml.Element(
        "testsuite", name=args.case_id, tests=str(result.testsRun),
        failures=str(len(result.failures)), errors=str(len(result.errors)),
        skipped=str(len(result.skipped)))
    details = {test.id(): ("failure", text) for test, text in result.failures}
    details.update({test.id(): ("error", text) for test, text in result.errors})
    details.update({test.id(): ("skipped", reason) for test, reason in result.skipped})
    for name in result.executed:
        case = xml.SubElement(document, "testcase", name=name, classname=name.rsplit(".", 1)[0])
        if name in details:
            kind, text = details[name]
            xml.SubElement(case, kind).text = text
    xml.ElementTree(document).write(args.output / "junit.xml", encoding="utf-8", xml_declaration=True)
    acceptance = {
        "schema": 1, "case_id": "case-" + hashlib.sha256(args.case_id.encode()).hexdigest(),
        "assertions": [{"name": name, "passed": name in result.passed} for name in result.executed]}
    (args.output / "python-regressions-acceptance.json").write_text(json.dumps(acceptance) + "\n")
    assert result.testsRun == 174 and len(set(result.executed)) == 174
    assert result.wasSuccessful() and len(result.passed) == 174 and not result.skipped


if __name__ == "__main__":
    main()
