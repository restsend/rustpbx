"""Scenario DSL — declarative CC workflow scenarios loaded from TOML.

A scenario is a named, ordered list of steps that the :class:`Orchestrator`
executes against the session-scoped pbx. Keeping scenarios declarative means
the exact JSON event-hook flow of every test is reproducible and inspectable
(the orchestrator dumps the full webhook + RWI WS payloads to the report).

Schema::

    name        = "ivr_queue"
    description = "..."
    module      = "scenario"

    [setup]
    agents       = [ { agent_id="1002", skills=["support"], ... } ]
    skill_groups = [ { skill_group_id="support", overflow_groups=["sales"], ... } ]

    [[steps]]
    op    = "wait_webhook"
    event = "call_ringing"
    store = "ringing"
    ...

Supported ``op`` verbs and their required keys are listed in ``KNOWN_OPS``.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

KNOWN_OPS: dict[str, set[str]] = {
    # setup / config
    "seed_agents": {"agents"},
    "update_skill_group": {"skill_group_id", "body"},
    "create_skill_group": {"skill_group_id", "body"},
    # agent lifecycle (VQ)
    "agent_online": {"username"},
    "agent_offline": {"username"},
    "agent_status": {"username", "status"},
    # call control
    "originate": {"call_id", "destination"},
    "dtmf": {"call_id", "digits"},
    "hangup": {"call_id"},
    "end_call": {"call_id"},
    "blind_transfer": {"call_id", "target"},
    "consult": {"call_id", "target"},
    "consult_connected": {"call_id", "tid", "session_b"},
    "consult_merge": {"call_id", "tid"},
    "consult_complete": {"call_id", "tid"},
    # conference (MCU)
    "conference_create": {"conf_id"},
    "conference_add": {"conf_id", "call_id"},
    "conference_mute": {"conf_id", "call_id"},
    "conference_unmute": {"conf_id", "call_id"},
    "conference_remove": {"conf_id", "call_id"},
    "conference_destroy": {"conf_id"},
    # event assertions
    "wait_webhook": {"event"},
    "wait_rwi": {"event"},
    "wait_sequence": {"events"},
    "assert_field": {"var", "path"},
    "assert_no_webhook": {"event"},
    "check_cdr": {"call_id", "path"},
    # misc
    "sleep": {"secs"},
    "log": {"msg"},
}

STEP_KEYS = {
    "op", "label", "store", "call_id", "destination", "caller_id", "timeout_secs",
    "digits", "event", "events", "timeout", "wait", "var", "path", "equals",
    "one_of", "not_equal", "secs", "msg", "username", "status", "target", "tid",
    "session_b", "conf_id", "agents", "skill_group_id", "body", "answer_mode",
    "hangup_after", "dtmf_flows", "ring_secs", "set_idle", "port", "caller",
    "reason", "via", "occurrence", "match",
}


@dataclass
class Scenario:
    """A parsed scenario definition."""

    name: str
    description: str
    module: str
    setup: dict = field(default_factory=dict)
    steps: list[dict] = field(default_factory=list)
    path: Optional[Path] = None

    @classmethod
    def load(cls, path: Path) -> "Scenario":
        data = tomllib.loads(Path(path).read_text(encoding="utf-8"))
        scenario = cls(
            name=str(data.get("name", Path(path).stem)),
            description=str(data.get("description", "")),
            module=str(data.get("module", "scenario")),
            setup=dict(data.get("setup") or {}),
            steps=list(data.get("steps") or []),
            path=Path(path),
        )
        scenario._validate()
        return scenario

    # ---- validation ----

    def _validate(self) -> None:
        if not self.name:
            raise ValueError(f"{self.path}: scenario 'name' is required")
        if not isinstance(self.steps, list) or not self.steps:
            raise ValueError(f"{self.path}: scenario '{self.name}' has no steps")
        for i, step in enumerate(self.steps):
            self._validate_step(step, i)

    def _validate_step(self, step: dict, i: int) -> None:
        where = f"{self.path} step[{i}]"
        op = step.get("op")
        if not isinstance(step, dict) or not op:
            raise ValueError(f"{where}: missing 'op'")
        if op not in KNOWN_OPS:
            raise ValueError(f"{where}: unknown op '{op}'")
        for key in step:
            if key not in STEP_KEYS:
                raise ValueError(f"{where}: unknown key '{key}' (op={op})")
        missing = KNOWN_OPS[op] - set(step)
        if missing:
            raise ValueError(f"{where} op={op}: missing required keys {sorted(missing)}")
