#!/usr/bin/env python3
"""Executable fake remote host for configure-azure-host.sh tests."""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
import tempfile
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "configure-azure-host.sh"


_DISPATCHER = r'''#!/usr/bin/env python3
import json
import os
import pathlib
import shutil
import subprocess
import sys
import uuid

fixture_root = pathlib.Path(os.environ["AZURE_HOST_FIXTURE_ROOT"])
fs_root = fixture_root / "fs"
state_path = fixture_root / "state.json"
mutation_path = fixture_root / "mutations.log"
command = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]


def load_state():
    return json.loads(state_path.read_text())


def save_state(state):
    state_path.write_text(json.dumps(state, sort_keys=True) + "\n")


def remote_path(value):
    path = pathlib.Path(value)
    if not path.is_absolute():
        return path
    state = load_state()
    remote_stage = state.get("remote_stage", "")
    if remote_stage and (value == remote_stage or value.startswith(remote_stage + "/")):
        return path
    return fs_root / value.lstrip("/")


def record_mutation(words):
    with mutation_path.open("a") as log:
        log.write(" ".join(words) + "\n")


def run_ssh():
    remote_command = args[-1]
    state = load_state()
    if remote_command == "mktemp -d /tmp/legal-mcp-install.XXXXXXXX":
        stage = pathlib.Path("/tmp") / ("legal-mcp-install." + uuid.uuid4().hex)
        stage.mkdir(mode=0o700)
        state["remote_stage"] = str(stage)
        save_state(state)
        print(stage)
        return 0
    if remote_command.startswith("REMOTE_STAGE=") and remote_command.endswith(" bash -s"):
        stage = state.get("remote_stage")
        if not stage:
            print("fixture remote stage was not created", file=sys.stderr)
            return 90
        script = sys.stdin.buffer.read()
        environment = os.environ.copy()
        environment["REMOTE_STAGE"] = stage
        completed = subprocess.run(
            ["/usr/bin/bash", "-s"], input=script, env=environment
        )
        return completed.returncode
    if remote_command.startswith("rm -rf '"):
        stage = state.get("remote_stage", "")
        if stage:
            shutil.rmtree(stage, ignore_errors=True)
        return 0
    print(f"unexpected fake ssh command: {remote_command}", file=sys.stderr)
    return 91


def run_scp():
    remaining = list(args)
    while remaining and remaining[0] == "-o":
        remaining = remaining[2:]
    if len(remaining) < 2:
        return 92
    sources = [pathlib.Path(item) for item in remaining[:-1]]
    stage_value = load_state().get("remote_stage", "")
    if not stage_value:
        return 93
    stage = pathlib.Path(stage_value)
    for source in sources:
        shutil.copy2(source, stage / source.name)
    return 0


def install(arguments):
    directory = False
    mode = None
    paths = []
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == "-d":
            directory = True
            index += 1
        elif argument in {"-o", "-g", "-m"}:
            if argument == "-m":
                mode = int(arguments[index + 1], 8)
            index += 2
        else:
            paths.append(argument)
            index += 1
    if directory:
        for value in paths:
            destination = remote_path(value)
            destination.mkdir(parents=True, exist_ok=True)
            if mode is not None:
                destination.chmod(mode)
        return 0
    if len(paths) != 2:
        return 94
    source = remote_path(paths[0])
    destination = remote_path(paths[1])
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.is_symlink():
        destination.unlink()
    shutil.copyfile(source, destination)
    if mode is not None:
        destination.chmod(mode)
    return 0


def systemctl(arguments):
    state = load_state()
    action = arguments[0]
    unit = arguments[-1]
    if action == "is-enabled":
        if unit == "legal-mcp.service":
            print(state["legal_unit_file_state"])
            return state["legal_unit_file_status"]
        if unit == "caddy.service":
            print(state["caddy_enabled"])
            return 0 if state["caddy_enabled"] == "enabled" else 1
    if action == "is-active":
        active = state["legal_active"] if unit == "legal-mcp.service" else state["caddy_active"]
        print(active)
        return 0 if active == "active" else 3
    if action == "show":
        if state.get("systemctl_show_failure"):
            return 95
        property_name = ""
        for index, item in enumerate(arguments):
            if item.startswith("--property="):
                property_name = item.split("=", 1)[1]
                break
            if item == "--property" and index + 1 < len(arguments):
                property_name = arguments[index + 1]
                break
        if property_name == "LoadState":
            print(state["legal_load_state"])
            return 0
        if property_name == "FragmentPath":
            print(state["legal_fragment_path"])
            return 0
        return 96

    record_mutation(["systemctl", *arguments])
    if action == "disable" and unit == "caddy.service":
        state["caddy_enabled"] = "disabled"
        state["caddy_active"] = "inactive"
    elif action == "mask" and unit == "caddy.service":
        state["caddy_enabled"] = "masked"
        state["caddy_active"] = "inactive"
    elif action == "unmask" and unit == "caddy.service":
        state["caddy_enabled"] = "disabled"
    elif action == "daemon-reload":
        quadlet = None
        for directory in (
            "/run/containers/systemd",
            "/etc/containers/systemd",
            "/usr/share/containers/systemd",
        ):
            candidate = remote_path(directory + "/legal-mcp.container")
            if os.path.lexists(candidate):
                quadlet = candidate
                break
        native = remote_path("/etc/systemd/system/legal-mcp.service")
        if quadlet is not None:
            state["legal_unit_file_state"] = "generated"
            state["legal_unit_file_status"] = 0
            state["legal_load_state"] = "loaded"
            state["legal_fragment_path"] = "/run/systemd/generator/legal-mcp.service"
        elif os.path.exists(native):
            state["legal_unit_file_state"] = "disabled"
            state["legal_unit_file_status"] = 1
            state["legal_load_state"] = "loaded"
            state["legal_fragment_path"] = "/etc/systemd/system/legal-mcp.service"
        else:
            state["legal_unit_file_state"] = "not-found"
            state["legal_unit_file_status"] = 4
            state["legal_load_state"] = "not-found"
            state["legal_fragment_path"] = ""
    elif action == "stop" and unit == "legal-mcp.service":
        state["legal_active"] = "inactive"
    elif action == "enable" and unit == "legal-mcp.service":
        state["legal_unit_file_state"] = "enabled"
        state["legal_unit_file_status"] = 0
    else:
        print(f"unsupported fake systemctl invocation: {arguments}", file=sys.stderr)
        return 97
    save_state(state)
    return 0


def run_sudo():
    sudo_arguments = list(args)
    while sudo_arguments and "=" in sudo_arguments[0] and not sudo_arguments[0].startswith("/"):
        sudo_arguments.pop(0)
    if not sudo_arguments:
        return 98
    program = sudo_arguments[0]
    arguments = sudo_arguments[1:]
    if program == "mountpoint":
        return 0
    if program == "findmnt":
        print("xfs")
        return 0
    if program == "xfs_info":
        print("meta-data=/dev/fixture reflink=1")
        return 0
    if program == "test":
        if len(arguments) == 3 and arguments[1] == "=":
            return 0 if arguments[0] == arguments[2] else 1
        predicate, value = arguments
        path = remote_path(value)
        if predicate == "-e":
            return 0 if os.path.exists(path) else 1
        if predicate == "-L":
            return 0 if os.path.islink(path) else 1
        if predicate == "-f":
            return 0 if os.path.isfile(path) else 1
        return 99
    if program == "systemctl":
        return systemctl(arguments)
    if program == "install":
        record_mutation([program, *arguments])
        return install(arguments)
    if program == "tee":
        record_mutation([program, *arguments])
        destination = remote_path(arguments[-1])
        destination.parent.mkdir(parents=True, exist_ok=True)
        data = sys.stdin.buffer.read()
        destination.write_bytes(data)
        sys.stdout.buffer.write(data)
        return 0
    if program == "chmod":
        record_mutation([program, *arguments])
        remote_path(arguments[-1]).chmod(int(arguments[-2], 8))
        return 0
    if program == "rm":
        record_mutation([program, *arguments])
        for value in arguments[1:]:
            path = remote_path(value)
            try:
                path.unlink()
            except FileNotFoundError:
                pass
        return 0
    if program in {"apt-get", "env", "caddy", "visudo"}:
        if program == "apt-get":
            record_mutation([program, *arguments])
        return 0
    print(f"unsupported fake sudo invocation: {args}", file=sys.stderr)
    return 100


if command == "curl":
    output = pathlib.Path(args[args.index("--output") + 1])
    output.write_bytes(b"fixture caddy package\n")
    status = 0
elif command == "sha512sum":
    sys.stdin.buffer.read()
    status = 0
elif command == "ssh":
    status = run_ssh()
elif command == "scp":
    status = run_scp()
elif command == "sudo":
    status = run_sudo()
elif command == "caddy":
    if args == ["version"]:
        print("v2.11.4")
        status = 0
    else:
        status = 101
else:
    print(f"unsupported fixture command: {command}", file=sys.stderr)
    status = 102
sys.exit(status)
'''


