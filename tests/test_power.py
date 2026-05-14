from __future__ import annotations

from ato_mcp.util import power


def test_linux_sleep_inhibitor_wraps_current_command(monkeypatch) -> None:
    monkeypatch.setattr(power.sys, "platform", "linux")
    monkeypatch.setattr(power.sys, "argv", ["ato-mcp", "build-index", "--gpu"])
    monkeypatch.setattr(
        power.shutil,
        "which",
        lambda name: "/usr/bin/systemd-inhibit" if name == "systemd-inhibit" else None,
    )
    monkeypatch.setattr(power, "_systemd_inhibit_works", lambda _path: True)

    argv = power._inhibited_argv("ato-mcp corpus rebuild")

    assert argv == [
        "/usr/bin/systemd-inhibit",
        "--who=ato-mcp",
        "--what=sleep",
        "--mode=block",
        "--why=ato-mcp corpus rebuild",
        "ato-mcp",
        "build-index",
        "--gpu",
    ]


def test_macos_sleep_inhibitor_wraps_current_command(monkeypatch) -> None:
    monkeypatch.setattr(power.sys, "platform", "darwin")
    monkeypatch.setattr(power.sys, "argv", ["ato-mcp", "build-index"])
    monkeypatch.setattr(
        power.shutil,
        "which",
        lambda name: "/usr/bin/caffeinate" if name == "caffeinate" else None,
    )

    argv = power._inhibited_argv("ato-mcp corpus rebuild")

    assert argv == ["/usr/bin/caffeinate", "-dimsu", "ato-mcp", "build-index"]


def test_sleep_inhibitor_noops_without_platform_tool(monkeypatch) -> None:
    monkeypatch.setattr(power.sys, "platform", "linux")
    monkeypatch.setattr(power.shutil, "which", lambda _name: None)

    assert power._inhibited_argv("ato-mcp corpus rebuild") is None


def test_sleep_inhibitor_respects_disable_env(monkeypatch) -> None:
    called = False

    def fail_exec(*_args, **_kwargs) -> None:
        nonlocal called
        called = True

    monkeypatch.setenv("ATO_MCP_ALLOW_SLEEP", "1")
    monkeypatch.setattr(power, "_inhibited_argv", lambda _reason: ["inhibitor", "ato-mcp"])
    monkeypatch.setattr(power.os, "execvpe", fail_exec)

    power.maybe_reexec_with_sleep_inhibitor("ato-mcp corpus rebuild")

    assert called is False
