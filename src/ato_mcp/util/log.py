"""Thin logging helper — stderr only, respects $ATO_MCP_LOG_LEVEL."""
from __future__ import annotations

import logging
import os
import sys


def get_logger(name: str) -> logging.Logger:
    logger = logging.getLogger(name)
    if not logger.handlers:
        handler = logging.StreamHandler(sys.stderr)
        handler.setFormatter(
            logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
        )
        logger.addHandler(handler)
        logger.setLevel(os.environ.get("ATO_MCP_LOG_LEVEL", "INFO").upper())
        logger.propagate = False
    return logger
