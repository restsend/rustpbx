"""Self-contained final HTML report generator.

Consumes per-lane results (junit.xml and/or evidence.json manifests) under
    <artifacts>/<run_id>/
and renders <artifacts>/<run_id>/report/index.html — a single portable file:
    * dashboard (totals, per-lane table, duration)
    * acceptance matrix (optional map: section -> test ids)
    * per-case cards: status/duration/error, inline screenshots (base64),
      inline audio player (<audio>), CDR field diff, event timeline,
      assertion-depth badges
    * known-issues page (strict-xfail product gaps)

Small artifacts (<1MB) are embedded as data URIs; larger ones are copied to
report/evidence/ and linked relatively.
"""

from __future__ import annotations

import base64
import html
import json
import shutil
import time
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

EMBED_LIMIT = 1_000_000  # 1MB per artifact
LEVEL_BADGES = ("FLOW", "FORMAT", "FIELD", "AUDIO")


@dataclass
class CaseView:
    lane: str
    name: str
    classname: str
    status: str  # passed/failed/error/skipped
    duration: float = 0.0
    error: str = ""
    evidence: dict = field(default_factory=dict)  # parsed evidence.json


def parse_junit(junit_path: Path, lane: str) -> list[CaseView]:
    cases: list[CaseView] = []
    try:
        root = ET.parse(junit_path).getroot()
    except ET.ParseError as exc:
        return [CaseView(lane=lane, name=f"<unparseable junit: {exc}>", classname=lane, status="error")]
    for tc in root.iter("testcase"):
        name = tc.get("name", "?")
        classname = tc.get("classname", "")
        dur = float(tc.get("time", "0") or 0)
        status, err = "passed", ""
        for tag, label in (("failure", "failed"), ("error", "error"), ("skipped", "skipped")):
            node = tc.find(tag)
            if node is not None:
                status = label
                err = (node.get("message") or "") + "\n" + (node.text or "")
                break
        cases.append(CaseView(lane=lane, name=name, classname=classname, status=status, duration=dur, error=err))
    return cases


def _find_evidence_dir(run_root: Path, lane: str, classname: str, name: str) -> Optional[Path]:
    base = run_root / f"lane-{lane}" / "tests"
    if not base.exists():
        return None
    stem = name.split("[")[0]
    for sep in ("::", "."):
        if sep in stem:
            stem = stem.rsplit(sep, 1)[-1]
    for d in sorted(base.iterdir(), reverse=True):
        if not d.is_dir():
            continue
        tail = d.name.split("-", 1)[-1]
        if stem in tail:
            return d
    return None


