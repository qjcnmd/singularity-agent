from __future__ import annotations

import os
import queue
import signal
import subprocess
import threading
import time
from contextlib import suppress
from dataclasses import dataclass
from pathlib import Path
from uuid import uuid4

from singularity.command.models import CommandRequest, ResourceLimits
from singularity.command.output import OutputCollector
from singularity.kernel.cancellation import throw_if_cancelled

_PIPE_READ_CHUNK_SIZE = 8192
PROCESS_POLL_INTERVAL_SECONDS = 0.02
READER_DRAIN_TIMEOUT_SECONDS = 1.0
READER_DRAIN_POLL_INTERVAL_SECONDS = 0.01
READER_JOIN_TIMEOUT_SECONDS = 1.0


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
    collector: OutputCollector | None
    reader_threads: list[threading.Thread]
    output_queue: queue.Queue[tuple[str, bytes]] | None
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
        if running.output_queue is None or running.collector is None:
            return
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
        self._drain_until_threads_quiet(running)
        return_code = process.returncode
        self.release(running)
        return return_code

    def release(self, running: RunningProcess) -> None:
        self._close_process_pipes(running.process)
        self._join_reader_threads(running)
        running.reader_threads.clear()
        running.output_queue = None
        if running.process is not None:
            with suppress(Exception):
                running.process.close()
        running.process = None

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
                throw_if_cancelled(cancellation_token)
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
            time.sleep(PROCESS_POLL_INTERVAL_SECONDS)

        if process.poll() is None:
            killed_reason = killed_reason or "force_kill"
            self._terminate_and_wait(process, reason=killed_reason)
        else:
            process.wait(timeout=0)
        self._drain_until_threads_quiet(running)
        return_code = process.returncode
        process_signal = -return_code if return_code is not None and return_code < 0 else None
        result = BackendRunResult(
            exit_code=return_code,
            signal=process_signal,
            timed_out=timed_out,
            idle_timed_out=idle_timed_out,
            killed_reason=killed_reason,
        )
        self.release(running)
        return result

    def _drain_available_output(self, running: RunningProcess) -> bool:
        if running.output_queue is None or running.collector is None:
            return False
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
        deadline = time.perf_counter() + READER_DRAIN_TIMEOUT_SECONDS
        while time.perf_counter() < deadline:
            self._drain_available_output(running)
            if not any(thread.is_alive() for thread in running.reader_threads):
                break
            time.sleep(READER_DRAIN_POLL_INTERVAL_SECONDS)
        self._drain_available_output(running)
        self._join_reader_threads(running)

    @staticmethod
    def _close_process_pipes(process: subprocess.Popen[bytes] | None) -> None:
        if process is None:
            return
        for pipe in (process.stdout, process.stderr):
            if pipe is not None:
                with suppress(Exception):
                    pipe.close()

    @staticmethod
    def _join_reader_threads(running: RunningProcess) -> None:
        deadline = time.perf_counter() + READER_JOIN_TIMEOUT_SECONDS
        for thread in list(running.reader_threads):
            remaining = max(0.0, deadline - time.perf_counter())
            thread.join(timeout=remaining)

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

        return subprocess.Popen(
            command,
            cwd=str(cwd),
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            shell=shell,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0,
            start_new_session=os.name != "nt",
        )

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
        with suppress(Exception):
            subprocess.run(
                ["taskkill", "/PID", str(pid), "/T", "/F"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
                timeout=5,
            )

    @staticmethod
    def _kill_posix_tree(
        pid: int,
        process: subprocess.Popen[bytes],
        *,
        reason: str,
    ) -> None:
        killpg = getattr(os, "killpg", None)
        getpgid = getattr(os, "getpgid", None)
        if not callable(killpg) or not callable(getpgid):
            process.terminate()
            return
        try:
            killpg(getpgid(pid), signal.SIGTERM)
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
            killpg(getpgid(pid), getattr(signal, "SIGKILL", signal.SIGTERM))
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
        read = getattr(pipe, "read1", None)
        if not callable(read):
            read = pipe.read  # type: ignore[attr-defined]
        while True:
            try:
                chunk = read(_PIPE_READ_CHUNK_SIZE)
            except ValueError:
                return
            if not chunk:
                return
            output_queue.put((stream_name, chunk))

    thread = threading.Thread(target=run, name=f"command-{stream_name}-reader", daemon=True)
    thread.start()
    return thread
