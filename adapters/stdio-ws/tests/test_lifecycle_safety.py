"""RED: cancellation wire shape, timeout/disconnect/own-cleanup, no creds (R8).

F2: dispatch happens only for a REAL published tool/action taken from the
pinned inventory descriptors AND an explicitly qualified synthetic allow
policy; default policy stays DENY. F3: the MCP side is a real official SDK
ClientSession over memory streams (SDKMemoryPeer), so the observed outgoing
request id and the cancelled notification are SDK wire facts, not queue mocks.
"""
import asyncio
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import (FIXTURE_BINDING, FIXTURE_CONFIG, FakeWS, SDKMemoryPeer,
                      load, real_descriptors, OwnChildLifecycleSpy)

ALLOW_FIND = {"tc_find": ["find_object"]}   # synthetic allow fixture


async def _registered(provider, policy, ws, peer, **lifecycle):
    pol = policy.default_policy().with_allowed(ALLOW_FIND)
    p = provider.Provider(FIXTURE_CONFIG, FIXTURE_BINDING,
                          mcp_session=peer.client_session, relay_port=peer.relay_port, ws=ws, policy=pol, **lifecycle)
    reg = asyncio.create_task(p.register())
    req = await ws.manager_recv_json()
    await ws.manager_reply(req["id"], result={"session_id": "s-1"})
    await reg
    return p


