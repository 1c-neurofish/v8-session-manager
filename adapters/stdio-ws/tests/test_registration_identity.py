"""RED: WS session.register shape and genuine binding admission (spec R1/R2)."""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import (FIXTURE_BINDING, FIXTURE_CONFIG, FakeWS,
                      SDKMemoryPeer, load, real_descriptors)


class RegisterShape(unittest.IsolatedAsyncioTestCase):
    async def test_register_exact_fields_no_database_key(self):
        provider = load("provider")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = provider.Provider(FIXTURE_CONFIG, FIXTURE_BINDING,
                                  mcp_session=peer.client_session, relay_port=peer.relay_port, ws=ws)
            task = asyncio.create_task(p.register())
            req = await ws.manager_recv_json()
            self.assertEqual(req["jsonrpc"], "2.0")
            self.assertEqual(req["method"], "session.register")
            self.assertIsInstance(req["id"], str)
            params = req["params"]
            self.assertEqual(params["kind"], "testpilot")
            self.assertEqual(params["infobase_name"], "gbig_pam_ai")
            self.assertEqual(params["cluster_server"], "onec-infra")
            self.assertEqual(params["ib_session_number"], 4242)
            self.assertIsInstance(params["ib_session_number"], int)
            self.assertNotIn("database_key", params)
            self.assertIn("tools", params)
            await ws.manager_reply(req["id"], result={"session_id": "s-1"})
            await task

    async def test_provider_pid_not_reported_as_testclient(self):
        provider = load("provider")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = provider.Provider(FIXTURE_CONFIG, FIXTURE_BINDING,
                                  mcp_session=peer.client_session, relay_port=peer.relay_port, ws=ws)
            task = asyncio.create_task(p.register())
            req = await ws.manager_recv_json()
        # host_id/pid, if present, describe the ACTUAL provider process — the
        # native TestClient identity lives only in the binding receipt fields
            # (F4: synthetic fixture own_pid deliberately differs, see harness).
            self.assertNotEqual(FIXTURE_BINDING["own_pid"], os.getpid())
            if "pid" in req["params"]:
                self.assertEqual(req["params"]["pid"], os.getpid())
            self.assertNotEqual(req["params"].get("pid"),
                                FIXTURE_BINDING["own_pid"])
            await ws.manager_reply(req["id"], result={"session_id": "s-1"})
            await task

    async def test_register_error_rejected(self):
        provider = load("provider")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = provider.Provider(FIXTURE_CONFIG, FIXTURE_BINDING,
                                  mcp_session=peer.client_session, relay_port=peer.relay_port, ws=ws)
            task = asyncio.create_task(p.register())
            req = await ws.manager_recv_json()
            await ws.manager_reply(req["id"], error={"code": -32603, "message": "no"})
            with self.assertRaises(Exception):
                await task


class BindingAdmission(unittest.TestCase):
    def test_ib_session_number_must_be_uint32_not_guid(self):
        policy = load("policy")
        bad = dict(FIXTURE_BINDING,
                   ib_session_number="d8cfbba6-15c7-4065-4277-b6abac9e3ab6")
        with self.assertRaises(Exception):
            policy.validate_binding(bad)
        policy.validate_binding(FIXTURE_BINDING)  # sane fixture passes

    def test_missing_required_binding_fields_denied(self):
        policy = load("policy")
        for key in ("own_pid", "process_birth", "testclient_port",
                    "connection_id", "ib_session_number"):
            bad = {k: v for k, v in FIXTURE_BINDING.items() if k != key}
            with self.assertRaises(Exception, msg=key):
                policy.validate_binding(bad)

    def test_unknown_binding_keys_rejected(self):
        policy = load("policy")
        bad = dict(FIXTURE_BINDING, database_key="forged")
        with self.assertRaises(Exception):
            policy.validate_binding(bad)


class AdmissionIsNotShape(unittest.TestCase):
    """F4: binding validation proves shape only; liveness/port/native gates are
    separate runtime checks. Offline probes are injected, never real OS/RAS."""

    def test_dead_own_pid_denied_despite_valid_shape(self):
        provider = load("provider")
        load("policy").validate_binding(FIXTURE_BINDING)  # shape passes
        with self.assertRaises(Exception):
            provider.check_admission(FIXTURE_BINDING,
                                     pid_alive=lambda pid: False,
                                     port_owner=lambda host, port: None)

    def test_port_owned_by_other_pid_denied(self):
        provider = load("provider")
        with self.assertRaises(Exception):
            provider.check_admission(
                FIXTURE_BINDING, pid_alive=lambda pid: True,
                port_owner=lambda host, port: 999999)  # not own_pid 424242

    def test_all_probes_matching_passes_offline(self):
        provider = load("provider")
        provider.check_admission(
            FIXTURE_BINDING,
            pid_alive=lambda pid: pid == FIXTURE_BINDING["own_pid"],
            port_owner=lambda host, port: FIXTURE_BINDING["own_pid"])


import asyncio  # noqa: E402  (kept at bottom to keep top imports minimal)

if __name__ == "__main__":
    unittest.main()
