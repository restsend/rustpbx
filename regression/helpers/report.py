"""HTML report generator for regression acceptance results.

Generates a standalone HTML report with:
  - Summary table (pass/fail per tier, per module)
  - Per-test details with event timelines
  - RTP stats tables
  - Embedded screenshots
  - SIP signaling traces (from sipbot output)
"""

from __future__ import annotations

import base64
import html
import json
import logging
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

logger = logging.getLogger(__name__)


@dataclass
class TestRecord:
    name: str
    module: str
    tier: str
    status: str = "pending"  # pending / passed / failed / error
    duration: float = 0.0
    error: Optional[str] = None
    webhook_events: list[dict] = field(default_factory=list)
    rtp_stats: dict[str, dict] = field(default_factory=dict)
    sip_output: dict[str, str] = field(default_factory=dict)
    screenshot_path: Optional[str] = None
    timestamp: float = field(default_factory=time.time)
    # Full RWI event-hook JSON flow (webhook raw bodies + RWI WS payloads).
    event_flow: Optional[dict] = None
    event_flow_path: Optional[str] = None

    def to_dict(self) -> dict:
        return {
            "name": self.name,
            "module": self.module,
            "tier": self.tier,
            "status": self.status,
            "duration": round(self.duration, 3),
            "error": self.error,
            "webhook_events": self.webhook_events,
            "rtp_stats": self.rtp_stats,
            "sip_output": self.sip_output,
            "screenshot_path": self.screenshot_path,
            "timestamp": self.timestamp,
            "event_flow_path": self.event_flow_path,
            "event_flow": self.event_flow,
        }


