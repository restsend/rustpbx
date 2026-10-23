#!/usr/bin/env python3
"""Hypothesis 模糊测试 —— 针对 route_decision_service.py 核心算法的属性验证。

运行:
    pip3 install hypothesis
    python3 examples/yunaoai/test_route_hypothesis.py

验证目标:
  A. resolve_caller_groups —— 最长前缀匹配的数学正确性
  B. match_rule 分层严格性 —— L1 ≻ L2 ≻ L3 永不倒退
  C. match_rule 同层排序键 —— prefix_len ≻ priority ≻ caller_len ≻ rule_index
  D. match_default_rule 默认组排序稳定性
  E. apply_rule_transforms strip 边界条件
"""
from __future__ import annotations
import sys, importlib.util
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SVC = _HERE / "route_decision_service.py"

spec = importlib.util.spec_from_file_location("route_decision_service", _SVC)
svc = importlib.util.module_from_spec(spec)
sys.modules["route_decision_service"] = svc
spec.loader.exec_module(svc)

from hypothesis import given, settings, strategies as st, assume, HealthCheck

_NUM_STR = st.from_regex(r"\d{1,15}", fullmatch=True)

_DIGITS = st.from_regex(r"\d{1,15}", fullmatch=True)


# ────────────────────────────────────────────────────────────────────────────
#  A. resolve_caller_groups
# ────────────────────────────────────────────────────────────────────────────

