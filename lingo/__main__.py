import logging
import sys
from logging.handlers import RotatingFileHandler

from .config import STATE_DIR


def main() -> int:
    STATE_DIR.mkdir(parents=True, exist_ok=True)
    handlers: list[logging.Handler] = [RotatingFileHandler(STATE_DIR / "lingo.log", maxBytes=1_000_000,
                                                           backupCount=2)]
    if sys.stderr.isatty():
        handlers.append(logging.StreamHandler())
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s",
                        handlers=handlers)
    from .ui import LingoApp

    return LingoApp().run(sys.argv)


if __name__ == "__main__":
    sys.exit(main())
