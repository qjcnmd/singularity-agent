from __future__ import annotations

import ctypes
import os
import shutil
import subprocess
from dataclasses import asdict, dataclass
from functools import lru_cache
from pathlib import Path
from typing import Any, Protocol

from singularity.sandbox.exceptions import SandboxCapabilityError, SandboxSetupError
from singularity.sandbox.models import (
    PreparedSandbox,
    SandboxCapabilities,
    SandboxRequest,
    SandboxResult,
)


class SandboxBackend(Protocol):
    def name(self) -> str:
        ...

    def capabilities(self) -> SandboxCapabilities:
        ...

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        ...

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        ...

    def cleanup(self, prepared: PreparedSandbox) -> None:
        ...


@dataclass(frozen=True)
class WindowsSandboxPrimitives:
    restricted_token: bool
    job_object: bool
    low_integrity: bool
    acl: bool
    firewall: bool
    private_desktop: bool


@dataclass(frozen=True)
class WindowsSandboxSetup:
    sandbox_account: bool
    acl_boundary: bool
    network_filter: bool
    private_desktop: bool
    execution_backend: bool


@dataclass(frozen=True)
class WindowsSandboxDoctorReport:
    implementation: str
    platform_supported: bool
    primitives: WindowsSandboxPrimitives
    setup: WindowsSandboxSetup
    available: bool
    missing_requirements: tuple[str, ...]

    def to_dict(self) -> dict[str, Any]:
        return {
            "implementation": self.implementation,
            "platform_supported": self.platform_supported,
            "primitives": asdict(self.primitives),
            "setup": asdict(self.setup),
            "available": self.available,
            "missing_requirements": list(self.missing_requirements),
        }

    @property
    def reason(self) -> str:
        if self.available:
            return "Windows sandbox backend is available."
        missing = ", ".join(self.missing_requirements) or "unknown requirements"
        return f"backend_unavailable: Windows sandbox requirements are missing: {missing}."


@lru_cache(maxsize=1)
def probe_windows_sandbox() -> WindowsSandboxDoctorReport:
    """Probe Windows primitives without changing accounts, ACLs, or firewall state.

    Singularity does not yet ship the privileged installer needed to establish
    the sandbox account, workspace ACL boundary, outbound firewall rules, and
    private desktop. Those setup facts therefore remain explicitly false. This
    prevents the former restricted-token/workspace-copy implementation from
    being mistaken for complete OS enforcement.
    """

    platform_supported = os.name == "nt"
    primitives = WindowsSandboxPrimitives(
        restricted_token=_has_windows_symbols(
            "advapi32",
            "OpenProcessToken",
            "CreateRestrictedToken",
            "CreateProcessAsUserW",
        ),
        job_object=_has_windows_symbols(
            "kernel32",
            "CreateJobObjectW",
            "SetInformationJobObject",
            "AssignProcessToJobObject",
            "TerminateJobObject",
        ),
        low_integrity=_has_windows_symbols(
            "advapi32",
            "ConvertStringSidToSidW",
            "SetTokenInformation",
        ),
        acl=platform_supported and shutil.which("icacls") is not None,
        firewall=_powershell_command_available("Get-NetFirewallRule"),
        private_desktop=_has_windows_symbols("user32", "CreateDesktopW", "CloseDesktop"),
    )
    setup = WindowsSandboxSetup(
        sandbox_account=False,
        acl_boundary=False,
        network_filter=False,
        private_desktop=False,
        execution_backend=False,
    )
    primitive_values = asdict(primitives)
    setup_values = asdict(setup)
    missing_items = [] if platform_supported else ["platform"]
    missing_items.extend(
        f"primitive:{name}" for name, ready in primitive_values.items() if not ready
    )
    missing_items.extend(f"setup:{name}" for name, ready in setup_values.items() if not ready)
    missing = tuple(missing_items)
    available = platform_supported and all(primitive_values.values()) and all(setup_values.values())
    return WindowsSandboxDoctorReport(
        implementation="elevated",
        platform_supported=platform_supported,
        primitives=primitives,
        setup=setup,
        available=available,
        missing_requirements=missing,
    )


class WindowsSandboxBackend:
    """Fail-closed boundary for the planned elevated Windows sandbox.

    This backend intentionally does not execute until every doctor requirement
    is configured and the native execution backend is implemented. A restricted
    token plus a copied workspace is not reported as filesystem or network
    isolation.
    """

    def name(self) -> str:
        return "windows"

    def doctor(self) -> WindowsSandboxDoctorReport:
        return probe_windows_sandbox()

    def setup(self) -> WindowsSandboxDoctorReport:
        report = self.doctor()
        raise SandboxSetupError(
            "elevated Windows sandbox setup is not implemented by this runtime; "
            f"{report.reason}"
        )

    def is_available(self) -> bool:
        return self.doctor().available

    def capabilities(self) -> SandboxCapabilities:
        return SandboxCapabilities(
            filesystem_isolation=False,
            copy_on_write=False,
            readonly_mount=False,
            network_isolation=False,
            env_isolation=False,
            process_tree_kill=False,
            timeout=False,
            output_limit=False,
            memory_limit=False,
            process_limit=False,
            artifact_capture=False,
            change_detection=False,
        )

    def prepare(self, request: SandboxRequest) -> PreparedSandbox:
        raise SandboxCapabilityError(self.doctor().reason)

    def run(self, prepared: PreparedSandbox) -> SandboxResult:
        raise SandboxCapabilityError(self.doctor().reason)

    def cleanup(self, prepared: PreparedSandbox) -> None:
        return None


def default_sandbox_backends() -> list[SandboxBackend]:
    if os.name != "nt":
        return []
    return [WindowsSandboxBackend()]


def _has_windows_symbols(library: str, *symbols: str) -> bool:
    if os.name != "nt":
        return False
    try:
        dll = ctypes.WinDLL(library, use_last_error=True)
        return all(hasattr(dll, symbol) for symbol in symbols)
    except (AttributeError, OSError):
        return False


def _powershell_command_available(command: str) -> bool:
    if os.name != "nt":
        return False
    executable = shutil.which("powershell") or shutil.which("pwsh")
    if executable is None:
        return False
    try:
        completed = subprocess.run(
            [
                executable,
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                f"if (Get-Command {command} -ErrorAction SilentlyContinue) {{ exit 0 }}; exit 1",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    return completed.returncode == 0
