"""Power-management helpers for maintainer corpus builds."""
from __future__ import annotations

import os
import shutil
import subprocess
import sys


def maybe_reexec_with_sleep_inhibitor(reason: str) -> None:
    """Re-exec the current command under the platform sleep inhibitor.

    This is maintainer-only protection for long corpus builds. It is a no-op
    when the host lacks a known inhibitor, so end-user/runtime paths never
    acquire a platform dependency.
    """

    if os.environ.get("ATO_MCP_SLEEP_INHIBITED") or _truthy(os.environ.get("ATO_MCP_ALLOW_SLEEP")):
        return
    argv = _inhibited_argv(reason)
    if argv is None:
        return

    env = os.environ.copy()
    env["ATO_MCP_SLEEP_INHIBITED"] = "1"
    os.execvpe(argv[0], argv, env)


def _inhibited_argv(reason: str) -> list[str] | None:
    current = [sys.argv[0], *sys.argv[1:]]
    if sys.platform == "darwin":
        caffeinate = shutil.which("caffeinate")
        if caffeinate:
            return [caffeinate, "-dimsu", *current]
        return None

    if sys.platform.startswith("linux"):
        systemd_inhibit = shutil.which("systemd-inhibit")
        if systemd_inhibit and _systemd_inhibit_works(systemd_inhibit):
            return [
                systemd_inhibit,
                "--who=ato-mcp",
                "--what=sleep",
                "--mode=block",
                f"--why={reason}",
                *current,
            ]
    return None


def _systemd_inhibit_works(systemd_inhibit: str) -> bool:
    result = subprocess.run(
        [
            systemd_inhibit,
            "--who=ato-mcp",
            "--what=sleep",
            "--mode=block",
            "--why=ato-mcp inhibitor probe",
            "true",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return result.returncode == 0


def _truthy(value: str | None) -> bool:
    return value in {"1", "true", "TRUE", "yes", "YES", "on", "ON"}
