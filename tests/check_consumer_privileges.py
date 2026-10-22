"""Exercise real root CLI dispatch in both engines, including group/env custody."""

import json
import os
from pathlib import Path
import pwd
import subprocess
import sys

native, python_peer, interpreter, case_id = sys.argv[1:]
assert os.getuid() == os.geteuid() == 0
account = pwd.getpwnam("demo")
root = Path("/run/consumer-boundary")
root.mkdir(mode=0o755)
secret = root / "root-secret"
secret.write_text("must-not-be-readable-by-validator")
secret.chmod(0o600)
assertions = []
for engine, binary in (("native", native), ("python", python_peer)):
    selected = root / engine
    selected.mkdir(mode=0o755)
    authority, data = selected / "authority", selected / "data"
    authority.mkdir(mode=0o700)
    data.mkdir(mode=0o700)
    os.chown(data, account.pw_uid, account.pw_gid)
    marker = data / "validator.json"
    code = """import json,os,pathlib,pwd,sys
pathlib.Path(sys.argv[1]).write_text(json.dumps({'dispatched':True}))
account=pwd.getpwnam('demo')
assert os.getuid()==os.geteuid()==account.pw_uid!=0
assert os.getgid()==os.getegid()==account.pw_gid!=0
assert set(os.getgroups())==set(os.getgrouplist('demo',account.pw_gid))
assert os.environ['USER']==os.environ['LOGNAME']=='demo'
assert os.environ['HOME']=='/' and os.getcwd()=='/'
assert not ({'HARBOR_DB_CONSUMER_SECRET','PYTHONSTARTUP','CREDENTIALS_DIRECTORY','LD_LIBRARY_PATH'} & os.environ.keys())
try: pathlib.Path(sys.argv[2]).read_text()
except PermissionError: pass
else: raise AssertionError('validator read root-only secret')
pathlib.Path(sys.argv[1]).write_text(json.dumps({'uid':os.geteuid(),'groups':os.getgroups()}))
print(json.dumps({'binding':{'validator':'unprivileged'},'directories':[],'required_files':[]}))
"""
    config = {
        "resource": "consumer-boundary",
        "state_dir": str(authority),
        "directories": [str(data)],
        "binding": {"backend": "fixture"},
        "user": "demo",
        "consumer_command": [interpreter, "-B", "-c", code, str(marker), str(secret)],
    }
    policy = selected / "policy.json"
    environment = {
        **os.environ,
        "HOME": "/root",
        "HARBOR_DB_CONSUMER_SECRET": "root-secret",
        "PYTHONSTARTUP": str(secret),
    }
    mutable = data / "validator"
    mutable.write_text(f"#!/bin/sh\nprintf dispatched > {marker}\n")
    os.chown(mutable, account.pw_uid, account.pw_gid)
    mutable.chmod(0o755)
    unsafe = [
        ("mutable-executable", dict(config, consumer_command=[str(mutable)])),
        ("root-user", dict(config, user="root")),
        (
            "missing-user",
            {key: value for key, value in config.items() if key != "user"},
        ),
    ]
    for name, value in unsafe:
        policy.write_text(json.dumps(value))
        result = subprocess.run(
            [binary, "--config", str(policy), "adopt", "--identity", "verified"],
            env=environment,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode != 0, (engine, value, result.stdout, result.stderr)
        assert not marker.exists() and not (authority / "identity.json").exists()
        assertions.append(
            {"name": f"{engine}-{name}-rejected-before-dispatch", "passed": True}
        )
    policy.write_text(json.dumps(config))
    for action in (("adopt", "--identity", "verified"), ("check",)):
        result = subprocess.run(
            [binary, "--config", str(policy), *action],
            env=environment,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, (engine, action, result.stdout, result.stderr)
        assert json.loads(marker.read_text())["uid"] == account.pw_uid
        marker.unlink()
        assertions.append(
            {
                "name": f"{engine}-{action[0]}-unprivileged-groups-environment-secret-custody",
                "passed": True,
            }
        )
assert len(assertions) == 10
print(json.dumps({"schema": 1, "case_id": case_id, "assertions": assertions}))
