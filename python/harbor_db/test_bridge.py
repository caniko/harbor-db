"""Version-1 low-level NixOS driver bridge; assertions and waits belong to Rust.

The inherited control descriptor is separate from stdout/stderr. To use inside
a NixOS test driver call ``serve(fd, machines)`` with its node-name mapping.
The command-line entry point provides a disposable local execute/transfer node
for prototype development, using the identical protocol.
"""

import argparse
import json
import os
import shlex
import shutil
import subprocess
import tempfile

VERSION = 1
MAX_FRAME_BYTES = 1024 * 1024


class LocalNode:
    def execute(self, argv, timeout):
        # Disk spool bounds memory even when a prototype accidentally floods logs.
        with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
            result = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=output,
                                    stderr=errors, timeout=timeout, check=False)
            output.seek(0)
            text = output.read(MAX_FRAME_BYTES)
            if len(text) >= MAX_FRAME_BYTES:
                raise ValueError("bridge execution output exceeds frame limit")
            return result.returncode, text.decode("utf-8")


def dispatch(request, machines):
    if not isinstance(request, dict):
        raise ValueError("request must be an object")
    operation = request.get("operation")
    allowed = {
        "execute": {"node", "operation", "argv", "timeout_seconds"},
        "start": {"node", "operation", "allow_reboot"},
        "stop": {"node", "operation"}, "crash": {"node", "operation"},
        "reboot": {"node", "operation"},
        "copy_to": {"node", "operation", "source", "destination"},
        "copy_from": {"node", "operation", "source", "destination"},
    }
    if operation not in allowed or set(request) != allowed[operation]:
        raise ValueError("unknown operation or request fields")
    node = machines[request["node"]]
    if operation == "execute":
        argv = request["argv"]
        timeout = request["timeout_seconds"]
        if not isinstance(argv, list) or not argv or any(
                not isinstance(v, str) or "\0" in v for v in argv) or not argv[0]:
            raise ValueError("invalid execution argv")
        if type(timeout) is not int or timeout <= 0:
            raise ValueError("invalid execution timeout")
        if isinstance(node, LocalNode):
            code, output = node.execute(argv, timeout)
        else:
            code, output = node.execute(shlex.join(argv), timeout=timeout)
        return {"exit_code": code, "output": output}
    if operation in {"copy_to", "copy_from"}:
        source, destination = request["source"], request["destination"]
        if any(not isinstance(v, str) or not v or "\0" in v
               for v in (source, destination)):
            raise ValueError("invalid transfer path")
        if isinstance(node, LocalNode):
            shutil.copyfile(source, destination)
        elif operation == "copy_to":
            node.copy_from_host(source, destination)
        else:
            # NixOS's target_dir is a directory beneath $out, not an exact
            # destination filename. Normalize that driver API to our file ABI.
            parent = os.path.dirname(os.path.abspath(destination))
            with tempfile.TemporaryDirectory(dir=parent) as staging:
                node.copy_from_machine(source, staging)
                shutil.copyfile(os.path.join(staging, os.path.basename(source)),
                                destination)
        return {"transferred": True}
    if isinstance(node, LocalNode):
        raise ValueError("VM lifecycle operation requires a NixOS driver node")
    if operation == "start":
        if type(request["allow_reboot"]) is not bool:
            raise ValueError("invalid allow_reboot")
        node.start(allow_reboot=request["allow_reboot"])
    elif operation == "stop":
        node.shutdown()
    elif operation == "crash":
        node.crash()
    else:
        node.reboot()
    return {"completed": True}


def serve(control_fd, machines):
    last_id = 0
    with os.fdopen(os.dup(control_fd), "rb", buffering=0) as incoming, \
            os.fdopen(os.dup(control_fd), "wb", buffering=0) as outgoing:
        while True:
            line = incoming.readline(MAX_FRAME_BYTES + 1)
            if not line:
                return
            if len(line) > MAX_FRAME_BYTES or not line.endswith(b"\n"):
                raise ValueError("oversized or truncated control frame")
            frame = json.loads(line)
            identifier = frame.get("id")
            if set(frame) != {"version", "id", "request"} or \
                    type(frame["version"]) is not int or frame["version"] != VERSION or \
                    type(identifier) is not int or identifier <= last_id:
                raise ValueError("invalid control version or sequence")
            last_id = identifier
            response = {"version": VERSION, "id": identifier}
            try:
                response["result"] = dispatch(frame["request"], machines)
            except Exception as error:
                response["error"] = {"kind": type(error).__name__, "message": str(error)}
            data = json.dumps(response, separators=(",", ":"), ensure_ascii=True).encode() + b"\n"
            if len(data) > MAX_FRAME_BYTES:
                data = json.dumps({"version": VERSION, "id": identifier, "error": {
                    "kind": "FrameLimit", "message": "response exceeds frame limit"}}).encode() + b"\n"
            outgoing.write(data)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--control-fd", type=int, required=True)
    args = parser.parse_args()
    serve(args.control_fd, {"local": LocalNode()})


if __name__ == "__main__":
    main()
