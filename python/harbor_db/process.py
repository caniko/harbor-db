"""Bounded receipt execution; adapter diagnostics are never persisted or echoed."""

import os
import selectors
import signal
import subprocess
import time


def terminate(process):
    if process.returncode is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    time.sleep(0.1)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait(timeout=5)


def execute(
    argv,
    *,
    timeout,
    environment,
    leases=(),
    cwd=None,
    maximum_output=1024 * 1024,
    failure_context=None,
    **identity,
):
    process = subprocess.Popen(
        argv,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
        cwd=cwd,
        pass_fds=tuple(leases),
        start_new_session=True,
        **identity,
    )
    output = bytearray()
    diagnostics = bytearray()
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
                            raise ValueError(
                                "storage worker receipt exceeded its size limit"
                            )
                    elif failure_context is not None:
                        diagnostics.extend(chunk)
                        if len(diagnostics) > maximum_output:
                            raise ValueError(
                                "storage worker receipt exceeded its size limit"
                            )
            # Preserve the leader's PID until group cleanup, including after EOF.
            # A silent descendant may still own explicitly inherited leases.
            while (
                os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                is None
            ):
                if time.monotonic() >= deadline:
                    raise ValueError("storage worker exceeded its execution limit")
                time.sleep(0.005)
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            result = process.wait(timeout=5)
        if result:
            if failure_context is not None:
                raise ValueError(
                    f"{failure_context}: {diagnostics.decode('utf-8').strip()}"
                )
            raise ValueError("storage worker failed; diagnostics suppressed")
        return bytes(output)
    except BaseException:
        terminate(process)
        raise
    finally:
        process.stdout.close()
        process.stderr.close()
