from __future__ import annotations

import singularity.sandbox.windows as _windows


def _runner_smoke_state(identity):
    if _windows.os.name != "nt":
        return _windows._missing(
            "Windows runner smoke requires Windows.",
            {"runner": "windows_runner.py"},
        )
    runner = _windows._runner_state()
    if not runner.ready:
        return runner
    sid = _windows._account_sid(identity.account_name)
    if not _windows._credential_state(identity).ready or not sid:
        state_dir = _windows._windows_state_dir_path()
        return _windows._missing(
            "Windows runner smoke requires sandbox account and credential.",
            _windows._probe_evidence(
                "runner_smoke_prerequisites",
                state_dir=state_dir,
                probe_root=state_dir / "runner-smoke",
                extra={"runner": "windows_runner.py"},
            ),
        )
    state_dir = _windows._windows_state_dir_path()
    root = state_dir / "runner-smoke"
    try:
        state_dir = _windows._windows_state_dir()
        root = state_dir / "runner-smoke"
        root.mkdir(parents=True, exist_ok=True)
        acl = _windows._apply_probe_root_acl(
            root,
            account_names=(identity.account_name,),
            operation="runner_smoke_acl",
        )
        if not acl.ok:
            return _windows._missing(
                "Windows runner smoke ACL setup failed.",
                {
                    **_windows._probe_evidence(
                        "runner_smoke_acl",
                        state_dir=state_dir,
                        probe_root=root,
                    ),
                    "runner": "windows_runner.py",
                    "reason": acl.reason,
                    "details": acl.details,
                },
            )
        spec_path = root / "runner-spec.json"
        result_path = root / "runner-result.json"
        spec = _windows.WindowsRunnerSpec(
            command=[_windows.sys.executable, "-c", "print('sandbox-smoke')"],
            cwd=str(root),
            env=_windows.WindowsSandboxBackend._runtime_env({}),
            timeout_seconds=5,
            max_output_chars=2000,
            network_mode="allowed",
            result_path=str(result_path),
        )
        try:
            spec_path.write_text(
                _windows.json.dumps(spec.to_dict(), ensure_ascii=False),
                encoding="utf-8",
            )
        except OSError as exc:
            return _windows._missing(
                "Windows runner smoke spec could not be written.",
                _windows._exception_diagnostics(
                    "runner_smoke_spec_write",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=spec_path,
                ),
            )
        prepared = _windows.SimpleNamespace(
            sandbox_root=root,
            baseline={
                "runner_spec": str(spec_path),
                "runner_result": str(result_path),
                "sandbox_account": identity.account_name,
                "credential_target": identity.credential_target,
                "sandbox_role": identity.role,
            },
            request=_windows.SimpleNamespace(
                profile=_windows.SimpleNamespace(
                    resources=_windows.SandboxResourceLimits(timeout_seconds=5)
                )
            ),
        )
        try:
            result = _windows.WindowsSandboxRunner(
                account_name=identity.account_name,
                credential_target=identity.credential_target,
            ).run(prepared)
        except Exception as exc:
            return _windows._missing(
                "Windows account-backed runner smoke failed.",
                _windows._account_runner_launch_exception_diagnostics(
                    "runner_smoke",
                    exc,
                    state_dir=state_dir,
                    probe_root=root,
                    path=root,
                ),
            )
        account_sid_hash = result.metadata.get("account_sid_hash")
        account_identity_verified = bool(account_sid_hash) and account_sid_hash == _windows._hash_sid(sid)
        ready = (
            result.exit_code == 0
            and "sandbox-smoke" in result.stdout
            and bool(result.metadata.get("restricted_token"))
            and bool(result.metadata.get("low_integrity"))
            and bool(result.metadata.get("private_desktop"))
            and bool(result.metadata.get("job_object"))
            and account_identity_verified
        )
        return _windows._state_from_bool(
            ready,
            "Windows account-backed runner smoke passed.",
            "Windows account-backed runner smoke failed.",
            _windows._runner_result_summary(
                _windows._runner_result_operation("runner_smoke", result),
                result,
                state_dir=state_dir,
                probe_root=root,
                path=root,
                extra={"account_identity_verified": account_identity_verified},
            ),
        )
    except Exception as exc:
        return _windows._missing(
            "Windows account-backed runner smoke failed.",
            _windows._account_runner_launch_exception_diagnostics(
                "runner_smoke",
                exc,
                state_dir=state_dir,
                probe_root=root,
                path=root,
            ),
        )
    finally:
        _windows._cleanup_probe_root(root)
