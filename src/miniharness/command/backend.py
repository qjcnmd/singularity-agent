from __future__ import annotations

import os
import queue
import signal
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from uuid import uuid4

from miniharness.command.models import CommandRequest, ResourceLimits
from miniharness.command.output import OutputCollector


@dataclass(frozen=True)
class BackendRunResult:
    exit_code: int | None
    signal: int | None
    timed_out: bool = False
    idle_timed_out: bool = False
    killed_reason: str | None = None
    error_code: str | None = None
    error_message: str | None = None


@dataclass
class RunningProcess:
    process_id: str
    process: subprocess.Popen[bytes] | None
    request: CommandRequest
    cwd: Path
    collector: OutputCollector
    reader_threads: list[threading.Thread]
    output_queue: queue.Queue[tuple[str, bytes]]
    started_at_monotonic: float
    owner_transaction: str | None = None
    start_error_code: str | None = None
    start_error_message: str | None = None


class ExecutionBackend:
    name = "abstract"

    def execute(
        self,
        *,
        request: CommandRequest,
        cwd: Path,
        env: dict[str, str],
        collector: OutputCollector,
        cancellation_token: object | None = None,
    ) -> BackendRunResult:
        raise NotImplementedError

    def start(
        self,
        *,
        request: CommandRequest,
        cwd: Path,
        env: dict[str, str],
        collector: OutputCollector,
        owner_transaction: str | None = None,
    ) -> RunningProcess:
        raise NotImplementedError


class SandboxBackend(ExecutionBackend):
    name = "sandbox"

    def execute(
        self,
        *,
        request: CommandRequest,
        cwd: Path,
        env: dict[str, str],
        collector: OutputCollector,
        cancellation_token: object | None = None,
    ) -> BackendRunResult:
        return BackendRunResult(
            exit_code=None,
            signal=None,
            error_code="sandbox_unavailable",
            error_message="Sandbox backend interface is reserved but not implemented.",
        )


class LocalProcessBackend(ExecutionBackend):
    name = "local_process"

    def __init__(self) -> None:
        self.supervisor = ProcessSupervisor()

    def execute(
        self,
        *,
        request: CommandRequest,
        cwd: Path,
        env: dict[str, str],
        collector: OutputCollector,
        cancellation_token: object | None = None,
    ) -> BackendRunResult:
        running = self.start(
            request=request,
            cwd=cwd,
            env=env,
            collector=collector,
        )
        if running.process is None:
            return BackendRunResult(
                exit_code=None,
                signal=None,
                error_code=running.start_error_code,
                error_message=running.start_error_message,
            )
        return self._monitor_until_exit(
            running,
            limits=request.resource_limits,
            cancellation_token=cancellation_token,
        )

    def start(
        self,
        *,
        request: CommandRequest,
        cwd: Path,
        env: dict[str, str],
        collector: OutputCollector,
        owner_transaction: str | None = None,
    ) -> RunningProcess:
        output_queue: queue.Queue[tuple[str, bytes]] = queue.Queue()
        try:
            process = self.supervisor.start_process(request, cwd=cwd, env=env)
        except FileNotFoundError as exc:
            return RunningProcess(
                process_id=uuid4().hex,
                process=None,
                request=request,
                cwd=cwd,
                collector=collector,
                reader_threads=[],
                output_queue=output_queue,
                started_at_monotonic=time.perf_counter(),
                owner_transaction=owner_transaction,
                start_error_code="command_not_found",
                start_error_message=str(exc),
            )
        except PermissionError as exc:
            return RunningProcess(
                process_id=uuid4().hex,
                process=None,
                request=request,
                cwd=cwd,
                collector=collector,
                reader_threads=[],
                output_queue=output_queue,
                started_at_monotonic=time.perf_counter(),
                owner_transaction=owner_transaction,
                start_error_code="permission_error",
                start_error_message=str(exc),
            )
        except Exception as exc:
            return RunningProcess(
                process_id=uuid4().hex,
                process=None,
                request=request,
                cwd=cwd,
                collector=collector,
                reader_threads=[],
                output_queue=output_queue,
                started_at_monotonic=time.perf_counter(),
                owner_transaction=owner_transaction,
                start_error_code="spawn_failed",
                start_error_message=str(exc),
            )

        threads = [
            _reader_thread("stdout", process.stdout, output_queue),
            _reader_thread("stderr", process.stderr, output_queue),
        ]
        return RunningProcess(
            process_id=uuid4().hex,
            process=process,
            request=request,
            cwd=cwd,
            collector=collector,
            reader_threads=threads,
            output_queue=output_queue,
            started_at_monotonic=time.perf_counter(),
            owner_transaction=owner_transaction,
        )

    def poll_output(self, running: RunningProcess) -> None:
        while True:
            try:
                stream, chunk = running.output_queue.get_nowait()
            except queue.Empty:
                return
            running.collector.add(stream, chunk)

    def stop(self, running: RunningProcess, *, reason: str = "stopped") -> int | None:
        process = running.process
        if process is None:
            return None
        self._terminate_and_wait(process, reason=reason)
        self.poll_output(running)
        return process.returncode

    def _monitor_until_exit(
        self,
        running: RunningProcess,
        *,
        limits: ResourceLimits,
        cancellation_token: object | None = None,
    ) -> BackendRunResult:
        process = running.process
        if process is None:
            return BackendRunResult(exit_code=None, signal=None, error_code="spawn_failed")

        started = running.started_at_monotonic
        last_output = started
        timed_out = False
        idle_timed_out = False
        killed_reason: str | None = None

        while True:
            try:
                _throw_if_cancelled(cancellation_token)
            except Exception:
                killed_reason = "cancelled"
                self.supervisor.kill_process_tree(process, reason=killed_reason)
                raise
            saw_output = self._drain_available_output(running)
            if saw_output:
                last_output = time.perf_counter()
            if process.poll() is not None:
                self._drain_until_threads_quiet(running)
                break
            now = time.perf_counter()
            if now - started > limits.timeout_seconds:
                timed_out = True
                killed_reason = "timeout"
                self.supervisor.kill_process_tree(process, reason=killed_reason)
                break
            if (
                limits.idle_timeout_seconds is not None
                and now - last_output > limits.idle_timeout_seconds
            ):
                idle_timed_out = True
                killed_reason = "idle_timeout"
                self.supervisor.kill_process_tree(process, reason=killed_reason)
                break
            time.sleep(0.02)

        if process.poll() is None:
            killed_reason = killed_reason or "force_kill"
            self._terminate_and_wait(process, reason=killed_reason)
        else:
            process.wait(timeout=0)
        self._drain_until_threads_quiet(running)
        return_code = process.returncode
        process_signal = -return_code if return_code is not None and return_code < 0 else None
        return BackendRunResult(
            exit_code=return_code,
            signal=process_signal,
            timed_out=timed_out,
            idle_timed_out=idle_timed_out,
            killed_reason=killed_reason,
        )

    def _drain_available_output(self, running: RunningProcess) -> bool:
        saw_output = False
        while True:
            try:
                stream, chunk = running.output_queue.get_nowait()
            except queue.Empty:
                break
            running.collector.add(stream, chunk)
            saw_output = True
        return saw_output

    def _drain_until_threads_quiet(self, running: RunningProcess) -> None:
        deadline = time.perf_counter() + 1
        while time.perf_counter() < deadline:
            self._drain_available_output(running)
            if not any(thread.is_alive() for thread in running.reader_threads):
                break
            time.sleep(0.01)
        self._drain_available_output(running)

    def _terminate_and_wait(
        self,
        process: subprocess.Popen[bytes],
        *,
        reason: str,
    ) -> int | None:
        self.supervisor.kill_process_tree(process, reason=reason)
        try:
            return process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        try:
            if process.poll() is None:
                process.kill()
        except Exception:
            pass
        try:
            return process.wait(timeout=1)
        except subprocess.TimeoutExpired:
            return process.returncode


