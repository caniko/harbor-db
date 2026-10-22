"""Exercise the generated NixOS preactivation hook without production privileges."""

import itertools
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path


def main():
    hook = Path(sys.argv[1]).read_text()
    runner, systemctl = sys.argv[2:4]
    assert runner in hook and systemctl in hook, (
        "generated systemd command paths must match before substituting fixtures"
    )
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        calls = root / "calls.jsonl"
        incoming = root / "incoming.json"
        run = root / "systemd-run"
        stop = root / "systemctl"
        stage = root / "harbor-db-postgres"
        body = (
            f"#!{sys.executable}\n"
            "import json, os, signal, sys\n"
            "from pathlib import Path\n"
            "with open(os.environ['CALLS'], 'a') as stream:\n"
            "    stream.write(json.dumps([os.path.basename(sys.argv[0]), *sys.argv[1:]]) + '\\n')\n"
            "if os.path.basename(sys.argv[0]) == 'harbor-db-postgres':\n"
            "    source=Path(sys.argv[sys.argv.index('--source')+1]); destination=Path(sys.argv[sys.argv.index('--destination')+1])\n"
            "    if os.environ.get('STAGE_FAIL') == '1': sys.exit(37)\n"
            "    destination.write_bytes(source.read_bytes())\n"
            "if os.path.basename(sys.argv[0]) == 'systemd-run':\n"
            "    if os.environ['RESULT'] == 'interrupt':\n"
            "        os.kill(os.getppid(), signal.SIGTERM)\n"
            "    sys.exit({'fail':23, 'launch-fail':29}.get(os.environ['RESULT'], 0))\n"
            "if os.path.basename(sys.argv[0]) == 'systemctl':\n"
            "    if sys.argv[1] == 'stop': sys.exit(int(os.environ.get('STOP_CODE', '0')))\n"
            "    if sys.argv[1] == 'show':\n"
            "        if os.environ.get('UNIT_STATE') == 'unavailable': sys.exit(1)\n"
            "        print(os.environ['UNIT_STATE'])\n"
        )
        for executable in (run, stop, stage):
            executable.write_text(body)
            executable.chmod(0o700)
        script = root / "pre-switch"
        adapter = re.search(
            r"-- (/nix/store/[^ ]+/bin/harbor-db-postgres) --config", hook
        ).group(1)
        script.write_text(
            hook.replace(runner, str(run))
            .replace(systemctl, str(stop))
            .replace(adapter, str(stage))
            .replace("/srv/import/off-host.json", str(incoming))
            .replace("/run/harbor-db-recovery-import.", str(root / "credential."))
            + "\nprintf 'admission-next\\n'\n"
        )
        count = 0
        for action in ("switch", "test", "boot", "dry-activate", "check"):
            for supplied in (False, True):
                if supplied:
                    incoming.write_text("{}")
                else:
                    incoming.unlink(missing_ok=True)
                for outcome in ("ready", "fail", "interrupt"):
                    calls.unlink(missing_ok=True)
                    result = subprocess.run(
                        ["bash", str(script), "/candidate", action],
                        env={**os.environ, "CALLS": str(calls), "RESULT": outcome},
                        capture_output=True,
                        text=True,
                        timeout=10,
                        check=False,
                    )
                    events = (
                        [json.loads(line) for line in calls.read_text().splitlines()]
                        if calls.exists()
                        else []
                    )
                    preparing = action in ("switch", "test")
                    expected = (
                        {"ready": 0, "fail": 23, "interrupt": 143}[outcome]
                        if preparing
                        else 0
                    )
                    assert result.returncode == expected, (
                        action,
                        outcome,
                        result.stderr,
                    )
                    assert ("admission-next" in result.stdout) == (expected == 0)
                    if preparing:
                        offset = 1 if supplied else 0
                        argv = events[offset][1:]
                        assert events[offset][0] == "systemd-run"
                        assert "--wait" in argv and "--collect" in argv
                        assert (
                            "--property=User=postgres" in argv
                            and "--property=Group=postgres" in argv
                        )
                        assert (
                            "--property=ReadWritePaths=/srv/backups /srv/disposable"
                            in argv
                        )
                        assert (
                            "prepare-recovery" in argv
                            and "--preparation-config" in argv
                        )
                        assert not any(
                            value in argv for value in ("adopt", "adopt-live", "initdb")
                        )
                        assert (
                            any(
                                "LoadCredential=recovery-off-host:" in value
                                for value in argv
                            )
                            == supplied
                        )
                        if supplied:
                            assert not any(
                                value.endswith(":" + str(incoming)) for value in argv
                            ), "untrusted raw import source passed directly to systemd"
                            assert (
                                events[0][0] == "harbor-db-postgres"
                                and "stage-off-host-receipt" in events[0]
                            )
                            staged = events[0][events[0].index("--destination") + 1]
                            assert (
                                "--property=LoadCredential=recovery-off-host:" + staged
                                in argv
                            )
                            assert not Path(staged).parent.exists(), (
                                "private staging root survives hook completion"
                            )
                        assert [event[0] for event in events] == (
                            ["harbor-db-postgres"] if supplied else []
                        ) + (
                            ["systemd-run", "systemctl"]
                            if expected
                            else ["systemd-run"]
                        )
                        if expected:
                            assert events[offset + 1][1] == "stop"
                            assert (
                                events[offset + 1][2]
                                == next(
                                    value.removeprefix("--unit=")
                                    for value in argv
                                    if value.startswith("--unit=")
                                )
                                + ".service"
                            )
                    else:
                        assert events == []
                    count += 1
        incoming.write_text("{}")
        calls.unlink(missing_ok=True)
        rejected = subprocess.run(
            ["bash", str(script), "/candidate", "switch"],
            env={
                **os.environ,
                "CALLS": str(calls),
                "RESULT": "ready",
                "STAGE_FAIL": "1",
            },
            capture_output=True,
            text=True,
            timeout=10,
        )
        assert rejected.returncode == 37 and "admission-next" not in rejected.stdout
        events = [json.loads(line) for line in calls.read_text().splitlines()]
        assert [event[0] for event in events] == ["harbor-db-postgres"], (
            "rejected import source reached transient preparation"
        )
        assert not Path(events[0][events[0].index("--destination") + 1]).parent.exists()
        cleanup_cases = 0
        for supplied in (True, False):
            if supplied:
                incoming.write_text("{}")
            else:
                incoming.unlink(missing_ok=True)
            for action in ("switch", "test"):
                for outcome in ("fail", "launch-fail", "interrupt"):
                    for state, shell_flags in itertools.product(
                        (
                            "inactive",
                            "failed",
                            "active",
                            "activating",
                            "deactivating",
                            "unavailable",
                            "",
                        ),
                        ((), ("-e",)),
                    ):
                        calls.unlink(missing_ok=True)
                        result = subprocess.run(
                            ["bash", *shell_flags, str(script), "/candidate", action],
                            env={
                                **os.environ,
                                "CALLS": str(calls),
                                "RESULT": outcome,
                                "STOP_CODE": "5",
                                "UNIT_STATE": state,
                            },
                            capture_output=True,
                            text=True,
                            timeout=10,
                            check=False,
                        )
                        expected = {"fail": 23, "launch-fail": 29, "interrupt": 143}[
                            outcome
                        ]
                        assert (
                            result.returncode == expected
                            and "admission-next" not in result.stdout
                        ), (
                            action,
                            outcome,
                            state,
                            shell_flags,
                            result.returncode,
                            result.stderr,
                        )
                        events = [
                            json.loads(line) for line in calls.read_text().splitlines()
                        ]
                        offset = 1 if supplied else 0
                        unit = (
                            next(
                                value.removeprefix("--unit=")
                                for value in events[offset]
                                if value.startswith("--unit=")
                            )
                            + ".service"
                        )
                        if supplied:
                            staged = Path(
                                events[0][events[0].index("--destination") + 1]
                            )
                            stopped = state in ("inactive", "failed")
                            assert staged.parent.exists() != stopped, (
                                action,
                                outcome,
                                state,
                                result.stderr,
                            )
                            if not stopped:
                                assert staged.read_text() == "{}", (
                                    "potentially live worker lost staged receipt"
                                )
                                staged.unlink()
                                staged.parent.rmdir()
                        assert [event[0] for event in events] == (
                            ["harbor-db-postgres"] if supplied else []
                        ) + ["systemd-run", "systemctl", "systemctl"]
                        assert events[offset + 1] == ["systemctl", "stop", unit]
                        assert events[offset + 2] == [
                            "systemctl",
                            "show",
                            "--property=ActiveState",
                            "--value",
                            unit,
                        ]
                        cleanup_cases += 1
        print(
            f"Managed recovery preparation: {count} action/credential/failure/cancellation cases passed"
        )
        print(
            f"Managed recovery cleanup: {cleanup_cases} failed-stop/terminal/live/unknown-state cases passed"
        )


if __name__ == "__main__":
    main()
