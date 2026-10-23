#!/usr/bin/env python3
"""全场景路由回归测试 —— 覆盖 route_decision_service.py 的核心路由决策路径。

运行:
    python3 examples/yunaoai/test_route_decision.py -v
"""
from __future__ import annotations
import json, sys, unittest, importlib.util
from pathlib import Path

_HERE = Path(__file__).resolve().parent
_SVC = _HERE / "route_decision_service.py"
_CFG = _HERE / "config" / "3201.json"

spec = importlib.util.spec_from_file_location("route_decision_service", _SVC)
svc = importlib.util.module_from_spec(spec)
sys.modules["route_decision_service"] = svc
spec.loader.exec_module(svc)

def _load_cfg():
    with open(_CFG) as f:
        return json.load(f)


class TestIsPassiveGateway(unittest.TestCase):
    """源码语义 (line ~294):
        dest 为空 OR dest == 'sip:0.0.0.0' → passive=True
        带端口的 sip:0.0.0.0:5060 → 精确匹配失败 → False
    """

    def test_passive_exact_match_0_0_0_0(self):
        self.assertTrue(svc.is_passive_gateway({"dest": "sip:0.0.0.0"}))

    def test_passive_empty_dest(self):
        self.assertTrue(svc.is_passive_gateway({"dest": ""}))

    def test_passive_missing_dest_key(self):
        self.assertTrue(svc.is_passive_gateway({}))

    def test_full_port_not_passive(self):
        self.assertFalse(svc.is_passive_gateway({"dest": "sip:0.0.0.0:5060"}))

    def test_ip_peer_is_not_passive(self):
        self.assertFalse(svc.is_passive_gateway({"dest": "sip:100.88.0.204:38890"}))

    def test_trunk_register_is_not_passive(self):
        self.assertFalse(svc.is_passive_gateway({
            "dest": "sip:ya-pbx.meshcc.cn:38896",
            "register_enabled": True}))


class TestGatewayTargets(unittest.TestCase):
    """gateway_targets 返回三元组 (targets, rtp_config, network_profile_id)。"""

    def setUp(self):
        self.cfg = _load_cfg()
        svc.SYNC_STATE = {"ok": True}
        svc.USERS = {}

    def test_passive_gateway_with_profile(self):
        """reg879830001: dest=0.0.0.0 + profile=tailscale → npid='tailscale'."""
        svc.USERS["879830001"] = {"contact": "100.88.0.204:38890"}
        _, _, npid = svc.gateway_targets("8798", ["reg879830001"], self.cfg)
        self.assertEqual(npid, "tailscale")

    def test_ip_peer_with_profile(self):
        """liusy: dest=sip:100.88.0.204:38890 + profile=tailscale → npid='tailscale'."""
        _, _, npid = svc.gateway_targets("1551", ["liusy"], self.cfg)
        self.assertEqual(npid, "tailscale")

    def test_trunk_without_profile(self):
        """ya-pbx: 无 profile → None."""
        _, _, npid = svc.gateway_targets("008613800138000", ["ya-pbx"], self.cfg)
        self.assertIsNone(npid)

    def test_internal_gateway_without_profile(self):
        """lvcx: 无 profile → None."""
        _, _, npid = svc.gateway_targets("8120", ["lvcx"], self.cfg)
        self.assertIsNone(npid)

    def test_mixed_gwlist_takes_first_profile(self):
        """['lvcx','liusy','ya-pbx'] → 取第一个非空 'tailscale'."""
        _, _, npid = svc.gateway_targets("x", ["lvcx", "liusy", "ya-pbx"], self.cfg)
        self.assertEqual(npid, "tailscale")

    def test_multiple_profile_gateways_take_first(self):
        """['liusy','yacc55-fs'] 都 tailscale → 返回 'tailscale'."""
        _, _, npid = svc.gateway_targets("x", ["liusy", "yacc55-fs"], self.cfg)
        self.assertEqual(npid, "tailscale")

    def test_all_gateways_without_profile(self):
        _, _, npid = svc.gateway_targets("x", ["lvcx", "ya-pbx"], self.cfg)
        self.assertIsNone(npid)

    def test_rtp_config_collects_external_ip(self):
        """reg879830001 有 external_ip=100.88.0.1 → 收集之."""
        svc.USERS["879830001"] = {"contact": "100.88.0.204:38890"}
        _, rtp, _ = svc.gateway_targets("8798", ["reg879830001"], self.cfg)
        self.assertEqual(rtp.get("external_ip"), "100.88.0.1")

    def test_tuple_shape_is_three(self):
        result = svc.gateway_targets("x", ["liusy"], self.cfg)
        self.assertEqual(len(result), 3,
                         "gateway_targets 必须返回 3-tuple (防止回退成 2-tuple)")

    def test_disabled_gateway_targets_skipped(self):
        """enabled=False 的 gateway 不生成 targets (回归防护)."""
        gw = dict(self.cfg["gateways"]["liusy"])
        gw["enabled"] = False
        cfg = {"gateways": {"liusy": gw}}
        svc.SYNC_STATE = {"ok": True}
        targets, _, _ = svc.gateway_targets("x", ["liusy"], cfg)
        self.assertEqual(targets, [])

    def test_missing_gateway_skipped(self):
        targets, _, _ = svc.gateway_targets("x", ["nonexistent"], self.cfg)
        self.assertEqual(targets, [])


class TestApplyRuleTransforms(unittest.TestCase):
    """源码 (line ~1230): out[strip:] 直接 cut 前 strip 位."""

    def test_strip_and_pri_prefix(self):
        # "59123456789" 去掉前 3 位 "591" → "23456789" + "00" → "0023456789"
        rule = {"strip": 3, "pri_prefix": "00"}
        self.assertEqual(svc.apply_rule_transforms(rule, "59123456789"),
                         "0023456789")

    def test_strip_only(self):
        rule = {"strip": 2, "pri_prefix": ""}
        self.assertEqual(svc.apply_rule_transforms(rule, "591234"), "1234")

    def test_pri_prefix_only(self):
        rule = {"strip": 0, "pri_prefix": "+86"}
        self.assertEqual(svc.apply_rule_transforms(rule, "13800138000"),
                         "+8613800138000")

    def test_no_transform(self):
        rule = {"strip": 0, "pri_prefix": ""}
        self.assertEqual(svc.apply_rule_transforms(rule, "12345"), "12345")

    def test_empty_rule(self):
        self.assertEqual(svc.apply_rule_transforms({}, "12345"), "12345")


if __name__ == "__main__":
    unittest.main(verbosity=2)
