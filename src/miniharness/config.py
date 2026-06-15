from __future__ import annotations

import os

from pydantic import BaseModel


class Settings(BaseModel):
    base_url: str
    api_key: str
    model: str

    @classmethod
    def from_env(cls) -> "Settings":
        missing = [
            name
            for name in (
                "MINIHARNESS_BASE_URL",
                "MINIHARNESS_API_KEY",
                "MINIHARNESS_MODEL",
            )
            if not os.getenv(name)
        ]
        if missing:
            raise RuntimeError(
                "Missing required environment variables: " + ", ".join(missing)
            )

        return cls(
            base_url=os.environ["MINIHARNESS_BASE_URL"],
            api_key=os.environ["MINIHARNESS_API_KEY"],
            model=os.environ["MINIHARNESS_MODEL"],
        )
