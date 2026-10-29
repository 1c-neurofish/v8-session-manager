"""RED: tool descriptors/profile fidelity and fail-closed action policy (R5/R6)."""
import asyncio
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import (EXPECTED_ACTION_TOTAL, EXPECTED_TOOLS, FIXTURE_BINDING,
                      FIXTURE_CONFIG, FakeWS, SDKMemoryPeer, load,
                      real_descriptors)

DANGEROUS = ["connect", "disconnect", "launch_client", "stop_client",
             "run_scenario", "write_content_to_file", "record_finish",
             "execute_command"]


class DescriptorProfile(unittest.TestCase):
    def test_inventory_publishes_exactly_10_tools_153_actions(self):
        self.assertEqual(sum(EXPECTED_TOOLS.values()), EXPECTED_ACTION_TOTAL)

    def test_policy_loads_inventory_and_counts(self):
        policy = load("policy")
        inv = policy.load_inventory()
        self.assertEqual(set(inv), set(EXPECTED_TOOLS))
        for name, count in EXPECTED_TOOLS.items():
            self.assertEqual(len(inv[name]), count, name)


class ActionPolicy(unittest.TestCase):
    def test_unknown_action_denied(self):
        policy = load("policy")
        pol = policy.default_policy()
        self.assertFalse(pol.allows("tc_field", "drop_everything"))

    def test_dangerous_actions_denied_by_default(self):
        policy = load("policy")
        pol = policy.default_policy()
        for action in DANGEROUS:
            self.assertFalse(pol.allows("tc_session", action), action)
            self.assertFalse(pol.allows("tc_scenario", action), action)
            self.assertFalse(pol.allows("tc_doc", action), action)
            self.assertFalse(pol.allows("tc_window", action), action)

    def test_default_policy_denies_all_until_reviewed(self):
        # No positive business actions before reviewed synthetic policy;
        # unapproved branches are DENIED (negative coverage only).
        policy = load("policy")
        pol = policy.default_policy()
        self.assertFalse(pol.allows("tc_field", "input_text"))


class UnknownActionBeforeDispatch(unittest.IsolatedAsyncioTestCase):
    async def test_tool_call_unknown_action_never_reaches_child(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = provider.Provider(FIXTURE_CONFIG, FIXTURE_BINDING,
                                  mcp_session=peer.client_session, relay_port=peer.relay_port, ws=ws,
                                  policy=policy.default_policy())
            reg = asyncio.create_task(p.register())
            req = await ws.manager_recv_json()
            await ws.manager_reply(req["id"], result={"session_id": "s-1"})
            await reg
            await ws.manager_send({"jsonrpc": "2.0", "id": "c1",
                                   "method": "tool.call",
                                   "params": {"name": "testpilot_field",
                                              "arguments": {"action": "no_such"}}})
            resp = await ws.manager_recv_json()
            self.assertEqual(resp["id"], "c1")
            self.assertIn("error", resp)
            self.assertEqual(resp["error"]["code"], -32602)
            # child must have seen zero outbound calls besides list/init
            self.assertEqual(peer.seen_call_ids, [])


if __name__ == "__main__":
    unittest.main()