class ReportGenerator:
    """Collects test records and renders a standalone HTML report."""

    def __init__(self, report_dir: Path):
        self.report_dir = report_dir
        self.records: list[TestRecord] = []
        self.start_time = time.time()

    def add_record(self, record: TestRecord) -> None:
        self.records.append(record)

    def _encode_screenshot(self, path: str) -> Optional[str]:
        p = Path(path)
        if not p.exists():
            return None
        data = p.read_bytes()
        b64 = base64.b64encode(data).decode()
        return f"data:image/png;base64,{b64}"

    def _render_summary_table(self) -> str:
        tiers: dict[str, dict[str, int]] = {}
        modules: dict[str, dict[str, int]] = {}
        for r in self.records:
            t = tiers.setdefault(r.tier, {"passed": 0, "failed": 0, "total": 0})
            m = modules.setdefault(r.module, {"passed": 0, "failed": 0, "total": 0})
            t["total"] += 1
            m["total"] += 1
            if r.status == "passed":
                t["passed"] += 1
                m["passed"] += 1
            else:
                t["failed"] += 1
                m["failed"] += 1

        rows = []
        # tier summary
        for tier in sorted(tiers):
            d = tiers[tier]
            rate = (d["passed"] / d["total"] * 100) if d["total"] else 0
            cls = "pass" if d["failed"] == 0 else "fail"
            rows.append(
                f"<tr class='{cls}'><td>{tier}</td><td>{d['passed']}</td>"
                f"<td>{d['failed']}</td><td>{d['total']}</td>"
                f"<td>{rate:.1f}%</td></tr>"
            )

        # module breakdown
        module_rows = []
        for mod in sorted(modules):
            d = modules[mod]
            rate = (d["passed"] / d["total"] * 100) if d["total"] else 0
            cls = "pass" if d["failed"] == 0 else "fail"
            module_rows.append(
                f"<tr class='{cls}'><td>{mod}</td><td>{d['passed']}</td>"
                f"<td>{d['failed']}</td><td>{d['total']}</td>"
                f"<td>{rate:.1f}%</td></tr>"
            )

        return f"""
        <h2>Tier Summary</h2>
        <table class="summary"><thead><tr><th>Tier</th><th>Passed</th><th>Failed</th><th>Total</th><th>Rate</th></tr></thead>
        <tbody>{''.join(rows)}</tbody></table>
        <h2>Module Summary</h2>
        <table class="summary"><thead><tr><th>Module</th><th>Passed</th><th>Failed</th><th>Total</th><th>Rate</th></tr></thead>
        <tbody>{''.join(module_rows)}</tbody></table>
        """

    def _render_event_timeline(self, events: list[dict]) -> str:
        if not events:
            return ""
        rows = []
        for ev in events:
            ts = ev.get("timestamp", 0)
            etype = ev.get("event_type", "")
            cid = ev.get("call_id", "")
            rows.append(
                f"<tr><td>{ts:.3f}</td>"
                f"<td class='event-{etype}'><strong>{etype}</strong></td>"
                f"<td>{html.escape(str(cid))}</td></tr>"
            )
        tbl = (
            "<table class='events'><thead><tr><th>Time</th>"
            "<th>Event Type</th><th>Call ID</th></tr></thead>"
            f"<tbody>{''.join(rows)}</tbody></table>"
        )
        return f"<details class='collapsible-section'><summary>Webhook Events ({len(events)})</summary>{tbl}</details>"

    def _render_event_flow(self, record: TestRecord) -> str:
        """Collapsible viewer + file link for the test's full JSON event flow."""
        flow = record.event_flow
        if not flow:
            return ""
        wh_count = flow.get("webhook_count", 0)
        rwi_count = flow.get("rwi_count", 0)
        link = ""
        if record.event_flow_path:
            link = (
                f" &mdash; <a class='flow-link' href='{html.escape(record.event_flow_path)}' "
                f"target='_blank' rel='noopener'>open full JSON flow</a>"
            )
        pretty = json.dumps(flow, indent=2, ensure_ascii=False, default=str)
        return (
            f"<details class='collapsible-section flow-detail'>"
            f"<summary>Full RWI Event-Hook JSON Flow "
            f"({wh_count} webhook &middot; {rwi_count} RWI-WS events){link}</summary>"
            f"<pre class='flow'>{html.escape(pretty)}</pre></details>"
        )

    def _render_rtp_table(self, stats: dict[str, dict]) -> str:
        if not stats:
            return ""
        rows = []
        for name, s in stats.items():
            rows.append(
                f"<tr><td>{name}</td><td>{s.get('rx_packets', 0)}</td>"
                f"<td>{s.get('rx_bytes', 0)}</td><td>{s.get('tx_packets', 0)}</td>"
                f"<td>{s.get('tx_bytes', 0)}</td></tr>"
            )
        tbl = (
            "<table class='rtp'><thead><tr><th>UA</th><th>RX Pkts</th>"
            "<th>RX Bytes</th><th>TX Pkts</th><th>TX Bytes</th></tr></thead>"
            f"<tbody>{''.join(rows)}</tbody></table>"
        )
        return f"<details class='collapsible-section'><summary>RTP Stats</summary>{tbl}</details>"

    def _render_sip_trace(self, sip_output: dict[str, str]) -> str:
        if not sip_output:
            return ""
        sections = []
        for name, output in sip_output.items():
            escaped = html.escape(output[-2000:])
            sections.append(
                f"<details><summary>SIP: {name}</summary><pre class='sip'>{escaped}</pre></details>"
            )
        return f"<div class='sip-traces'>{''.join(sections)}</div>"

    def _render_test_details(self) -> str:
        cards = []
        for r in self.records:
            status_class = r.status
            error_html = ""
            if r.error:
                error_html = (
                    f"<details class='error-detail'>"
                    f"<summary>Error Details</summary>"
                    f"<div class='error'><pre>{html.escape(r.error)}</pre></div>"
                    f"</details>"
                )

            screenshot_html = ""
            if r.screenshot_path:
                b64 = self._encode_screenshot(r.screenshot_path)
                if b64:
                    screenshot_html = (
                        f"<details><summary>Screenshot</summary>"
                        f"<img src='{b64}' style='max-width:100%;border:1px solid #ccc;' /></details>"
                    )

            timeline = self._render_event_timeline(r.webhook_events)
            flow = self._render_event_flow(r)
            rtp = self._render_rtp_table(r.rtp_stats)
            sip = self._render_sip_trace(r.sip_output)

            cards.append(
                f"""
                <div class='test-card {status_class}'>
                    <div class='test-header'>
                        <span class='test-status status-{status_class}'>{r.status.upper()}</span>
                        <span class='test-name'>{html.escape(r.name)}</span>
                        <span class='test-meta'>{r.module} / {r.tier} / {r.duration:.2f}s</span>
                    </div>
                    {error_html}
                    {timeline}
                    {flow}
                    {rtp}
                    {sip}
                    {screenshot_html}
                </div>
                """
            )
        return "\n".join(cards)

    def render_html(self) -> str:
        total = len(self.records)
        passed = sum(1 for r in self.records if r.status == "passed")
        failed = total - passed
        duration = time.time() - self.start_time
        overall_rate = (passed / total * 100) if total else 0
        overall_class = "pass" if failed == 0 else "fail"

        summary = self._render_summary_table()
        details = self._render_test_details()

        return f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<title>RustPBX CC E2E Regression Report</title>
<style>
* {{ box-sizing: border-box; margin: 0; padding: 0; }}
body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
       background: #0f172a; color: #e2e8f0; padding: 20px; }}
