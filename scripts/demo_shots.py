#!/usr/bin/env python3
"""Screenshot the releases page (manage + client-telemetry tabs)."""
import os
from playwright.sync_api import sync_playwright

BASE = os.environ.get("TELEMETRY_BASE", "http://127.0.0.1:8099")
OUT = os.environ.get("SHOT_OUT", "/private/var/folders/yr/3_mgdw995s7bb4grvcmsk1d00000gn/T/opencode/shots")
os.makedirs(OUT, exist_ok=True)

def go(page, url, wait=1500):
    page.goto(url, wait_until="domcontentloaded")
    page.wait_for_timeout(wait)

with sync_playwright() as p:
    browser = p.chromium.launch()
    page = browser.new_page(viewport={"width": 1600, "height": 1000}, locale="zh-CN")

    # login (bootstrap first admin if needed)
    go(page, f"{BASE}/console/login", 800)
    page.fill('input[name="identifier"]', "admin")
    page.fill('input[name="password"]', "Verif123456")
    page.click('button[type="submit"]')
    page.wait_for_timeout(1500)

    # 1. releases page — management tab
    go(page, f"{BASE}/console/releases", 2000)
    page.screenshot(path=f"{OUT}/1-releases-manage.png")

    # 2. client telemetry tab
    tab = page.locator("button:has-text('客户端上报')").first
    tab.click(timeout=5000)
    page.wait_for_timeout(2500)
    page.screenshot(path=f"{OUT}/2-telemetry-overview.png")

    # 3. scroll to distributions + recent list
    page.mouse.wheel(0, 700)
    page.wait_for_timeout(800)
    page.screenshot(path=f"{OUT}/3-telemetry-distributions.png")

    # 4. open one report body modal
    view = page.get_by_role("button", name="查看").first
    if view.count() > 0:
        view.click(timeout=2000)
        page.wait_for_timeout(1200)
        page.screenshot(path=f"{OUT}/4-telemetry-body.png")
        page.keyboard.press("Escape")

    browser.close()
print("saved:", OUT)
