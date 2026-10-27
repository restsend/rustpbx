"""ACD console tab — open / edit / take-effect verification with screenshots.

Walks the real console SPA through everything the ACD config-entry work added:

1. ACD tab renders the 全局设置 card (enabled / default_policy / strict_fifo /
   no_answer_cooldown_secs) next to the policy list.
2. Editing the card from the UI PUTs /cc/acd/config, persists cc/acd.toml
   (with the console_managed marker) and hot-reloads the engine.
3. The policy editor exposes the previously headless fields (priority extras,
   schedule, overflow triggers / sequential mode / escalation fair) and saves
   them through to cc/acd.toml.
4. Skill-group modal edits a break-catalog override (extra.phone) from the UI.
5. The Agents list shows the 今日小休 column (GET /cc/agents/breaks/today).

Screenshots land in screenshots/acd-console-shots/ for human review.
"""

from __future__ import annotations

import json

import pytest
import pytest_asyncio

pytestmark = [pytest.mark.tier3, pytest.mark.browser]

SHOT_DIR = "/Users/pi/workspace/rs/rustpbx/screenshots/acd-console-shots"
API_BASE = "/api/cc"


@pytest_asyncio.fixture
async def cc_api(pbx, webhook_server):
    pbx.config_builder.database_url = f"sqlite://{pbx.work_dir}/acd-shot.db?mode=rwc"
    server = pbx
    server.prepare(webhook_url=webhook_server.url, build=False)
    # The console SPA template resolves CWD-relative `src/addons/cc/templates`
    # — the regression work_dir needs the same symlinks the dev layout has.
    nested = server.work_dir / "src" / "addons" / "cc"
    nested.mkdir(parents=True, exist_ok=True)
    for asset in ("templates", "static", "locales"):
        link = nested / asset
        target = pbx.project_root / "src" / "addons" / "cc" / asset
        if target.is_dir() and not link.exists():
            link.symlink_to(target, target_is_directory=True)
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


def _policy_payload(name: str) -> dict:
    return {
        "name": name,
        "priority": {
            "vip_bonus": {},
            "wait_time_weight": 1.0,
            "base_priority": 0,
            "fifo_within_same_priority": True,
        },
        "strategy": {
            "strategy_type": "longest_idle",
            "skill_weights": None,
            "max_concurrent_calls": 1,
            "require_exact_skill": False,
            "skill_level_tie_break": "longest_idle",
        },
        "overflow": {
            "triggers": [],
            "chain": [],
            "retry_per_target": 1,
            "retry_interval_secs": 5,
            "mode": "replace",
            "escalation_timeline": [],
        },
        "schedule": {"business_hours": None, "holidays": {}, "night_mode": None},
        "available_states": ["idle"],
    }


async def _shot(page, path: str) -> None:
    import os

    os.makedirs(SHOT_DIR, exist_ok=True)
    await page.screenshot(path=f"{SHOT_DIR}/{path}", full_page=True)