class ReportBuilder:
    def __init__(self, run_root: Path, *, title: str = "RustPBX Unified Regression Report"):
        self.run_root = Path(run_root)
        self.title = title
        self.cases: list[CaseView] = []
        self.known_issues: list[str] = []
        self.acceptance_map: dict = {}  # section -> list[(testname, note)]
        self.lane_meta: dict = {}  # lane -> {duration, started}

    # -- inputs -------------------------------------------------------------

    def add_junit(self, junit_path, lane: str) -> int:
        p = Path(junit_path)
        if not p.exists():
            self.lane_meta[lane] = {"status": "absent", "junit": str(p)}
            return 0
        cases = parse_junit(p, lane)
        self.cases.extend(cases)
        self.lane_meta[lane] = {
            "status": "ok",
            "junit": str(p),
            "total": len(cases),
            "failed": sum(1 for c in cases if c.status in ("failed", "error")),
            "duration": round(sum(c.duration for c in cases), 1),
        }
        return len(cases)

    def load_evidence(self) -> int:
        """Attach evidence.json manifests to cases by directory match."""
        n = 0
        for case in self.cases:
            d = _find_evidence_dir(self.run_root, case.lane, case.classname, case.name)
            if not d:
                continue
            manifest = d / "evidence.json"
            if manifest.exists():
                try:
                    case.evidence = json.loads(manifest.read_text(encoding="utf-8"))
                    n += 1
                except json.JSONDecodeError:
                    pass
        return n

    def set_known_issues(self, issues: list[str]) -> None:
        self.known_issues = list(issues)

    def set_acceptance_map(self, mapping: dict) -> None:
        self.acceptance_map = mapping

    # -- rendering ----------------------------------------------------------

    @staticmethod
    def _data_uri(path: Path, mime: str) -> Optional[str]:
        try:
            data = path.read_bytes()
        except OSError:
            return None
        if len(data) > EMBED_LIMIT:
            return None
        return f"data:{mime};base64," + base64.b64encode(data).decode()

    def _render_case(self, c: CaseView) -> str:
        color = {"passed": "#22c55e", "failed": "#ef4444", "error": "#f97316", "skipped": "#94a3b8"}.get(c.status, "#94a3b8")
        ev = c.evidence or {}
        levels = ev.get("assertion_levels") or []
        badges = " ".join(
            f'<span class="badge">{html.escape(l)}</span>' for l in LEVEL_BADGES if l in levels
        ) or '<span class="badge dim">no depth markers</span>'

        parts = [
            f'<div class="case" id="case-{len(self.cases) and c.name and html.escape(c.name)[:60]}">',
            f'<div class="case-head" style="border-left-color:{color}">',
            f'<span class="st" style="color:{color}">{c.status.upper()}</span> ',
            f'<code>{html.escape(c.classname)}::{html.escape(c.name)}</code> '
            f'<span class="dur">{c.duration:.1f}s</span> <span class="lane">{html.escape(c.lane)}</span>',
            f'<div>{badges}</div></div>',
        ]
        if c.error:
            parts.append(f"<pre class='err'>{html.escape(c.error[:3000])}</pre>")

        edir = self.run_root / f"lane-{c.lane}" / "tests"
        # find actual evidence dir for relative links
        d = _find_evidence_dir(self.run_root, c.lane, c.classname, c.name)
        for entry in ev.get("entries", []):
            kind, fname = entry.get("kind"), entry.get("file", "")
            fpath = (d / fname) if d else None
            note = html.escape(str(entry.get("note") or ""))
            if kind == "screenshot" and fpath and fpath.exists():
                uri = self._data_uri(fpath, "image/png")
                if uri:
                    parts.append(f"<figure><img src='{uri}' style='max-width:640px'><figcaption>{note}</figcaption></figure>")
                else:
                    parts.append(f"<p><a href='evidence/{html.escape(fname)}'>{html.escape(fname)}</a> ({note})</p>")
                    self._copy_out(fpath)
            elif kind == "recording" and fpath and fpath.exists():
                uri = self._data_uri(fpath, "audio/wav")
                if uri:
                    parts.append(
                        f"<div class='audio'><audio controls preload='none' src='{uri}'></audio>"
                        f"<span> {html.escape(fname)} {note}</span></div>"
                    )
                else:
                    parts.append(f"<p><a href='evidence/{html.escape(fname)}'>audio: {html.escape(fname)}</a> ({note})</p>")
                    self._copy_out(fpath)
            elif kind == "cdr":
                diff = ""
                if entry.get("diff") and d and (d / (fname + ".diff.txt")).exists():
                    diff = f"<pre class='err'>{html.escape((d / (fname + '.diff.txt')).read_text()[:2000])}</pre>"
                parts.append(f"<p class='okline'>CDR snapshot: {html.escape(fname)} {note}</p>{diff}")
            elif kind == "events":
                parts.append(f"<p class='okline'>events({entry.get('channel')}): {entry.get('count')} captured → {html.escape(fname)}</p>")
            elif kind == "result":
                data = entry.get("data") or {}
                summ = data.get("summary") or json.dumps(data, ensure_ascii=False)[:200]
                parts.append(f"<p class='okline'>assert {html.escape(str(entry.get('name')))}: {html.escape(str(summ))}</p>")

        parts.append("</div>")
        return "\n".join(parts)

    def _copy_out(self, fpath: Path) -> None:
        out = self.run_root / "report" / "evidence"
        out.mkdir(parents=True, exist_ok=True)
        try:
            shutil.copy2(fpath, out / fpath.name)
        except OSError:
            pass

    def render(self) -> Path:
        t0 = time.time()
        total = len(self.cases)
        by = {"passed": 0, "failed": 0, "error": 0, "skipped": 0}
        for c in self.cases:
            by[c.status] = by.get(c.status, 0) + 1
        depth = sum(1 for c in self.cases if (c.evidence or {}).get("assertion_levels"))

        css = """
        body{font-family:-apple-system,Segoe UI,Roboto,sans-serif;background:#0b1020;color:#e2e8f0;margin:0;padding:24px}
        h1{font-size:22px} h2{font-size:17px;margin-top:28px} code{color:#93c5fd}
        .cards{display:flex;gap:14px;flex-wrap:wrap;margin:14px 0}
        .card{background:#131a33;border-radius:10px;padding:14px 20px;min-width:130px}
        .card .n{font-size:26px;font-weight:700}
        table{border-collapse:collapse;width:100%;margin:8px 0}
        td,th{border:1px solid #233054;padding:6px 10px;font-size:13px;text-align:left}
        .case{background:#101731;border-radius:10px;margin:10px 0;padding:10px 14px}
        .case-head{border-left:4px solid #334;padding-left:10px}
        .st{font-weight:700;margin-right:8px} .dur{color:#7dd3fc;margin-left:8px} .lane{color:#64748b;margin-left:8px}
        .badge{background:#1e293b;border-radius:6px;padding:1px 8px;margin-right:4px;font-size:11px}
        .badge.dim{color:#64748b}
        pre.err{background:#1a0f14;border:1px solid #7f1d1d;border-radius:8px;padding:10px;overflow-x:auto;font-size:12px;max-height:320px}
        pre{overflow-x:auto}
        .err{color:#fca5a5} .okline{color:#86efac;font-size:13px}
        figure{margin:8px 0} figcaption{color:#94a3b8;font-size:12px}
        .audio{margin:6px 0} audio{height:32px}
        details{margin:6px 0} summary{cursor:pointer;color:#93c5fd}
        """
        cards = f"""
        <div class="cards">
          <div class="card"><div class="n">{total}</div>total</div>
          <div class="card"><div class="n" style="color:#22c55e">{by['passed']}</div>passed</div>
          <div class="card"><div class="n" style="color:#ef4444">{by['failed'] + by['error']}</div>failed</div>
          <div class="card"><div class="n" style="color:#94a3b8">{by['skipped']}</div>skipped</div>
          <div class="card"><div class="n" style="color:#7dd3fc">{depth}</div>with evidence+depth</div>
        </div>"""

        lane_rows = "".join(
            f"<tr><td>{html.escape(lane)}</td><td>{meta.get('total','—')}</td>"
            f"<td>{meta.get('failed','—')}</td><td>{meta.get('duration','—')}s</td><td>{meta.get('status','')}</td></tr>"
            for lane, meta in self.lane_meta.items()
        )

        acc_rows = ""
        for section, items in sorted(self.acceptance_map.items()):
            got = [(n2, self._status_of(n2)) for n2 in items]
            mark = "".join(
                f"<span style='color:{ {'passed':'#22c55e','failed':'#ef4444','error':'#f97316','skipped':'#94a3b8'}.get(s2,'#64748b') }'>"
                f"{'✓' if s2=='passed' else '✗' if s2 in ('failed','error') else '○'}</span> {html.escape(n2)} "
                for n2, s2 in got
            )
            acc_rows += f"<tr><td>{html.escape(section)}</td><td>{mark or '<i>unmapped</i>'}</td></tr>"

        known = "".join(f"<li>{html.escape(i)}</li>" for i in self.known_issues) or "<li>none</li>"

        failed_first = sorted(self.cases, key=lambda c: {"error": 0, "failed": 1, "skipped": 2, "passed": 3}[c.status])
        case_html = "".join(self._render_case(c) for c in failed_first)

        doc = f"""<!doctype html><html><head><meta charset="utf-8">
<title>{html.escape(self.title)}</title><style>{css}</style></head><body>
<h1>{html.escape(self.title)}</h1>
<p>run: <code>{html.escape(self.run_root.name)}</code> · generated {time.strftime('%Y-%m-%d %H:%M:%S')} · render {time.time()-t0:.1f}s</p>
{cards}
<h2>Lanes</h2><table><tr><th>lane</th><th>total</th><th>failed</th><th>duration</th><th>status</th></tr>{lane_rows}</table>
<h2>Acceptance matrix</h2><table><tr><th>Acceptance section</th><th>Case status</th></tr>{acc_rows}</table>
<h2>Known issues (strict-xfail product gaps)</h2><ul>{known}</ul>
<h2>Cases (failures first)</h2>
{case_html}
</body></html>"""
        out = self.run_root / "report" / "index.html"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(doc, encoding="utf-8")
        return out

    def _status_of(self, testname: str) -> Optional[str]:
        stem = testname.split("[")[0]
        for c in self.cases:
            if c.name == testname or c.name.startswith(stem):
                return c.status
        return None
