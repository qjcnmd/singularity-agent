"""Operator key management for remote approval grant signing.

The operator key binds the approver identity to a cryptographic secret,
preventing grant forgery by processes that can write to the grant store.
The key file lives outside the workspace (default ~/.singularity/policy/operator.pem)
and must be distributed via secure channels at deployment time.
"""
from __future__ import annotations

import hashlib
import hmac
import json
import os
from pathlib import Path
from typing import Any


def default_operator_key_path() -> Path:
    """Return the default operator key path.

    Trust boundary: respects ``SINGULARITY_POLICY_HOME`` so tests and
    operators can redirect the key location without affecting
    ``Path.home()``. In production the key lives at
    ``~/.singularity/policy/operator.pem`` and must be distributed via
    secure channels at deployment time.
    """
    env_home = os.environ.get("SINGULARITY_POLICY_HOME")
    base = Path(env_home).expanduser() if env_home else Path.home()
    return base / ".singularity" / "policy" / "operator.pem"


def load_operator_key(path: Path | None = None) -> bytes:
    key_path = path or default_operator_key_path()
    if not key_path.exists():
        raise FileNotFoundError(
            f"Operator key not found at {key_path}. "
            "Generate one with: openssl rand -out operator.pem 32"
        )
    return key_path.read_bytes()


def generate_operator_key(path: Path | None = None) -> bytes:
    key_path = path or default_operator_key_path()
    key_path.parent.mkdir(parents=True, exist_ok=True)
    key = os.urandom(32)
    key_path.write_bytes(key)
    return key


def _canonical_payload(payload: dict[str, Any]) -> bytes:
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")


def sign_grant(grant_payload: dict[str, Any], operator_key: bytes) -> str:
    return hmac.new(operator_key, _canonical_payload(grant_payload), hashlib.sha256).hexdigest()


def verify_grant_signature(grant_payload: dict[str, Any], signature: str, operator_key: bytes) -> bool:
    expected = sign_grant(grant_payload, operator_key)
    return hmac.compare_digest(expected, signature)


def operator_fingerprint(operator_key: bytes) -> str:
    return hashlib.sha256(operator_key).hexdigest()[:16]
