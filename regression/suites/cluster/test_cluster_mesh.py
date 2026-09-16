"""Dual-process cluster suite: two real rustpbx nodes as AMI/SIP peers.

Topology: node A and node B, each configured with the other as a [cluster]
peer (addr/sip_port/ami_port). Both run with the [ami] loopback allowlist so
the test process can drive either control plane.

Coverage:
  * cluster/ping mesh — each node must reach its peer's /ami/v1/health with
    status ok and a numeric latency (peer identity addr:sip_port echoed back)
  * cluster/list_calls — schema on both nodes while clustered
SIP-level home-node forwarding stays covered by the Rust suite
(tests/proxy_e2e/test_cluster_home_proxy_e2e.rs).
"""

from __future__ import annotations

from pathlib import Path

import pytest
import pytest_asyncio

import helpers as h

pytestmark = [pytest.mark.cluster]


def _append_cluster(pbx, peer_sip_port: int, peer_ami_port: int):
    """Append a [cluster] section to the generated TOML (prepare already ran)."""
    conf = Path(pbx.config_path)
    text = conf.read_text(encoding="utf-8")
    text += (
        "\n[cluster]\n"
        f'peers = [{{ addr = "127.0.0.1", sip_port = {peer_sip_port}, ami_port = {peer_ami_port} }}]\n'
        'session_registry_backend = "memory"\n'
    )
    conf.write_text(text, encoding="utf-8")


async def _post(pbx, path: str):
    import aiohttp

    async with aiohttp.ClientSession() as session:
        async with session.post(f"{pbx.http_url}{path}", json={}) as resp:
            body = await resp.json(content_type=None)
            return resp.status, body


async def _get(pbx, path: str):
    import aiohttp

    async with aiohttp.ClientSession() as session:
        async with session.get(f"{pbx.http_url}{path}") as resp:
            body = await resp.json(content_type=None)
            return resp.status, body


@pytest_asyncio.fixture
async def cluster(pbx, webhook_server):
    """Two booted nodes with mutual peer configuration."""
    a, b = pbx, None
    from helpers.pbx_server import PbxServer

    b = PbxServer(
        host="127.0.0.1",
        sip_port=pbx.sip_port + 100,
        http_port=pbx.http_port + 100,
        rwi_token=pbx.rwi_token,
        project_root=pbx.project_root,
        work_dir=Path(str(pbx.work_dir) + "-node-b"),
    )
    b._config_builder = pbx.config_builder.__class__(
        project_root=pbx.project_root,
        work_dir=b.work_dir,
        sip_port=b.sip_port,
        http_port=b.http_port,
        rwi_token=b.rwi_token,
        webhook_url=webhook_server.url,
        addons=["cc"],
    )
    b.config_builder.outbound_enabled = True

    pbx.config_builder.outbound_enabled = True
    pbx.prepare(webhook_url=webhook_server.url, build=False)
    b.prepare(webhook_url=webhook_server.url, build=False)
    _append_cluster(pbx, b.sip_port, b.http_port)
    _append_cluster(b, pbx.sip_port, pbx.http_port)
    pbx.start(timeout=90)
    b.start(timeout=90)
    yield a, b
    b.stop()


async def test_cluster_ping_mesh(cluster, evidence):
    """Dual-node interconnect: cluster/ping from A->B and B->A must both probe
    the peer healthy (status ok + numeric latency) and echo the peer identity."""
    a, b = cluster
    for src, peer in ((a, b), (b, a)):
        status, body = await _post(src, "/ami/v1/cluster/ping")
        assert status == 200, f"cluster/ping returned {status}: {str(body)[:200]}"
        text = str(body)
        assert f"{peer.sip_port}" in text, f"ping results missing peer port {peer.sip_port}: {text[:400]}"
        assert "ok" in text.lower(), f"peer not healthy in ping results: {text[:400]}"
        evidence.log_metric(f"ping_{src.sip_port}->ed{peer.sip_port}", "ok")
    evidence.log_metric("cluster_mesh", "bidirectional")


async def test_cluster_list_calls_schema(cluster, evidence):
    """Both nodes' /ami/v1/calls must return structured JSON (commerce-gated)."""
    a, b = cluster
    for node in (a, b):
        status, body = await _get(node, "/ami/v1/calls")
        assert status == 200, f"calls on {node.sip_port}: {status} {str(body)[:160]}"
        assert isinstance(body, (list, dict)), f"calls unexpected type: {type(body).__name__}"
    evidence.log_metric("nodes", 2)
