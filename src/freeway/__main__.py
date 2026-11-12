import argparse
import logging
import os

import uvicorn
from dotenv import find_dotenv, load_dotenv


def main() -> None:
    parser = argparse.ArgumentParser(prog="freeway")
    parser.add_argument("--config", default=None, help="path to config.yaml (default: $ROUTER_CONFIG or ./config.yaml)")
    parser.add_argument("--host", default=None)
    parser.add_argument("--port", type=int, default=None)
    args = parser.parse_args()

    load_dotenv(find_dotenv(usecwd=True))  # .env of the folder the server is started from
    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO"), format="%(asctime)s %(levelname)s %(name)s: %(message)s")

    from .app import create_app
    from .config import load_settings

    settings = load_settings(args.config)
    uvicorn.run(
        create_app(settings),
        host=args.host or settings.host,
        port=args.port or settings.port,
        log_level="warning",
    )


if __name__ == "__main__":
    main()
