"""Record complete frozen-oracle or explicitly cataloged Python execution."""

import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import stat
import tomllib
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as xml


@contextlib.contextmanager
def immutable_git_fixture(executable):
    """Model only the pinned Git input's root ownership in a Nix user namespace."""
    if executable is None:
        yield None
        return
    store = pathlib.Path("/nix/store")
    selected = pathlib.Path(executable)
    if (
        not selected.is_absolute()
        or ".." in selected.parts
        or not selected.is_relative_to(store)
    ):
        raise ValueError("Git fixture must be a pinned immutable store input")
    original = pathlib.Path.lstat
    owner = original(store).st_uid
    if owner not in (0, 65534) or owner == os.getuid() != 0:
        raise ValueError("Git fixture is not an independently owned store input")
    paths = set()

    def reference(path, depth=0):
        if depth > 64 or not path.is_relative_to(store):
            raise ValueError("Git fixture reference escaped its store input")
        for entry in (path, *path.parents):
            if entry == store:
                break
            metadata = original(entry)
            if metadata.st_uid != owner:
                raise ValueError(
                    "Git fixture ownership differs from the immutable store"
                )
            paths.add(entry)
            if stat.S_ISLNK(metadata.st_mode):
                target = entry.readlink()
                reference(
                    target
                    if target.is_absolute()
                    else entry.parent.resolve(strict=True) / target,
                    depth + 1,
                )
            elif metadata.st_mode & 0o222:
                raise ValueError("Git fixture must remain read-only")

    reference(selected)
    resolved = selected.resolve(strict=True)
    reference(resolved)
    metadata = original(resolved)
    if not stat.S_ISREG(metadata.st_mode) or not metadata.st_mode & 0o111:
        raise ValueError("Git fixture must be an executable immutable file")

    def root_view(path, *args, **kwargs):
        metadata = original(path, *args, **kwargs)
        if path not in paths:
            return metadata
        if metadata.st_uid != owner:
            raise ValueError("Git fixture ownership changed during execution")
        values = list(metadata)
        values[4] = 0
        return os.stat_result(values)

    evidence = {
        "kind": "test-only-pinned-git-namespace-ownership",
        "executable": str(selected),
        "resolved": str(resolved),
        "actual_store_uid": owner,
        "modeled_root_uid": 0,
        "actual_runner_uid": os.getuid(),
        "paths": sorted(map(str, paths)),
        "production_source_changed": False,
    }
    with patch.object(pathlib.Path, "lstat", root_view):
        yield evidence


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


@contextlib.contextmanager
def store_ownership(fixture):
    """Model Nix's numeric root view without changing production admission."""
    if fixture is None:
        yield
        return
    store = pathlib.Path("/nix/store")
    info = fixture.lstat()
    assert fixture.is_relative_to(store) and stat.S_ISREG(info.st_mode)
    assert not info.st_mode & 0o222, "store root fixture must be immutable"
    root_uid = info.st_uid
    if root_uid == 0:
        yield
        return
    assert root_uid != os.geteuid(), "builder-owned files cannot model store root"
    original = pathlib.Path.lstat

    def mapped(path):
        info = original(path)
        if not path.is_relative_to(store) or info.st_uid != root_uid:
            return info
        fields = list(info)
        fields[4] = 0
        return os.stat_result(fields)

    # Only the explicit immutable store owner's numeric view is translated.
    # Mutable paths, builder ownership and every mode/alias check remain real.
    with patch.object(pathlib.Path, "lstat", mapped):
        yield


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tests", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--case-id", required=True)
    parser.add_argument("--catalog", type=pathlib.Path)
    parser.add_argument("--immutable-git-fixture", type=pathlib.Path)
    parser.add_argument("--store-root-fixture", type=pathlib.Path)
    args = parser.parse_args()
    suite = unittest.defaultTestLoader.discover(str(args.tests), pattern="test_*.py")

    def methods(tests):
        for test in tests:
            if isinstance(test, unittest.TestSuite):
                yield from methods(test)
            else:
                yield test.id()

    discovered = list(methods(suite))
    if args.catalog:
        catalog = tomllib.loads(args.catalog.read_text())
        expected = {
            entry["selector_prefix"] + selector
            for entry in catalog["inventories"]
            if entry["origin"] == "python"
            for selector in entry["selectors"]
        }
        assert len(expected) >= 174, "the complete frozen corpus must remain registered"
        assert set(discovered) == expected, (
            "Python execution differs from the declared catalog"
        )
    else:
        assert len(discovered) == 174
    assert len(set(discovered)) == len(discovered), "duplicate Python test selectors"
    expected_count = len(discovered)
    with (
        immutable_git_fixture(args.immutable_git_fixture) as fixture,
        store_ownership(args.store_root_fixture),
    ):
        result = unittest.TextTestRunner(verbosity=2, resultclass=Recorded).run(suite)
    if fixture is not None:
        (args.output / "python-regressions-fixtures.json").write_text(
            json.dumps(fixture, indent=2) + "\n"
        )
    document = xml.Element(
        "testsuite",
        name=args.case_id,
        tests=str(result.testsRun),
        failures=str(len(result.failures)),
        errors=str(len(result.errors)),
        skipped=str(len(result.skipped)),
    )
    details = {test.id(): ("failure", text) for test, text in result.failures}
    details.update({test.id(): ("error", text) for test, text in result.errors})
    details.update({test.id(): ("skipped", reason) for test, reason in result.skipped})
    for name in result.executed:
        case = xml.SubElement(
            document, "testcase", name=name, classname=name.rsplit(".", 1)[0]
        )
        if name in details:
            kind, text = details[name]
            xml.SubElement(case, kind).text = text
    xml.ElementTree(document).write(
        args.output / "junit.xml", encoding="utf-8", xml_declaration=True
    )
    acceptance = {
        "schema": 1,
        "case_id": "case-" + hashlib.sha256(args.case_id.encode()).hexdigest(),
        "assertions": [
            {"name": name, "passed": name in result.passed} for name in result.executed
        ],
    }
    (args.output / "python-regressions-acceptance.json").write_text(
        json.dumps(acceptance) + "\n"
    )
    assert (
        result.testsRun == expected_count
        and len(set(result.executed)) == expected_count
    )
    assert (
        result.wasSuccessful()
        and len(result.passed) == expected_count
        and not result.skipped
    )


if __name__ == "__main__":
    main()
