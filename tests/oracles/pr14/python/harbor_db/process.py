"""Bounded receipt execution; adapter diagnostics are never persisted or echoed."""

import os
import selectors
import signal
import subprocess
import time


def terminate(process):
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)


def execute(argv, *, timeout, environment, leases=(), cwd=None, maximum_output=1024 * 1024, **identity):
    process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               env=environment, cwd=cwd, pass_fds=tuple(leases), start_new_session=True, **identity)
    output = bytearray()
    deadline = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ, True)
            selector.register(process.stderr, selectors.EVENT_READ, False)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ValueError("storage worker exceeded its execution limit")
                for key, _ in selector.select(min(remaining, 0.1)):
                    chunk = os.read(key.fileobj.fileno(), 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    elif key.data:
                        output.extend(chunk)
                        if len(output) > maximum_output:
                            raise ValueError("storage worker receipt exceeded its size limit")
            result = process.wait(timeout=max(0.001, deadline - time.monotonic()))
        if result:
            raise ValueError("storage worker failed; diagnostics suppressed")
        return bytes(output)
    except BaseException:
        terminate(process)
        raise
    finally:
        process.stdout.close()
        process.stderr.close()
