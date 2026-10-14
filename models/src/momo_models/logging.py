"""统一日志：structlog；`MOMO_LOG_FORMAT=pretty|json`（config.md §2.1）。"""

import logging
import sys

import structlog

_configured = False


def configure_logging(level: str = "info", fmt: str = "pretty") -> None:
    """进程启动时调用一次；重复调用幂等。"""
    global _configured
    if _configured:
        return
    _configured = True

    logging.basicConfig(
        format="%(message)s",
        stream=sys.stderr,
        level=getattr(logging, level.upper(), logging.INFO),
    )

    shared: list[structlog.types.Processor] = [
        structlog.contextvars.merge_contextvars,
        structlog.processors.add_log_level,
        structlog.processors.TimeStamper(fmt="iso"),
        structlog.processors.StackInfoRenderer(),
        structlog.processors.format_exc_info,
    ]
    renderer: structlog.types.Processor
    if fmt == "json":
        renderer = structlog.processors.JSONRenderer()
    else:
        renderer = structlog.dev.ConsoleRenderer()

    structlog.configure(
        processors=[*shared, renderer],
        wrapper_class=structlog.make_filtering_bound_logger(
            getattr(logging, level.upper(), logging.INFO)
        ),
        logger_factory=structlog.PrintLoggerFactory(sys.stderr),
        cache_logger_on_first_use=True,
    )


def get_logger(name: str) -> structlog.stdlib.BoundLogger:
    """模块级入口：`log = get_logger(__name__)`。"""
    return structlog.get_logger(name)