h1 {{ color: #38bdf8; margin-bottom: 10px; }}
h2 {{ color: #7dd3fc; margin: 20px 0 10px; }}
.header {{ background: #1e293b; padding: 20px; border-radius: 8px; margin-bottom: 20px;
           display: flex; justify-content: space-between; align-items: center; }}
.overall {{ font-size: 2em; font-weight: bold; }}
.overall.pass {{ color: #4ade80; }}
.overall.fail {{ color: #f87171; }}
.summary, .events, .rtp {{ border-collapse: collapse; width: 100%; margin-bottom: 15px;
                           background: #1e293b; border-radius: 8px; overflow: hidden; }}
.summary th, .events th, .rtp th {{ background: #334155; padding: 8px 12px; text-align: left;
                                    color: #cbd5e1; font-size: 0.85em; text-transform: uppercase; }}
.summary td, .events td, .rtp td {{ padding: 8px 12px; border-top: 1px solid #334155; font-size: 0.9em; }}
tr.pass {{ background: rgba(74, 222, 128, 0.05); }}
tr.fail {{ background: rgba(248, 113, 113, 0.05); }}
.test-card {{ background: #1e293b; border-radius: 8px; margin-bottom: 15px; overflow: hidden;
              border-left: 4px solid #64748b; }}
.test-card.passed {{ border-left-color: #4ade80; }}
.test-card.failed {{ border-left-color: #f87171; }}
.test-header {{ padding: 12px 16px; display: flex; align-items: center; gap: 12px;
                background: #334155; }}
.test-status {{ padding: 2px 8px; border-radius: 4px; font-size: 0.8em; font-weight: bold; }}
.status-passed {{ background: #4ade80; color: #052e16; }}
.status-failed {{ background: #f87171; color: #450a0a; }}
.status-error {{ background: #fbbf24; color: #451a03; }}
.test-name {{ font-weight: 600; flex: 1; }}
.test-meta {{ color: #94a3b8; font-size: 0.85em; }}
.error {{ padding: 12px 16px; background: rgba(248, 113, 113, 0.1); }}
.error pre {{ white-space: pre-wrap; margin-top: 6px; font-size: 0.85em; color: #fca5a5; }}
details.error-detail {{ margin: 4px 16px; padding: 0; border: 1px solid rgba(248,113,113,0.2); border-radius: 6px; }}
details.error-detail > summary {{ background: rgba(248,113,113,0.15); padding: 6px 12px; color: #fca5a5;
    font-weight: 600; font-size: 0.85em; border-radius: 5px; cursor: pointer; }}
details.error-detail > .error {{ background: transparent; padding: 4px 12px 8px; }}
.test-card > table, .test-card > details, .test-card > div {{ margin: 12px 16px; }}
.sip {{ background: #0f172a; padding: 10px; border-radius: 4px; font-size: 0.8em;
        max-height: 300px; overflow-y: auto; white-space: pre-wrap; color: #94a3b8; }}
details {{ margin: 8px 0; }}
summary {{ cursor: pointer; color: #38bdf8; font-size: 0.9em; }}
details.collapsible-section {{ margin: 6px 16px; border: 1px solid #334155; border-radius: 6px; padding: 0; }}
details.collapsible-section > summary {{ padding: 6px 12px; font-weight: 600; font-size: 0.85em;
    color: #94a3b8; background: rgba(51,65,85,0.4); border-radius: 5px; }}
details.collapsible-section > table {{ margin: 8px 0; }}
.flow-detail > summary {{ color: #eab308; }}
.flow-link {{ color: #38bdf8; text-decoration: none; font-weight: 400; }}
.flow {{ background: #0f172a; padding: 10px; border-radius: 4px; font-size: 0.72em;
        line-height: 1.35; max-height: 420px; overflow-y: auto; white-space: pre; color: #a5f3fc;
        border: 1px solid #334155; margin: 8px 12px; }}
img {{ margin: 8px 0; }}
</style>
</head>
<body>
<div class="header">
    <div>
        <h1>RustPBX CC E2E Regression Report</h1>
        <p>Generated: {time.strftime('%Y-%m-%d %H:%M:%S')} | Duration: {duration:.1f}s</p>
    </div>
    <div class="overall {overall_class}">{passed}/{total} ({overall_rate:.1f}%)</div>
</div>
{summary}
<h2>Test Details</h2>
{details}
</body>
</html>"""

    def write(self) -> Path:
        self.report_dir.mkdir(parents=True, exist_ok=True)
        html_path = self.report_dir / "regression_report.html"
        html_path.write_text(self.render_html(), encoding="utf-8")

        json_path = self.report_dir / "regression_report.json"
        json_path.write_text(
            json.dumps(
                {
                    "total": len(self.records),
                    "passed": sum(1 for r in self.records if r.status == "passed"),
                    "duration": time.time() - self.start_time,
                    "tests": [r.to_dict() for r in self.records],
                },
                indent=2,
                ensure_ascii=False,
            ),
            encoding="utf-8",
        )
        logger.info("Report written to %s", html_path)
        return html_path
