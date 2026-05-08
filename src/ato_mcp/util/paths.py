"""XDG-aware path resolution for the ato-mcp data directory."""
from __future__ import annotations

import os
from pathlib import Path

from platformdirs import user_data_dir

APP_NAME = "ato-mcp"


def data_dir() -> Path:
    """Return the configured data directory, creating it if needed.

    Resolution order: $ATO_MCP_DATA_DIR, $XDG_DATA_HOME/ato-mcp, platformdirs default.
    """
    override = os.environ.get("ATO_MCP_DATA_DIR")
    if override:
        path = Path(override).expanduser()
    else:
        path = Path(user_data_dir(APP_NAME, appauthor=False))
    path.mkdir(parents=True, exist_ok=True)
    return path


def live_dir() -> Path:
    p = data_dir() / "live"
    p.mkdir(parents=True, exist_ok=True)
    return p


def db_path() -> Path:
    return live_dir() / "ato.db"


def model_path() -> Path:
    return live_dir() / "model_quantized.onnx"


def tokenizer_path() -> Path:
    return live_dir() / "tokenizer.json"