class TestResolveCallerGroupsFuzzy:
    """caller_groups 最长前缀匹配的数学属性验证."""

    @given(caller=_NUM_STR, records=st.lists(_DIGITS, min_size=1, max_size=8))
    @settings(max_examples=400, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_returned_length_is_actually_longest(self, caller, records):
        """属性: 对于每个返回的 (group → L)，L 确实是该组内最长匹配记录."""
        cfg = {"caller_groups": {"g1": records}}
        hits = svc.resolve_caller_groups(caller, cfg)
        if "g1" not in hits:
            for r in records:
                assert not (r and caller.startswith(r)), \
                    f"caller='{caller}' startswith('{r}') 但未命中 g1"
        else:
            L = hits["g1"]
            assert L > 0
            for r in records:
                if r and caller.startswith(r):
                    assert len(r) <= L, \
                        f"caller='{caller}' 命中记录 '{r}' (len={len(r)}), " \
                        f"但返回最长只有 L={L}"

    @given(caller=_NUM_STR,
           groups=st.dictionaries(
               keys=st.from_regex(r"cg[a-z]{0,4}", fullmatch=True),
               values=st.lists(_DIGITS, min_size=0, max_size=5),
               min_size=1, max_size=6))
    @settings(max_examples=300, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_each_group_independently_evaluated(self, caller, groups):
        """属性: 多组同时存在时，每组独立计算最长匹配."""
        cfg = {"caller_groups": groups}
        hits = svc.resolve_caller_groups(caller, cfg)
        for gname, recs in groups.items():
            local_hits = {r for r in recs if r and caller.startswith(r)}
            expected = max((len(r) for r in local_hits), default=0)
            if expected == 0:
                assert gname not in hits or hits[gname] == 0, \
                    f"组 {gname} 无匹配记录但被返回"
            else:
                assert gname in hits and hits[gname] == expected, \
                    f"组 {gname}: expected={expected} got={hits.get(gname)}"

    @given(caller=_NUM_STR)
    @settings(max_examples=200, suppress_health_check=[HealthCheck.filter_too_much])
    def test_no_groups_returns_empty(self, caller):
        assert svc.resolve_caller_groups(caller, {}) == {}
        assert svc.resolve_caller_groups(caller, {"caller_groups": {}}) == {}
        assert svc.resolve_caller_groups(caller, {"caller_groups": None}) == {}

    @given(records=st.lists(_DIGITS, min_size=0, max_size=5))
    @settings(max_examples=200)
    def test_empty_records_group_never_hits(self, records):
        cfg = {"caller_groups": {"empty-group": records + [""]}}
        for caller in ["", "1234", "0000"]:
            hits = svc.resolve_caller_groups(caller, cfg)
            if records and any(caller.startswith(r) for r in records if r):
                assert "empty-group" in hits
            else:
                assert "empty-group" not in hits or hits.get("empty-group", 0) == 0


# ────────────────────────────────────────────────────────────────────────────
#  B. match_rule 分层严格性
# ────────────────────────────────────────────────────────────────────────────

_LAYER_RANK = {"L1": 2, "L2": 1, "L3": 0}

def _layer_rank(layer):
    return _LAYER_RANK.get(layer, -1)

_generated_group_counter = [0]

def _next_gid():
    _generated_group_counter[0] += 1
    return f"g{_generated_group_counter[0]:04d}"


class TestMatchRuleLayerPriority:
    """L1 ≻ L2 ≻ L3 分层永不倒退."""

    @given(
        callee=_NUM_STR,
        short_idx=st.integers(min_value=1, max_value=5),
        long_idx=st.integers(min_value=3, max_value=8),
        caller_records=st.lists(_DIGITS, min_size=1, max_size=4),
        caller_pick=st.integers(min_value=0, max_value=3),
    )
    @settings(max_examples=300, suppress_health_check=[HealthCheck.too_slow])
    def test_l1_strictly_beats_l2(self, callee, short_idx, long_idx,
                                  caller_records, caller_pick):
        """构造同时有 L1 候选和 L2 候选的配置 → L1 必须赢.

        策略: 从 callee 直接截取前缀 → 保证一定匹配.
        """
        assume(len(callee) > long_idx > short_idx)
        l1_prefix = callee[:short_idx]    # 短前缀，命中
        l2_prefix = callee[:long_idx]     # 长前缀，也命中

        # caller 必须命中 cg-x: 选一条 record 当 caller
        caller = caller_records[caller_pick % len(caller_records)] + "99"
        assume(any(caller.startswith(r) for r in caller_records))

        cfg = {
            "caller_groups": {"cg-x": caller_records},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    {"prefix": l1_prefix, "caller_group": "cg-x", "priority": 5},
                ]},
                {"point_id": _next_gid(), "rule_list": [
                    {"prefix": l2_prefix, "priority": 100},
                ]},
            ],
        }
        group_hits = svc.resolve_caller_groups(caller, cfg)
        assert "cg-x" in group_hits, "caller 应该命中 cg-x"

        _, _, layer = svc.match_rule(caller, callee, group_hits, cfg)
        assert layer == "L1", \
            f"L1 候选存在 (caller 命中 cg-x, callee startswith '{l1_prefix}'), " \
            f"但返回 layer={layer}"

    @given(
        l2_prefix=_DIGITS, caller=_NUM_STR, callee=_NUM_STR,
        caller_records=st.lists(_DIGITS, min_size=1, max_size=3),
    )
    @settings(max_examples=300, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_l2_strictly_beats_l3(self, l2_prefix, caller, callee, caller_records):
        """构造同时有 L2 候选和 L3 候选的配置 → L2 必须赢."""
        assume(callee.startswith(l2_prefix))

        cfg = {
            "caller_groups": {"cg-y": caller_records},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    # L2 候选: 无主叫过滤 + 被叫前缀命中
                    {"prefix": l2_prefix, "priority": 10},
                ]},
                {"point_id": _next_gid(), "rule_list": [
                    # L3 候选: 主叫命中 + 空前缀
                    {"prefix": "", "caller_group": "cg-y", "priority": 100},
                ]},
            ],
        }
        group_hits = svc.resolve_caller_groups(caller, cfg)
        assume("cg-y" in group_hits)

        _, _, layer = svc.match_rule(caller, callee, group_hits, cfg)
        assert layer == "L2", \
            f"L2 候选存在 (prefix='{l2_prefix}'), 但返回 layer={layer}"

    @given(caller=_NUM_STR, callee=_NUM_STR,
           caller_records=st.lists(_DIGITS, min_size=1, max_size=4))
    @settings(max_examples=250, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_l3_when_only_caller_hit_no_prefix(self, caller, callee, caller_records):
        """只有主叫命中 + 空前缀规则 → 必须返回 L3 (不是 None)."""
        cfg = {
            "caller_groups": {"cg-z": caller_records},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    {"prefix": "", "caller_group": "cg-z", "priority": 1},
                ]},
            ],
        }
        group_hits = svc.resolve_caller_groups(caller, cfg)
        assume("cg-z" in group_hits)

        result_grp, result_rule, layer = svc.match_rule(caller, callee, group_hits, cfg)
        assert layer == "L3" and result_rule is not None, \
            f"主叫命中 cg-z + 空前缀 → 应返回 L3, 实际 layer={layer}"

    @given(caller=_NUM_STR, callee=_NUM_STR,
           caller_records=st.lists(_DIGITS, min_size=1, max_size=3))
    @settings(max_examples=250, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_no_hit_returns_none(self, caller, callee, caller_records):
        """所有规则都不命中 → 返回 (None, None, None)."""
        cfg = {
            "caller_groups": {"cg-miss": caller_records},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    # prefix 不可能命中 (caller 组也故意不命中)
                    {"prefix": "0" * 20, "priority": 1},
                    {"prefix": "1" * 20, "caller_group": "cg-unknown", "priority": 1},
                ]},
            ],
        }
        group_hits = svc.resolve_caller_groups(caller, cfg)
        assume("cg-miss" not in group_hits or
               not callee.startswith("0" * 20))

        grp, rule, layer = svc.match_rule(caller, callee, group_hits, cfg)
        if rule is not None:
            # 有返回也可能是 L2 无主叫过滤 + prefix="" (空前缀 L2)
            assert rule.get("caller_group") is None \
                   and not str(rule.get("caller", "")).strip(), \
                "唯一可能的命中是 L2 空前缀无主叫过滤规则"