class ProcessSupervisor:
    def start_process(
        self,
        request: CommandRequest,
        *,
        cwd: Path,
        env: dict[str, str],
    ) -> subprocess.Popen[bytes]:
        if request.argv is not None:
            command: str | list[str] = request.argv
            shell = False
        elif request.shell is not None:
            command = request.shell
            shell = True
        else:
            raise ValueError("Command request must provide argv or shell.")

        kwargs: dict[str, object] = {
            "cwd": str(cwd),
            "env": env,
            "stdin": subprocess.DEVNULL,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
            "shell": shell,
        }
        if os.name == "nt":
            kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        else:
            kwargs["start_new_session"] = True
        return subprocess.Popen(command, **kwargs)

    def kill_process_tree(
        self,
        process: subprocess.Popen[bytes],
        *,
        reason: str,
    ) -> None:
        if process.poll() is not None:
            return
        if os.name == "nt":
            self._kill_windows_tree(process.pid)
            return
        self._kill_posix_tree(process.pid, process, reason=reason)

    @staticmethod
    def _kill_windows_tree(pid: int) -> None:
        try:
            subprocess.run(
                ["taskkill", "/PID", str(pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=5,
            )
        except Exception:
            pass

    @staticmethod
    def _kill_posix_tree(
        pid: int,
        process: subprocess.Popen[bytes],
        *,
        reason: str,
    ) -> None:
        try:
            os.killpg(os.getpgid(pid), signal.SIGTERM)
        except ProcessLookupError:
            return
        except Exception:
            process.terminate()
        try:
            process.wait(timeout=1 if reason != "force_kill" else 0.2)
            return
        except subprocess.TimeoutExpired:
            pass
        try:
            os.killpg(os.getpgid(pid), signal.SIGKILL)
        except Exception:
            process.kill()


def _reader_thread(
    stream_name: str,
    pipe: object,
    output_queue: queue.Queue[tuple[str, bytes]],
) -> threading.Thread:
    def run() -> None:
        if pipe is None:
            return
        while True:
            try:
                chunk = pipe.read(1)  # type: ignore[attr-defined]
            except ValueError:
                return
            if not chunk:
                return
            output_queue.put((stream_name, chunk))

    thread = threading.Thread(target=run, name=f"command-{stream_name}-reader", daemon=True)
    thread.start()
    return thread


def _throw_if_cancelled(cancellation_token: object | None) -> None:
    if cancellation_token is not None and hasattr(cancellation_token, "throw_if_cancelled"):
        cancellation_token.throw_if_cancelled()
