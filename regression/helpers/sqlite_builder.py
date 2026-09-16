"""SQLite database builder for RustPBX E2E tests.

Creates a file-based SQLite database with the RustPBX schema seed data
for scenarios that require persistent state (agents, skill groups, call
records). Most tests use `sqlite::memory:` via the config, but this
helper supports cases where a file-based DB is needed.
"""

from __future__ import annotations

import sqlite3
from pathlib import Path
from typing import Optional


class SqliteBuilder:
    """Create a SQLite database file with CC addon tables."""

    def __init__(self, db_path: Path):
        self.db_path = db_path
        self.conn: Optional[sqlite3.Connection] = None

    def connect(self) -> sqlite3.Connection:
        if self.db_path.exists():
            self.db_path.unlink()
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self.conn = sqlite3.connect(str(self.db_path))
        self.conn.execute("PRAGMA journal_mode=WAL")
        return self.conn

    def create_tables(self) -> None:
        assert self.conn is not None
        c = self.conn.cursor()
        c.executescript(
            """
            CREATE TABLE IF NOT EXISTS cc_agent (
                agent_id TEXT PRIMARY KEY,
                display_name TEXT,
                primary_endpoint TEXT,
                skills TEXT,
                max_concurrency INTEGER DEFAULT 1,
                role TEXT DEFAULT 'agent',
                status TEXT DEFAULT 'offline',
                created_at TEXT DEFAULT (datetime('now')),
                updated_at TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS cc_skill (
                skill_id TEXT PRIMARY KEY,
                name TEXT,
                description TEXT,
                created_at TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS cc_skill_group (
                skill_group_id TEXT PRIMARY KEY,
                name TEXT,
                skills_required TEXT,
                overflow_groups TEXT,
                sla_target_secs INTEGER DEFAULT 30,
                max_wait_secs INTEGER DEFAULT 90,
                created_at TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS cc_acd_queue (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                policy_name TEXT,
                strategy TEXT DEFAULT 'longest_idle',
                skill_group_id TEXT,
                max_queue_size INTEGER DEFAULT 100,
                agent_timeout_secs INTEGER DEFAULT 20,
                wrap_up_time_secs INTEGER DEFAULT 10
            );

            CREATE TABLE IF NOT EXISTS cc_call (
                call_id TEXT PRIMARY KEY,
                agent_id TEXT,
                direction TEXT,
                caller_id TEXT,
                callee_id TEXT,
                status TEXT,
                started_at TEXT,
                ended_at TEXT,
                duration_secs INTEGER
            );

            CREATE TABLE IF NOT EXISTS cc_call_record (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                call_id TEXT,
                agent_id TEXT,
                direction TEXT,
                trunk TEXT,
                caller TEXT,
                callee TEXT,
                started_at TEXT,
                ended_at TEXT,
                duration_secs INTEGER,
                recording_url TEXT,
                hangup_reason TEXT,
                metadata TEXT
            );

            CREATE TABLE IF NOT EXISTS cc_agent_presence (
                agent_id TEXT PRIMARY KEY,
                status TEXT DEFAULT 'offline',
                last_changed_at TEXT DEFAULT (datetime('now')),
                break_reason TEXT
            );

            CREATE TABLE IF NOT EXISTS cc_agent_endpoint (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent_id TEXT,
                endpoint_type TEXT,
                endpoint_value TEXT,
                priority INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS cc_agent_stats (
                agent_id TEXT PRIMARY KEY,
                total_calls INTEGER DEFAULT 0,
                answered_calls INTEGER DEFAULT 0,
                missed_calls INTEGER DEFAULT 0,
                total_talk_secs INTEGER DEFAULT 0,
                total_wrap_secs INTEGER DEFAULT 0
            );
            """
        )
        self.conn.commit()

    def seed_agents(self, agents: list[dict]) -> None:
        assert self.conn is not None
        for a in agents:
            self.conn.execute(
                """INSERT OR REPLACE INTO cc_agent
                   (agent_id, display_name, primary_endpoint, skills, max_concurrency, role, status)
                   VALUES (?, ?, ?, ?, ?, ?, 'offline')""",
                (
                    a["agent_id"],
                    a.get("display_name", a["agent_id"]),
                    a.get("primary_endpoint", a["agent_id"]),
                    ",".join(a.get("skills", [])),
                    a.get("max_concurrency", 1),
                    a.get("role", "agent"),
                ),
            )
        self.conn.commit()

    def seed_skill_groups(self, groups: list[dict]) -> None:
        assert self.conn is not None
        for g in groups:
            self.conn.execute(
                """INSERT OR REPLACE INTO cc_skill_group
                   (skill_group_id, name, skills_required, overflow_groups, sla_target_secs, max_wait_secs)
                   VALUES (?, ?, ?, ?, ?, ?)""",
                (
                    g["skill_group_id"],
                    g.get("name", g["skill_group_id"]),
                    ",".join(g.get("skills_required", [])),
                    ",".join(g.get("overflow_groups", [])),
                    g.get("sla_target_secs", 30),
                    g.get("max_wait_secs", 90),
                ),
            )
        self.conn.commit()

    def seed_acd_policies(self, policies: list[dict]) -> None:
        assert self.conn is not None
        for p in policies:
            self.conn.execute(
                """INSERT OR REPLACE INTO cc_acd_queue
                   (policy_name, strategy, max_queue_size, agent_timeout_secs, wrap_up_time_secs)
                   VALUES (?, ?, ?, ?, ?)""",
                (
                    p["policy_name"],
                    p.get("strategy", "longest_idle"),
                    p.get("max_queue_size", 100),
                    p.get("agent_timeout_secs", 20),
                    p.get("wrap_up_time_secs", 10),
                ),
            )
        self.conn.commit()

    def close(self) -> None:
        if self.conn:
            self.conn.close()
            self.conn = None

    @property
    def url(self) -> str:
        return f"sqlite://{self.db_path}?mode=rwc"

    def build(
        self,
        agents: Optional[list[dict]] = None,
        skill_groups: Optional[list[dict]] = None,
        acd_policies: Optional[list[dict]] = None,
    ) -> str:
        """Create DB with seed data, return the connection URL."""
        self.connect()
        self.create_tables()
        if agents:
            self.seed_agents(agents)
        if skill_groups:
            self.seed_skill_groups(skill_groups)
        if acd_policies:
            self.seed_acd_policies(acd_policies)
        self.close()
        return self.url


