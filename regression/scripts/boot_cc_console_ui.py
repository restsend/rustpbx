"""Boot a CC-enabled PBX for manual / Playwright console-UI verification.

Boots the same layout the tier3 CC fixtures use (file DB so resources
persist across HTTP pulls), prints the console URL, then idles until
SIGTERM/SIGINT so an external Playwright run can drive the UI.

    python3 regression/scripts/boot_cc_console_ui.py &
    CONSOLE_URL=http://127.0.0.1:18080/console npx playwright test ...
"""

from __future__ import annotations

import os
import signal
import sys
import time
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(PROJECT_ROOT / "regression"))

import helpers as h  # noqa: E402
from helpers.config_builder import ConfigBuilder  # noqa: E402
from helpers.pbx_server import PbxServer  # noqa: E402

SIP_PORT = int(os.environ.get("UI_SIP_PORT", "18070"))
HTTP_PORT = int(os.environ.get("UI_HTTP_PORT", "18080"))
WORK_DIR = Path(os.environ.get("UI_WORK_DIR", str(PROJECT_ROOT / "target" / "cc-console-ui")))
RWI_TOKEN = "cc-console-ui-token"


def main() -> int:
    WORK_DIR.mkdir(parents=True, exist_ok=True)
    cb = ConfigBuilder(
        project_root=PROJECT_ROOT,
        work_dir=WORK_DIR,
        sip_port=SIP_PORT,
        http_port=HTTP_PORT,
        rwi_token=RWI_TOKEN,
        webhook_url="",
        # demo_mode → the bootstrap superuser (demo@miuda.ai / hello@miuda.ai)
        # the Playwright login helper expects.
        addons=["cc"],
    )
    cb.database_url = f"sqlite://{WORK_DIR}/cc-console-ui.db?mode=rwc"
    cb.demo_mode = True

    server = PbxServer(
        host="127.0.0.1",
        sip_port=SIP_PORT,
        http_port=HTTP_PORT,
        rwi_token=RWI_TOKEN,
        project_root=PROJECT_ROOT,
        work_dir=WORK_DIR,
    )
    server._config_builder = cb
    server.prepare(webhook_url="", build=False)
    # The builder hardcodes demo_mode = false; flip it so the bootstrap
    # superuser (demo@miuda.ai) exists for the Playwright login helper.
    conf = server.work_dir / "rustpbx_regression.toml"
    conf.write_text(
        conf.read_text(encoding="utf-8").replace("demo_mode = false", "demo_mode = true"),
        encoding="utf-8",
    )
    # The server runs with cwd = work_dir, so `/static/*` (console.css, icons)
    # resolves to work_dir/static — symlink the repo's static dir or the
    # console renders unstyled.
    static_link = WORK_DIR / "static"
    if not static_link.exists():
        static_link.symlink_to(PROJECT_ROOT / "static", target_is_directory=True)
    server.start(timeout=90)

    print(f"CONSOLE_URL={server.http_url}/console", flush=True)
    print(f"API_BASE={server.http_url}/api", flush=True)

    stop = {"flag": False}

    def _stop(signum, _frame):
        stop["flag"] = True

    signal.signal(signal.SIGTERM, _stop)
    signal.signal(signal.SIGINT, _stop)
    try:
        while not stop["flag"]:
            time.sleep(0.5)
    finally:
        server.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
