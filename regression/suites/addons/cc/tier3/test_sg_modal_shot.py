"""Edit-Skill-Group modal layout — the General tab was compacted so the
Save/Update button stays inside the viewport WITHOUT scrolling.

Opens the real console SPA, edits the `support` skill group, and asserts the
submit button's bounding box sits fully inside the browser viewport. Also
captures a screenshot of the modal for human review.
"""

from __future__ import annotations

import pytest
import pytest_asyncio

pytestmark = [pytest.mark.tier3]

SHOT_DIR = "/Users/pi/workspace/rs/rustpbx/screenshots/cc-modal-shots"


@pytest_asyncio.fixture
async def cc_api(pbx, webhook_server):
    pbx.config_builder.database_url = f"sqlite://{pbx.work_dir}/cc-modal.db?mode=rwc"
    # The console SPA template resolves CWD-relative `src/addons/cc/templates`
    # — the regression work_dir needs the same symlinks the dev layout has.
    server = pbx
    server.prepare(webhook_url=webhook_server.url, build=False)
    nested = server.work_dir / "src" / "addons" / "cc"
    nested.mkdir(parents=True, exist_ok=True)
    for asset in ("templates", "static"):
        link = nested / asset
        target = pbx.project_root / "src" / "addons" / "cc" / asset
        if target.is_dir() and not link.exists():
            link.symlink_to(target, target_is_directory=True)
    # Console Tailwind (static/css/console.css) is served CWD-relative too.
    static_link = server.work_dir / "static"
    if not static_link.exists():
        static_link.symlink_to(pbx.project_root / "static", target_is_directory=True)
    server.start(timeout=90)

    import aiohttp
    from helpers.pbx_server import PbxApiClient

    session = aiohttp.ClientSession()
    client = PbxApiClient(session, pbx.http_url, pbx.rwi_token)
    assert await client.ensure_console_auth(), "console superuser auth failed"
    seed = await client.seed_default_agents()
    assert seed.get("agents", 0) >= 1, f"agent seeding failed: {seed}"
    client.pbx = pbx
    yield client
    await session.close()


@pytest.mark.asyncio
async def test_sg_modal_general_tab_save_visible(pbx, cc_api, page, browser_context, evidence):
    # Share the console session with the browser (SPA api() calls + page load).
    cookies = cc_api._cookies or {}
    await browser_context.add_cookies([
        {"name": k, "value": v, "url": pbx.http_url} for k, v in cookies.items()
    ])

    await page.goto(f"{pbx.http_url}/console/cc#skill-groups")
    try:
        await page.wait_for_selector("button[title='Edit skill group']", timeout=20000)
    except Exception:
        await page.screenshot(path="/tmp/sg_debug.png")
        body = await page.inner_text("body")
        raise AssertionError(
            f"SPA edit button missing. body[:600]={body[:600]!r}"
        )
    await page.click("button[title='Edit skill group']")
    await page.wait_for_selector("#skillgroup-modal")
    # pickers (skills / QC) mount async — let the tab settle before measuring.
    await page.wait_for_timeout(800)

    btn = page.locator("#skillgroup-modal button[type=submit]")
    assert await btn.is_visible(), "submit button not rendered"
    box = await btn.bounding_box()
    vp = page.viewport_size
    assert box is not None, "submit button has no bounding box"
    import os
    os.makedirs(SHOT_DIR, exist_ok=True)
    await page.screenshot(path=f"{SHOT_DIR}/sg-modal-general.png")

    inside = 0 <= box["y"] and box["y"] + box["height"] <= vp["height"]
    assert inside, (
        f"Update button below the fold — user must scroll to save. "
        f"button={box} viewport={vp}"
    )
    evidence.log_metric("save_btn", {"y": box["y"], "viewport_h": vp["height"]})