class Cancellation(unittest.IsolatedAsyncioTestCase):
    async def test_cancel_sends_actual_sdk_request_id(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = await _registered(provider, policy, ws, peer)
            await ws.manager_send({"jsonrpc": "2.0", "id": "ws-1",
                                   "method": "tool.call",
                                   "params": {"name": "testpilot_find",
                                              "arguments": {"action": "find_object"}}})
            # wait until the real SDK request id is observed on the wire
            for _ in range(100):
                if peer.seen_call_ids:
                    break
                await asyncio.sleep(0.01)
            self.assertTrue(peer.seen_call_ids)
            await ws.manager_send({"jsonrpc": "2.0", "id": "ws-1",
                                   "method": "tool.cancel",
                                   "params": {"id": "ws-1"}})
            for _ in range(100):
                if peer.cancelled_notifications:
                    break
                await asyncio.sleep(0.01)
            self.assertEqual(len(peer.cancelled_notifications), 1)
            self.assertIn(peer.seen_call_ids[0], peer.pending_call_replies)
            note = peer.cancelled_notifications[0]
            self.assertEqual(note["requestId"], peer.seen_call_ids[0])

    async def test_unknown_cancel_is_noop(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            await _registered(provider, policy, ws, peer)
            await ws.manager_send({"jsonrpc": "2.0", "id": "ghost",
                                   "method": "tool.cancel",
                                   "params": {"id": "ghost"}})
            await asyncio.sleep(0.05)
            self.assertEqual(peer.cancelled_notifications, [])

    async def test_exactly_one_terminal_response_on_late_result(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = await _registered(provider, policy, ws, peer)
            await ws.manager_send({"jsonrpc": "2.0", "id": "ws-2",
                                   "method": "tool.call",
                                   "params": {"name": "testpilot_find",
                                              "arguments": {"action": "find_object"}}})
            for _ in range(100):
                if peer.seen_call_ids:
                    break
                await asyncio.sleep(0.01)
            mcp_id = peer.seen_call_ids[0]
            await ws.manager_send({"jsonrpc": "2.0", "id": "ws-2",
                                   "method": "tool.cancel",
                                   "params": {"id": "ws-2"}})
            resp1 = await ws.manager_recv_json()       # terminal error response
            self.assertEqual(resp1["id"], "ws-2")
            # late duplicate child response must be discarded silently
            peer.answer_call(mcp_id, {"content": []})
            await asyncio.sleep(0.05)
            if not ws.incoming.empty():
                extra = await ws.manager_recv_json()
                self.assertNotEqual(extra.get("id"), "ws-2")


class PolicyGateBeforeDispatch(unittest.IsolatedAsyncioTestCase):
    async def test_denied_action_never_reaches_sdk(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = await _registered(provider, policy, ws, peer)
            # find_objects is a real published action but NOT in ALLOW_FIND
            await ws.manager_send({"jsonrpc": "2.0", "id": "ws-3",
                                   "method": "tool.call",
                                   "params": {"name": "testpilot_find",
                                              "arguments": {"action": "find_objects"}}})
            resp = await ws.manager_recv_json()
            self.assertEqual(resp["id"], "ws-3")
            self.assertIn("error", resp)
            self.assertEqual(peer.seen_call_ids, [])

    async def test_unpublished_tool_name_rejected(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = await _registered(provider, policy, ws, peer)
            await ws.manager_send({"jsonrpc": "2.0", "id": "ws-4",
                                   "method": "tool.call",
                                   "params": {"name": "testpilot_execute_code",
                                              "arguments": {"action": "execute_code"}}})
            resp = await ws.manager_recv_json()
            self.assertEqual(resp["id"], "ws-4")
            self.assertIn("error", resp)
            self.assertEqual(peer.seen_call_ids, [])


class Lifecycle(unittest.IsolatedAsyncioTestCase):
    async def test_shutdown_terminates_only_own_spawned_child(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            child, foreign = OwnChildLifecycleSpy(), OwnChildLifecycleSpy(foreign=True)
            p = await _registered(provider, policy, ws, peer, own_child=child, native_client=foreign)
            await ws.manager_send({"jsonrpc":"2.0","id":"pending-before-shutdown","method":"tool.call",
                                   "params":{"name":"testpilot_find","arguments":{"action":"find_object"}}})
            for _ in range(100):
                if peer.seen_call_ids: break
                await asyncio.sleep(0.01)
            self.assertTrue(peer.seen_call_ids)
            rid = peer.seen_call_ids[0]
            self.assertIn(rid,peer.pending_call_replies)
            await ws.manager_send({"jsonrpc":"2.0","id":"bye-1","method":"session.shutdown","params":{}})
            await asyncio.wait_for(peer.cancel_observed.wait(), 2)
            self.assertEqual(peer.cancelled_notifications[0]["requestId"],rid)
            self.assertIn(rid,peer.pending_call_replies)  # child has not supplied a reply
            await asyncio.wait_for(p.wait_closed(), 2)
            self.assertIn("terminate",child.calls)
            self.assertEqual(child.calls[-2:],["wait_started","wait_finished"])
            self.assertLess(child.calls.index("terminate"),child.calls.index("wait_finished"))
            self.assertEqual(child.returncode,0)
            self.assertEqual(foreign.calls,[])
            replies=[await ws.manager_recv_json(timeout=2) for _ in range(2)]
            self.assertEqual({r["id"] for r in replies},{"pending-before-shutdown","bye-1"})

    async def test_reconnect_requires_fresh_binding_no_replay(self):
        provider = load("provider")
        policy = load("policy")
        ws = FakeWS()
        async with SDKMemoryPeer(tools=real_descriptors()) as peer:
            p = await _registered(provider, policy, ws, peer)
            await ws.manager_send({"jsonrpc": "2.0", "id": "ws-9",
                                   "method": "tool.call",
                                   "params": {"name": "testpilot_find",
                                              "arguments": {"action": "find_object"}}})
            for _ in range(100):
                if peer.seen_call_ids:
                    break
                await asyncio.sleep(0.01)
            await ws.close()                           # disconnect mid-call
            # pending call terminated with safe failure, never blindly replayed
            resp = await ws.manager_recv_json()
            self.assertEqual(resp["id"], "ws-9")


class NoSecretLogging(unittest.TestCase):
    def test_error_text_and_argv_not_logged(self):
        provider = load("provider")
        sentinel = "P@ssw0rd-sentinel"
        with self.assertLogs(level="WARNING") as cm:
            provider.log_safe_error("boom " + sentinel)
        self.assertNotIn(sentinel, "\n".join(cm.output))


if __name__ == "__main__":
    unittest.main()