def default_agents() -> list[dict]:
    return [
        {
            "agent_id": "1001",
            "display_name": "Agent 1001 (Regression)",
            "primary_endpoint": "1001",
            "skills": ["support", "sales"],
            "max_concurrency": 3,
            "role": "agent",
        },
        {
            "agent_id": "1002",
            "display_name": "Agent 1002 (Regression)",
            "primary_endpoint": "1002",
            "skills": ["support"],
            "max_concurrency": 3,
            "role": "agent",
        },
        {
            "agent_id": "1003",
            "display_name": "Agent 1003 (Regression)",
            "primary_endpoint": "1003",
            "skills": ["support", "sales", "vip"],
            "max_concurrency": 3,
            "role": "agent",
        },
    ]


def default_skill_groups() -> list[dict]:
    return [
        {
            "skill_group_id": "support",
            "skills_required": ["support"],
            "overflow_groups": [],
            "sla_target_secs": 30,
            "max_wait_secs": 90,
        },
        {
            "skill_group_id": "sales",
            "skills_required": ["sales"],
            "overflow_groups": [],
            "sla_target_secs": 30,
            "max_wait_secs": 90,
        },
        {
            "skill_group_id": "vip",
            "skills_required": ["vip"],
            "overflow_groups": ["support"],
            "sla_target_secs": 15,
            "max_wait_secs": 60,
        },
    ]


def default_acd_policies() -> list[dict]:
    return [
        {"policy_name": "default", "strategy": "longest_idle"},
        {"policy_name": "rr_policy", "strategy": "round_robin"},
        {"policy_name": "skill_policy", "strategy": "skill_based"},
        {"policy_name": "least_policy", "strategy": "least_calls"},
    ]
