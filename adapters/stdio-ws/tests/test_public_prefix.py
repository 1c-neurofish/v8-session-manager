#++agent TASK-225 [06.10.2026 00:00:00]
"""Публичный префикс отделён от pinned upstream descriptor и policy namespace."""
import asyncio
import copy
import unittest
from _harness import (load, real_descriptors, SDKMemoryPeer, FakeWS,
                      FIXTURE_CONFIG, FIXTURE_BINDING)


class PublicPrefix(unittest.IsolatedAsyncioTestCase):
    async def test_registration_and_dispatch_use_distinct_names(self):
        provider, policy = load('provider'), load('policy')
        upstream = real_descriptors()
        config = {**FIXTURE_CONFIG, 'descriptor_sha256': provider.descriptor_digest(upstream),
                  'server_version': '1.30.0'}
        async with SDKMemoryPeer(tools=upstream) as peer:
            seen = []
            async def answer(params):
                seen.append(params)
                return {'content': [{'type': 'text', 'text': 'safe synthetic'}]}
            peer.call_handler = answer
            ws = FakeWS()
            p = provider.Provider(config, FIXTURE_BINDING, mcp_session=peer.client_session,
                relay_port=peer.relay_port, ws=ws,
                policy=policy.default_policy().with_allowed({'tc_find': ['find_object']}))
            try:
                registration = asyncio.create_task(p.register())
                frame = await ws.manager_recv_json()
                tools = frame['params']['tools']
                expected = {'testpilot_' + d['name'][3:] for d in upstream}
                self.assertEqual({d['name'] for d in tools}, expected)
                for source, public in zip(upstream, tools):
                    self.assertEqual(public['input_schema'], source['inputSchema'])
                    self.assertEqual(public['description'], source['description'])
                self.assertEqual(provider.descriptor_digest(upstream), config['descriptor_sha256'])
                await ws.manager_reply(frame['id'], result={'session_id': 'synthetic-adapter'})
                await registration
                await ws.manager_send({'jsonrpc': '2.0', 'id': 'public-call', 'method': 'tool.call',
                    'params': {'name': 'testpilot_find', 'arguments': {'action': 'find_object'}}})
                result = await ws.manager_recv_json()
                self.assertEqual(result['result']['content'][0]['text'], 'safe synthetic')
                self.assertEqual(seen[0]['name'], 'tc_find')
                self.assertEqual(seen[0]['arguments']['connection_id'], FIXTURE_BINDING['connection_id'])
                for index, name in enumerate(('tc_find', 'tc_session', 'testpilot_invented', 'find')):
                    await ws.manager_send({'jsonrpc': '2.0', 'id': 'denied-' + str(index), 'method': 'tool.call',
                        'params': {'name': name, 'arguments': {'action': 'find_object'}}})
                    denied = await ws.manager_recv_json()
                    self.assertEqual(denied['error']['code'], -32602)
                self.assertEqual(len(seen), 1)
                self.assertFalse(policy.default_policy().allows('testpilot_find', 'find_object'))
            finally:
                await ws.close()
                if p._loop_task:
                    await p._loop_task

    async def test_upstream_drift_is_rejected_before_publication(self):
        provider = load('provider')
        for descriptors in (real_descriptors()[:-1], copy.deepcopy(real_descriptors())):
            if len(descriptors) == 10:
                descriptors[0]['name'] = 'testpilot_session'
            async with SDKMemoryPeer(tools=descriptors) as peer:
                p = provider.Provider(FIXTURE_CONFIG, FIXTURE_BINDING,
                    mcp_session=peer.client_session, relay_port=peer.relay_port, ws=FakeWS())
                with self.assertRaises(provider.AdmissionError):
                    await p._fetch_tools()
                self.assertIsNone(p._tools)

    async def test_digest_is_checked_in_upstream_namespace(self):
        provider = load('provider')
        async with SDKMemoryPeer() as peer:
            config = {**FIXTURE_CONFIG, 'descriptor_sha256': '0' * 64, 'server_version': '1.30.0'}
            p = provider.Provider(config, FIXTURE_BINDING, mcp_session=peer.client_session,
                relay_port=peer.relay_port, ws=FakeWS())
            with self.assertRaises(provider.AdmissionError):
                await p._fetch_tools()
            self.assertIsNone(p._tools)
#++agent TASK-225