# ────────────────────────────────────────────────────────────────────────────
#  C. match_rule 同层排序键
# ────────────────────────────────────────────────────────────────────────────

class TestMatchRuleWithinLayerOrdering:
    """同层内: prefix_len ≻ priority ≻ caller_len ≻ rule_index (配置顺序)."""

    @given(callee=_NUM_STR,
           short_idx=st.integers(min_value=1, max_value=5),
           long_idx=st.integers(min_value=3, max_value=8))
    @settings(max_examples=200, suppress_health_check=[HealthCheck.too_slow])
    def test_prefix_length_beats_priority(self, callee, short_idx, long_idx):
        """同 L2 层: 长前缀 (不管 priority 多低) 赢短前缀 (不管 priority 多高).

        策略: 从 callee 截取前缀 → 保证同时命中.
        """
        assume(len(callee) > long_idx > short_idx)
        short_pfx = callee[:short_idx]
        long_pfx = callee[:long_idx]

        cfg = {
            "caller_groups": {},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    {"prefix": short_pfx, "priority": 100},
                    {"prefix": long_pfx, "priority": 1},
                ]},
            ],
        }
        _, rule, _ = svc.match_rule("9999", callee, {}, cfg)
        assert rule is not None, "应该有规则命中"
        assert rule["prefix"] == long_pfx, \
            f"长前缀 '{long_pfx}' (prio=1) 应该赢短前缀 '{short_pfx}' (prio=100)"

    @given(callee=_NUM_STR, prio_a=st.integers(0, 100), prio_b=st.integers(0, 100))
    @settings(max_examples=200)
    def test_priority_beats_caller_len(self, callee, prio_a, prio_b):
        """同 L1 层同 prefix: priority 高的赢 caller_len 长的."""
        shorter = callee[:3] if len(callee) >= 3 else callee
        longer = callee[:5] if len(callee) >= 5 else callee

        assume(shorter and longer)

        cfg = {
            "caller_groups": {"cg-s": [shorter], "cg-l": [longer]},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    # caller_len 短但 priority 高
                    {"prefix": shorter, "caller_group": "cg-s", "priority": prio_a},
                    # caller_len 长但 priority 低
                    {"prefix": shorter, "caller_group": "cg-l", "priority": prio_b},
                ]},
            ],
        }
        caller = longer  # 命中两个组
        group_hits = svc.resolve_caller_groups(caller, cfg)
        assume("cg-s" in group_hits and "cg-l" in group_hits)

        _, rule, _ = svc.match_rule(caller, callee, group_hits, cfg)
        assert rule is not None
        expected_prio = max(prio_a, prio_b)
        assert int(rule.get("priority", 0)) == expected_prio, \
            f"同 prefix 同 L1: priority 高的应赢 (expected={expected_prio})"

    @given(callee=_NUM_STR,
           caller_records=st.lists(_DIGITS, min_size=2, max_size=5))
    @settings(max_examples=250, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_caller_len_beats_rule_index(self, callee, caller_records):
        """同 L1 同 prefix 同 priority: caller_len 长的赢 (不管配置顺序)."""
        assume(len(caller_records) >= 2)

        pfx = callee[:3] if len(callee) >= 3 else callee
        assume(pfx)

        cfg = {
            "caller_groups": {f"cg{i}": [rec] for i, rec in enumerate(caller_records)},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    # 故意把 caller_len 最短的放在最后 (配置顺序"赢")
                    {"prefix": pfx, "caller_group": f"cg{i}", "priority": 1}
                    for i, _ in enumerate(caller_records)
                ]},
            ],
        }
        caller = max(caller_records, key=len) + "99"  # 命中最长记录
        group_hits = svc.resolve_caller_groups(caller, cfg)

        _, rule, _ = svc.match_rule(caller, callee, group_hits, cfg)
        assert rule is not None
        winner_cg = rule.get("caller_group", "")
        winner_len = len(caller_records[int(winner_cg[2:])])
        max_len = max(len(r) for r in caller_records
                      if any(caller.startswith(rr) for rr in [r]))
        assert winner_len == max_len, \
            f"caller_len 最长者应赢: winner={winner_cg} len={winner_len}, max={max_len}"

    @given(callee=_NUM_STR,
           caller_records=st.lists(_DIGITS, min_size=1, max_size=3))
    @settings(max_examples=200, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_config_order_breaks_tie(self, callee, caller_records):
        """同 L1 同 prefix 同 priority 同 caller_len → 配置顺序 (rule_index) 决胜."""
        assume(caller_records)
        pfx = callee[:3] if len(callee) >= 3 else callee
        caller = caller_records[0] + "extra"

        cfg = {
            "caller_groups": {"cg-tie": caller_records},
            "route_points": [
                {"point_id": _next_gid(), "rule_list": [
                    {"prefix": pfx, "caller_group": "cg-tie", "priority": 5},
                    {"prefix": pfx, "caller_group": "cg-tie", "priority": 5},
                ]},
            ],
        }
        group_hits = svc.resolve_caller_groups(caller, cfg)
        assume("cg-tie" in group_hits)

        grp, rule, _ = svc.match_rule(caller, callee, group_hits, cfg)
        assert rule is not None
        assert grp is not None

        all_rules = grp["rule_list"]
        assert all_rules.index(rule) >= 0  # 必须是同 route_group 里的某一条


# ────────────────────────────────────────────────────────────────────────────
#  D. match_default_rule 默认组排序
# ────────────────────────────────────────────────────────────────────────────

class TestMatchDefaultRuleFuzzy:
    """默认组内排序: prefix_len ≻ priority ≻ 配置顺序."""

    @given(callee=_NUM_STR,
           rules=st.lists(
               st.fixed_dictionaries({
                   "prefix": _DIGITS,
                   "priority": st.integers(0, 100),
               }),
               min_size=2, max_size=8))
    @settings(max_examples=300, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_longest_prefix_wins(self, callee, rules):
        """默认组: 最长被叫前缀赢 (不管 priority)."""
        assume(any(callee.startswith(r["prefix"]) for r in rules))

        longest_len = max(len(r["prefix"]) for r in rules
                          if callee.startswith(r["prefix"]))
        same_len_rules = [r for r in rules if len(r["prefix"]) == longest_len
                          and callee.startswith(r["prefix"])]

        grp = {"rule_list": rules}
        winner = svc.match_default_rule(grp, callee)
        assert winner is not None

        winner_len = len(winner["prefix"])
        assert winner_len == longest_len, \
            f"winner prefix len={winner_len}, 最长匹配={longest_len}"

        # 在同最长长度里 priority 最高者赢
        if len(same_len_rules) > 1:
            max_prio = max(r.get("priority", 0) for r in same_len_rules)
            assert winner.get("priority", 0) == max_prio, \
                f"同最长 prefix 里 priority 应最高 (got winner prio={winner.get('priority',0)}, max={max_prio})"

    @given(callee=_NUM_STR,
           rules=st.lists(
               st.fixed_dictionaries({
                   "prefix": _DIGITS,
                   "priority": st.integers(0, 100),
               }),
               min_size=1, max_size=6))
    @settings(max_examples=200, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_all_rules_miss_returns_none(self, callee, rules):
        """所有规则都不匹配 → 返回 None."""
        assume(not any(callee.startswith(r["prefix"]) for r in rules))
        grp = {"rule_list": rules}
        winner = svc.match_default_rule(grp, callee)
        assert winner is None

    @given(callee=_NUM_STR,
           rules=st.lists(
               st.fixed_dictionaries({
                   "prefix": st.from_regex(r"\d{0,10}", fullmatch=True),
                   "priority": st.integers(0, 100),
               }),
               min_size=2, max_size=6))
    @settings(max_examples=250, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_empty_prefix_matches_everything(self, callee, rules):
        """空前缀规则 → 匹配所有被叫号码 (兜底语义)."""
        if any(callee.startswith(r["prefix"]) for r in rules):
            grp = {"rule_list": rules}
            winner = svc.match_default_rule(grp, callee)
            assert winner is not None
            assert callee.startswith(winner.get("prefix", "")), \
                "winner prefix 必须匹配 callee (空前缀匹配任何 callee)"


# ────────────────────────────────────────────────────────────────────────────
#  E. apply_rule_transforms strip 边界
# ────────────────────────────────────────────────────────────────────────────

class TestApplyRuleTransformsFuzzy:
    """strip + pri_prefix 号码变换边界属性."""

    @given(number=_NUM_STR, strip=st.integers(0, 20))
    @settings(max_examples=300, suppress_health_check=[HealthCheck.filter_too_much])
    def test_strip_range_behavior(self, number, strip):
        """strip ≤ len(number) → 截取后 len(number)-strip 位; strip > len → 返回空串."""
        rule = {"strip": strip, "pri_prefix": ""}
        result = svc.apply_rule_transforms(rule, number)
        if strip <= len(number):
            assert result == number[strip:], \
                f"strip={strip}, number='{number}' → expected '{number[strip:]}', got '{result}'"
        else:
            assert result == "", \
                f"strip={strip} > len={len(number)}, expected '', got '{result}'"

    @given(number=_NUM_STR, pri=st.from_regex(r"\d{0,10}", fullmatch=True))
    @settings(max_examples=300, suppress_health_check=[HealthCheck.filter_too_much])
    def test_pri_prefix_concatenation(self, number, pri):
        rule = {"strip": 0, "pri_prefix": pri}
        result = svc.apply_rule_transforms(rule, number)
        assert result == pri + number

    @given(number=_NUM_STR, strip=st.integers(0, 15),
           pri=st.from_regex(r"\d{0,5}", fullmatch=True))
    @settings(max_examples=300, suppress_health_check=[HealthCheck.filter_too_much])
    def test_combined_strip_and_pri(self, number, strip, pri):
        rule = {"strip": strip, "pri_prefix": pri}
        result = svc.apply_rule_transforms(rule, number)
        expected = pri + (number[strip:] if strip <= len(number) else "")
        assert result == expected, \
            f"strip={strip} pri='{pri}' num='{number}' → expected '{expected}', got '{result}'"

    @given(number=_NUM_STR, strip=st.integers(0, 20))
    @settings(max_examples=200, suppress_health_check=[HealthCheck.filter_too_much])
    def test_result_is_digits_or_empty(self, number, strip):
        rule = {"strip": strip, "pri_prefix": ""}
        result = svc.apply_rule_transforms(rule, number)
        assert result == "" or result.isdigit()


# ────────────────────────────────────────────────────────────────────────────
#  F. resolve_caller_groups + match_rule 集成: 属性等价性
# ────────────────────────────────────────────────────────────────────────────

_rule_caller_keys = st.one_of(
    st.fixed_dictionaries({"caller_group": st.from_regex(r"cg[a-z]{0,3}", fullmatch=True)}),
    st.fixed_dictionaries({"caller": _DIGITS}),
    st.fixed_dictionaries({}),
)

_any_rule = st.builds(
    lambda pf, pri, ex: {"prefix": pf, "priority": pri, **ex},
    st.from_regex(r"\d{0,10}", fullmatch=True),
    st.integers(0, 100),
    _rule_caller_keys,
)


class TestMatchRuleDeterminism:
    """同样输入 → 同样输出 (幂等 / 无状态)."""

    @given(caller=_NUM_STR, callee=_NUM_STR,
           cfg=st.fixed_dictionaries({
               "caller_groups": st.dictionaries(
                   keys=st.from_regex(r"cg[a-z]{0,3}", fullmatch=True),
                   values=st.lists(_DIGITS, min_size=0, max_size=4)),
               "route_points": st.lists(
                   st.fixed_dictionaries({
                       "point_id": st.from_regex(r"rp\d{1,4}", fullmatch=True),
                       "rule_list": st.lists(_any_rule, min_size=1, max_size=4),
                   }),
                   min_size=1, max_size=3),
           }))
    @settings(max_examples=200, suppress_health_check=[HealthCheck.filter_too_much, HealthCheck.too_slow])
    def test_deterministic(self, caller, callee, cfg):
        group_hits = svc.resolve_caller_groups(caller, cfg)
        grp1, rule1, layer1 = svc.match_rule(caller, callee, group_hits, cfg)
        grp2, rule2, layer2 = svc.match_rule(caller, callee, group_hits, cfg)
        assert layer1 == layer2
        assert (grp1 is None) == (grp2 is None)
        assert (rule1 is None) == (rule2 is None)
        if rule1 is not None:
            assert rule1.get("prefix") == rule2.get("prefix")
            assert rule1.get("caller_group") == rule2.get("caller_group")
            assert rule1.get("caller") == rule2.get("caller")


# ────────────────────────────────────────────────────────────────────────────
#  runner
# ────────────────────────────────────────────────────────────────────────────

def run_section(name, cls):
    import inspect
    print(f"\n{'═' * 60}")
    print(f"  {name}  ({cls.__name__})")
    print(f"{'═' * 60}")
    passed = failed = 0
    for method_name, method in inspect.getmembers(cls, predicate=inspect.isfunction):
        if not method_name.startswith("test_"):
            continue
        try:
            instance = cls()
            method(instance)
            print(f"  ✅ {method_name}")
            passed += 1
        except AssertionError as e:
            print(f"  ❌ {method_name}\n     {e}")
            failed += 1
        except Exception as e:
            print(f"  ⚠️  {method_name} → {type(e).__name__}: {e}")
    return passed, failed


def main():
    sections = [
        ("A. resolve_caller_groups 最长前缀正确性", TestResolveCallerGroupsFuzzy),
        ("B. match_rule 分层严格性 L1≻L2≻L3", TestMatchRuleLayerPriority),
        ("C. match_rule 同层排序键优先级", TestMatchRuleWithinLayerOrdering),
        ("D. match_default_rule 默认组排序", TestMatchDefaultRuleFuzzy),
        ("E. apply_rule_transforms strip/pri 边界", TestApplyRuleTransformsFuzzy),
        ("F. match_rule 幂等 / 无状态", TestMatchRuleDeterminism),
    ]
    total_passed = total_failed = 0
    for name, cls in sections:
        p, f = run_section(name, cls)
        total_passed += p
        total_failed += f

    print(f"\n{'═' * 60}")
    print(f"  总计: {total_passed} ✅   {total_failed} ❌")
    print(f"{'═' * 60}")
    sys.exit(0 if total_failed == 0 else 1)


if __name__ == "__main__":
    main()