class AzureHostFixture:
    QUADLET_DIRECTORIES = (
        "/run/containers/systemd",
        "/etc/containers/systemd",
        "/usr/share/containers/systemd",
    )

    def __init__(self) -> None:
        self._temporary = tempfile.TemporaryDirectory(prefix="azure-host-fixture-")
        self.root = pathlib.Path(self._temporary.name)
        self.fs_root = self.root / "fs"
        self.bin_dir = self.root / "bin"
        self.bin_dir.mkdir()
        self.mutation_path = self.root / "mutations.log"
        self.mutation_path.write_text("")
        self.state_path = self.root / "state.json"
        self._write_state(
            {
                "caddy_active": "inactive",
                "caddy_enabled": "disabled",
                "legal_active": "inactive",
                "legal_fragment_path": "",
                "legal_load_state": "not-found",
                "legal_unit_file_state": "not-found",
                "legal_unit_file_status": 4,
                "remote_stage": "",
                "systemctl_show_failure": False,
            }
        )
        dispatcher = self.bin_dir / "fixture-command"
        dispatcher.write_text(_DISPATCHER)
        dispatcher.chmod(0o755)
        for name in ("caddy", "curl", "scp", "sha512sum", "ssh", "sudo"):
            (self.bin_dir / name).symlink_to(dispatcher.name)

        self.write_file(
            "/var/lib/australian-legal-mcp/.legal-mcp-data-volume", "fixture\n"
        )
        for directory in (
            "/etc/australian-legal-mcp",
            "/etc/caddy",
            "/etc/sudoers.d",
            "/etc/systemd/system",
            *self.QUADLET_DIRECTORIES,
        ):
            self.path(directory).mkdir(parents=True, exist_ok=True)

        inputs = self.root / "inputs"
        inputs.mkdir()
        self.binary = inputs / "legal-mcp"
        self.binary.write_text("fixture legal-mcp binary\n")
        self.binary.chmod(0o755)
        self.onnx_runtime = inputs / "libonnxruntime.so"
        self.onnx_runtime.write_text("fixture ONNX Runtime\n")

    def close(self) -> None:
        state = self.state
        remote_stage = state.get("remote_stage", "")
        if remote_stage:
            shutil.rmtree(remote_stage, ignore_errors=True)
        self._temporary.cleanup()

    def __enter__(self) -> "AzureHostFixture":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    def path(self, remote_path: str) -> pathlib.Path:
        return self.fs_root / remote_path.lstrip("/")

    def write_file(self, remote_path: str, content: str) -> pathlib.Path:
        destination = self.path(remote_path)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(content)
        return destination

    def dangling_symlink(self, remote_path: str) -> pathlib.Path:
        destination = self.path(remote_path)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.symlink_to("/fixture-target-does-not-exist")
        return destination

    @property
    def state(self) -> dict[str, Any]:
        return json.loads(self.state_path.read_text())

    def set_systemd_state(
        self,
        *,
        unit_file_status: int,
        unit_file_state: str,
        load_state: str,
        fragment_path: str,
    ) -> None:
        state = self.state
        state.update(
            {
                "legal_fragment_path": fragment_path,
                "legal_load_state": load_state,
                "legal_unit_file_state": unit_file_state,
                "legal_unit_file_status": unit_file_status,
            }
        )
        self._write_state(state)

    def fail_systemctl_show(self) -> None:
        state = self.state
        state["systemctl_show_failure"] = True
        self._write_state(state)

    @property
    def mutations(self) -> list[str]:
        return self.mutation_path.read_text().splitlines()

    def run(self) -> subprocess.CompletedProcess[str]:
        environment = os.environ.copy()
        environment["AZURE_HOST_FIXTURE_ROOT"] = str(self.root)
        environment["PATH"] = str(self.bin_dir) + os.pathsep + environment["PATH"]
        return subprocess.run(
            [
                str(SCRIPT),
                "--host",
                "azureadmin@example.com",
                "--public-host",
                "fixture.australiaeast.cloudapp.azure.com",
                "--blob-base-url",
                "https://fixtureaccount.blob.core.windows.net/legal-corpus",
                "--binary",
                str(self.binary),
                "--onnx-runtime",
                str(self.onnx_runtime),
            ],
            cwd=ROOT,
            env=environment,
            text=True,
            capture_output=True,
            timeout=30,
        )

    def _write_state(self, state: dict[str, Any]) -> None:
        self.state_path.write_text(json.dumps(state, sort_keys=True) + "\n")
