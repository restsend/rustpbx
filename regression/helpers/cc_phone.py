"""Playwright page helpers for the cc-phone widget.

Provides high-level methods for agent login, dial, answer, hold, hangup,
presence changes, and DTMF — using the cc-phone widget's CSS selectors.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Optional

logger = logging.getLogger(__name__)


class CcPhonePage:
    """Wraps a Playwright Page that has the cc-phone widget mounted.

    The page is a standalone test page with a pre-configured widget
    (``open_widget``).
    """

    def __init__(self, page):
        self.page = page

    async def open_widget(
        self,
        pbx,
        *,
        agent: str = "1001",
        password: str = "123456",
        locale: str = "en-US",
    ) -> None:
        """Open the static cc-phone E2E host page served by the PBX and wait
        for the widget to register.

        The old dev-console login form no longer exists. This purpose-built
        page mounts the widget VISIBLY via ``CCPhone.create`` so Playwright
        can interact with its DOM.
        """
        url = (
            f"{pbx.http_url}/static/cc/cc-phone-e2e-host.html"
            f"?server=ws://{pbx.host}:{pbx.http_port}/ws"
            f"&agent={agent}&password={password}&locale={locale}"
        )
        await self.page.goto(url, wait_until="networkidle")
        await self.page.wait_for_selector(".cc-phone-root", timeout=30000, state="attached")
        await self._wait_registered()

    async def login(
        self,
        *,
        agent: str = "1001",
        password: str = "123456",
        ws_url: str = "",
        auto_answer: bool = False,
        auto_answer_delay: str = "2",
        relay_only: bool = False,
    ) -> None:
        """Fill login form and connect the cc-phone widget."""
        await self.page.locator("[data-testid='inp-agent']").fill(agent)
        await self.page.locator("[data-testid='inp-pass']").fill(password)
        if ws_url:
            await self.page.locator("[data-testid='inp-server']").fill(ws_url)

        if auto_answer:
            await self.page.locator("[data-testid='opt-auto-answer']").check()
            await self.page.locator("[data-testid='opt-auto-answer-delay']").fill(
                auto_answer_delay
            )
        if relay_only:
            await self.page.locator("[data-testid='opt-relay-only']").check()

        await self.page.locator("[data-testid='btn-connect']").click()
        await self.page.wait_for_selector(".cc-phone-root", timeout=30000, state="attached")
        await self._wait_registered()

    async def _wait_registered(self, timeout: float = 30) -> None:
        try:
            await self.page.wait_for_function(
                "() => { const s = document.getElementById('agent-status'); "
                "return s && s.textContent === 'registered'; }",
                timeout=int(timeout * 1000),
            )
        except Exception:
            logger.warning("agent-status did not reach 'registered', continuing")

    async def hide_widget(self) -> None:
        await self.page.evaluate(
            "() => { const el = document.getElementById('cc-phone-root'); "
            "if (el) el.style.display = 'none'; }"
        )

    async def show_widget(self) -> None:
        await self.page.evaluate(
            "() => { const el = document.getElementById('cc-phone-root'); "
            "if (el) el.style.display = ''; }"
        )

    # ---- call operations ----

    async def dial(self, destination: str) -> None:
        await self.page.locator(".cc-input").fill(destination)
        await self.page.locator(".cc-dial-btn-primary").click()

    async def wait_for_active_call(self, timeout: float = 15) -> None:
        await self.page.wait_for_selector(".cc-active-bar", timeout=int(timeout * 1000))

    async def wait_for_incoming(self, timeout: float = 15) -> None:
        await self.page.wait_for_selector(".cc-incoming-panel", timeout=int(timeout * 1000))

    async def answer_incoming(self) -> None:
        await self.page.locator(".cc-incoming-panel .cc-btn.cc-btn-primary").click()

    async def hold(self) -> None:
        await self.page.locator(".cc-action-btn").first.click()

    async def unhold(self) -> None:
        await self.page.locator(".cc-action-btn").first.click()

    async def wait_for_hold_state(self, timeout: float = 10) -> None:
        await self.page.wait_for_function(
            "() => { const s = document.querySelector('.cc-active-state'); "
            "return s && /Hold/i.test(s.textContent); }",
            timeout=int(timeout * 1000),
        )

    async def wait_for_active_state(self, timeout: float = 10) -> None:
        await self.page.wait_for_function(
            "() => { const s = document.querySelector('.cc-active-state'); "
            "return s && /Active/i.test(s.textContent); }",
            timeout=int(timeout * 1000),
        )

    async def send_dtmf(self, digits: str) -> None:
        await self.page.locator(".cc-action-btn").nth(1).click()
        await self.page.wait_for_selector(".cc-dtmf-keypad", timeout=5000)
        for d in digits:
            key_map = {"*": "star", "#": "pound"}
            attr = key_map.get(d, d)
            sel = f".cc-dtmf-key[data-key='{d}'], .cc-dtmf-key:has-text('{d}')"
            try:
                await self.page.locator(sel).first.click(timeout=2000)
            except Exception:
                await self.page.locator(".cc-dtmf-key").nth(int(d) if d.isdigit() else 10).click()

    async def hangup(self) -> None:
        await self.page.locator(".cc-action-danger").click()

    async def wait_for_idle(self, timeout: float = 15) -> None:
        await self.page.wait_for_selector(".cc-dialpad", timeout=int(timeout * 1000))

    async def get_active_caller(self) -> Optional[str]:
        try:
            el = self.page.locator(".cc-active-caller")
            return await el.text_content(timeout=3000)
        except Exception:
            return None

    async def get_active_timer(self) -> Optional[str]:
        try:
            el = self.page.locator(".cc-active-timer")
            return await el.text_content(timeout=3000)
        except Exception:
            return None

    # ---- presence ----

    async def set_presence(self, presence: str) -> None:
        """Set presence via SDK (faster than UI interaction)."""
        await self.page.evaluate(
            f"(window.__testPhone || window.phone)?.setPresence('{presence}')"
        )

    async def set_presence_ui(self, label_pattern: str) -> None:
        """Set presence via UI hover + click."""
        await self.page.locator(".cc-presence-trigger").hover()
        await self.page.wait_for_selector(".cc-presence-menu", timeout=5000)
        await self.page.locator(f".cc-presence-item:has-text('{label_pattern}')").click()

    async def get_presence_label(self) -> Optional[str]:
        try:
            return await self.page.locator(".cc-presence-trigger").text_content(timeout=3000)
        except Exception:
            return None

    # ---- consult transfer ----

    async def start_consult(self, target: str) -> None:
        await self.page.locator(".cc-action-btn[title*='onsult'], .cc-action-btn[data-action='consult']").click()
        await self.page.wait_for_selector(".cc-consult-panel", timeout=5000)
        await self.page.locator(".cc-consult-input").fill(target)

    # ---- SDK direct calls ----

    async def sdk_dial(self, destination: str) -> None:
        await self.page.evaluate(f"window.phone.dial('{destination}')")

    async def sdk_answer(self) -> None:
        await self.page.evaluate("window.phone.answer()")

    async def sdk_hold(self) -> None:
        await self.page.evaluate("window.phone.hold()")

    async def sdk_retrieve(self) -> None:
        await self.page.evaluate("window.phone.retrieve()")

    async def sdk_hangup(self) -> None:
        await self.page.evaluate("window.phone.hangup()")

    async def sdk_is_registered(self) -> bool:
        return await self.page.evaluate("window.phone?.isRegistered() ?? false")

    # ---- event log ----

    async def get_event_log(self) -> list[str]:
        return await self.page.evaluate(
            """() => {
                const log = document.getElementById('event-log');
                return log ? Array.from(log.children).map(l => l.textContent || '') : [];
            }"""
        )

    async def wait_for_event_log(self, needle: str, timeout: float = 15) -> bool:
        try:
            await self.page.wait_for_function(
                f"(n) => ((document.getElementById('event-log') || {{}}).textContent || '').includes(n)",
                needle,
                timeout=int(timeout * 1000),
            )
            return True
        except Exception:
            return False

    # ---- screenshot ----

    async def screenshot(self, path: str) -> None:
        await self.page.screenshot(path=path, full_page=True)

    async def disconnect(self) -> None:
        try:
            btn = self.page.locator("[data-testid='btn-disconnect']")
            if await btn.is_enabled(timeout=2000):
                await btn.click()
        except Exception:
            pass
