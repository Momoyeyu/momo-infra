"""momo-svc-embed：BGE-M3（P3）；握手后一律 error not implemented。"""

from fastapi import FastAPI

from momo_models.config import get_settings
from momo_models.logging import configure_logging
from momo_models.services.common import make_app, serve


def create_app() -> FastAPI:
    settings = get_settings()
    configure_logging(settings.log_level, settings.log_format)
    return make_app(role="embed", settings=settings)


def main() -> None:
    settings = get_settings()
    app = create_app()
    serve(app, settings.embed_listen)


if __name__ == "__main__":
    main()