@pytest.mark.asyncio
async def test_acd_console_open_edit_take_effect(pbx, cc_api, page, browser_context):
    # Console session + Chinese locale for the screenshots.
    cookies = cc_api._cookies or {}
    await browser_context.add_cookies(
        [{"name": k, "value": v, "url": pbx.http_url} for k, v in cookies.items()]
    )
    await browser_context.add_cookies(
        [{"name": "locale", "value": "zh", "url": pbx.http_url}]
    )

    # ── 0. Seed one policy through the API so default_policy has a target ──
    resp = await cc_api.post(
        f"{API_BASE}/acd/policies", _policy_payload("support")
    )
    assert resp.get("success"), f"seed policy failed: {resp}"

    # ── 1. ACD tab opens with the 全局设置 card + policy list ──
    await page.goto(f"{pbx.http_url}/console/cc#acd")
    try:
        await page.wait_for_selector("text=全局设置", timeout=20000)
        # The policy ROW (font-semibold name div), not the <option> in the
        # default-policy dropdown which also carries the same text.
        await page.wait_for_selector('div.font-semibold:text-is("support")', timeout=8000)
    except Exception:
        await _shot(page, "debug_acd_tab.png")
        tab_html = await page.inner_text("body")
        raise AssertionError(
            f"ACD tab incomplete (全局设置/policy missing); body[:1200]={tab_html[:1200]!r}"
        )
    await page.wait_for_timeout(500)
    await _shot(page, "acd_1_tab_overview.png")

    # ── 2. Edit global settings from the UI and save ──
    cooldown = page.locator('input[x-model\.number="config.no_answer_cooldown_secs"]')
    await cooldown.fill("15")
    fifo = page.locator('input[x-model="config.strict_fifo"]')
    if await fifo.is_checked():
        await fifo.uncheck()
    await page.locator('select[x-model="config.default_policy"]').select_option("support")
    async with page.expect_response("**/api/cc/acd/config") as resp_info:
        await page.locator("button.bg-emerald-600").click()
    save_resp = await resp_info.value
    assert save_resp.status == 200, "PUT /cc/acd/config failed"
    save_body = await save_resp.json()
    assert save_body.get("success"), save_body
    # The handler hot-reloads the engine — the message says so.
    assert "applied" in save_body.get("message", ""), save_body
    await page.wait_for_timeout(600)
    await _shot(page, "acd_2_global_saved.png")

    # ── 3. Take effect: API reflects the edit; file carries the marker ──
    cfg = await cc_api.get(f"{API_BASE}/acd/config")
    assert cfg["data"]["strict_fifo"] is False, cfg
    assert cfg["data"]["no_answer_cooldown_secs"] == 15, cfg
    assert cfg["data"]["default_policy"] == "support", cfg

    acd_file = pbx.work_dir / "config" / "cc" / "acd.toml"
    text = acd_file.read_text(encoding="utf-8")
    assert "console_managed = true" in text, text
    assert "strict_fifo = false" in text, text
    assert "no_answer_cooldown_secs = 15" in text, text
    # The console-managed enabled=false would be honored — absent here means
    # the card round-tripped enabled=true through the loader correctly.

    # ── 4. Policy editor: fill the previously headless fields ──
    await page.locator("button", has_text="新增策略").first.click()
    await page.wait_for_selector("text=策略名称", timeout=10000)
    await page.locator('input[x-model\.trim="editor.form.name"]').fill("vip_priority")
    await page.locator(
        'input[x-model\.number="editor.form.priority.base_priority"]'
    ).fill("10")
    # VIP bonus row
    await page.locator("button", has_text="添加 VIP 等级").first.click()
    await page.locator('input[x-model\.trim="row.key"]').first.fill("vip")
    await page.locator('input[placeholder="50"]').first.fill("50")
    # Business hours + one holiday
    await page.locator("label", has_text="工作时间").first.locator("input").check()
    await page.locator(
        'input[x-model="editor.form.schedule.business_hours.start"]'
    ).fill("08:30")
    await page.locator("button", has_text="添加节假日").first.click()
    try:
        await page.locator('input[x-model="row.key"]').first.fill(
            "2026-10-01", timeout=8000
        )
    except Exception:
        await _shot(page, "debug_policy_modal.png")
        modal_text = await page.locator(".fixed.inset-0.z-50").inner_text()
        raise AssertionError(f"holiday row missing; modal text: {modal_text[:800]!r}")
    await page.locator('input[x-model\.trim="row.name"]').first.fill("国庆节")
    await _shot(page, "acd_3_policy_modal_priority_schedule.png")

    # Overflow: trigger row + sequential escalation with a fair step
    await page.locator("button", has_text="添加触发条件").first.click()
    await page.locator('select[x-model="row.trigger_type"]').first.select_option("timeout")
    await page.locator('input[placeholder="120"]').first.fill("90")
    await page.locator('select[x-model="editor.form.overflow.mode"]').select_option(
        "sequential"
    )
    await page.locator("button", has_text="添加升级步骤").first.click()
    await page.locator('input[x-model\.number="step.threshold_secs"]').first.fill("30")
    await page.locator('input[x-model\.trim="step.skill_group_id"]').first.fill("senior")
    await page.locator("label", has_text="公平轮询").first.locator("input").check()
    await _shot(page, "acd_4_policy_modal_overflow_timeline.png")

    async with page.expect_response("**/api/cc/acd/policies") as resp_info:
        await page.locator("div.fixed button[type=submit]").click()
    assert (await resp_info.value).status == 200
    await page.wait_for_selector('div.font-semibold:text-is("vip_priority")', timeout=10000)
    await _shot(page, "acd_5_policy_list_saved.png")

    # ── 5. Take effect: every field persisted through the DTO ──
    policies = await cc_api.get(f"{API_BASE}/acd/policies")
    by_name = {p["name"]: p for p in policies["data"]}
    p = by_name["vip_priority"]
    assert p["priority"]["base_priority"] == 10
    assert p["priority"]["vip_bonus"] == {"vip": 50}
    assert p["overflow"]["mode"] == "sequential"
    assert p["overflow"]["triggers"][0]["trigger_type"] == "timeout"
    assert p["overflow"]["triggers"][0]["value"] == 90
    step = p["overflow"]["escalation_timeline"][0]
    assert step["threshold_secs"] == 30 and step["skill_group_id"] == "senior"
    assert step["fair"] is True
    assert p["schedule"]["business_hours"]["start"] == "08:30"
    assert p["schedule"]["holidays"]["2026-10-01"]["name"] == "国庆节"

    # ── 6. Round trip: reopen the editor and confirm the UI re-renders it ──
    row = page.locator(
        "div.flex.items-center.justify-between.px-6.py-4", has_text="vip_priority"
    )
    await row.locator("button", has_text="编辑").click()
    await page.wait_for_selector("text=策略名称", timeout=10000)
    assert await page.locator(
        'input[x-model\.trim="row.key"]'
    ).first.input_value() == "vip"
    assert await page.locator(
        'select[x-model="editor.form.overflow.mode"]'
    ).input_value() == "sequential"
    assert await page.locator(
        'input[x-model="editor.form.schedule.business_hours.start"]'
    ).input_value() == "08:30"
    await _shot(page, "acd_6_policy_edit_roundtrip.png")
    await page.locator("div.fixed button", has_text="取消").first.click()

    # ── 7. Skill-group break-catalog override from the modal ──
    await page.goto(f"{pbx.http_url}/console/cc#skill-groups")
    await page.wait_for_selector("button[title='Edit skill group']", timeout=20000)
    await page.click("button[title='Edit skill group']")
    await page.wait_for_selector("#skillgroup-modal")
    await page.click('button[data-sg-tab="extras"]')
    await page.wait_for_selector("#sg-breaks-mount table", timeout=10000)
    await page.locator("#sg-breaks-mount button", has_text="新增类型").click()
    await page.locator('#sg-breaks-mount input[data-bk="id"]').first.fill("smoke")
    await page.locator('#sg-breaks-mount input[data-bk="label_zh"]').first.fill("测试小休")
    await page.locator('#sg-breaks-mount input[data-bk="max_secs"]').first.fill("600")
    await _shot(page, "acd_7_sg_breaks_override.png")
    async with page.expect_response("**/api/cc/skill-groups/support") as resp_info:
        await page.locator("#skillgroup-modal button[type=submit]").click()
    assert (await resp_info.value).status == 200

    group = await cc_api.get(f"{API_BASE}/skill-groups/support")
    data = group.get("data") or group
    extra = data.get("extra") or {}
    phone = extra.get("phone")
    phone_val = phone.get("value") if isinstance(phone, dict) else phone
    breaks = json.loads(phone_val)["breaks"]
    assert breaks[0]["id"] == "smoke" and breaks[0]["max_secs"] == 600, breaks

    # ── 8. Agents list: 今日小休 column ──
    await page.goto(f"{pbx.http_url}/console/cc#agents")
    await page.wait_for_selector("text=今日小休", timeout=20000)
    await page.wait_for_timeout(800)
    await _shot(page, "acd_8_agents_break_today.png")

    # ── 9. Monitor badge now reflects the engine (acd_enabled rides rows) ──
    await page.goto(f"{pbx.http_url}/console/cc#monitor")
    await page.wait_for_timeout(2500)
    await _shot(page, "acd_9_monitor_badge.png")
