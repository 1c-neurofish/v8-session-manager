"""Approved production-boundary MUST regressions; offline only, no native probes."""
import asyncio
import copy
import unittest
from _harness import (load, FIXTURE_CONFIG, FIXTURE_BINDING, TARGET_VERSION,
                      SDKMemoryPeer, FakeWS, OwnChildLifecycleSpy, real_descriptors)

class PublicOnlySession:
    """The SDK is a public port, not a mutable private transport container."""
    def __init__(self, session):
        object.__setattr__(self,'public',{n:getattr(session,n) for n in
                         ('initialize','list_tools','call_tool','send_notification')})
    def __getattr__(self,name):
        if name.startswith('_'):raise AssertionError('private SDK access forbidden')
        return object.__getattribute__(self,'public')[name]
    def __setattr__(self,name,value):raise AssertionError('SDK mutation forbidden')

class PublicSDKBoundary(unittest.IsolatedAsyncioTestCase):
    async def test_constructor_consumes_public_sdk_only(self):
        provider,policy=load('provider'),load('policy')
        async with SDKMemoryPeer() as peer:
            provider.Provider(FIXTURE_CONFIG,FIXTURE_BINDING,
                mcp_session=PublicOnlySession(peer.client_session), relay_port=peer.relay_port,ws=FakeWS(),
                policy=policy.default_policy())

class FreshAdmission(unittest.TestCase):
    def test_fresh_identity_before_effect_not_shape(self):
        provider=load('provider')
        identity={'own_pid':FIXTURE_BINDING['own_pid'],'process_birth':FIXTURE_BINDING['process_birth'],
                  'native_client_uid':FIXTURE_BINDING['native_client_uid'],
                  'testclient_host':FIXTURE_BINDING['testclient_host'],'testclient_port':FIXTURE_BINDING['testclient_port']}
        for field in ('process_birth','native_client_uid','testclient_host'):
            with self.subTest(field=field):
                changed={**identity,field:'foreign-or-stale'}
                with self.assertRaises(provider.AdmissionError):
                    provider.check_admission(FIXTURE_BINDING,pid_alive=lambda p:True,
                        port_owner=lambda h,p:FIXTURE_BINDING['own_pid'],identity_probe=lambda binding:changed)

class DescriptorAdmission(unittest.TestCase):
    def test_published_descriptor_drift_denies(self):
        provider=load('provider');descriptors=real_descriptors()
        missing=copy.deepcopy(descriptors[:-1])
        actions=copy.deepcopy(descriptors);actions[0]['inputSchema']['properties']['action']['enum'].append('invented-action')
        malformed=copy.deepcopy(descriptors);malformed[0]['inputSchema']={'type':'invented-type'}
        for desc,version in [(missing,TARGET_VERSION),(actions,TARGET_VERSION),(malformed,TARGET_VERSION),(descriptors,'8.5.1')]:
            with self.subTest(version=version):
                with self.assertRaises(provider.AdmissionError):
                    provider.validate_descriptors(desc,target_version=version)

class CleanupFailureBoundary(unittest.IsolatedAsyncioTestCase):
    async def test_failed_own_wait_cannot_report_closed(self):
        provider,policy=load('provider'),load('policy')
        class FailedWait(OwnChildLifecycleSpy):
            async def wait(self):
                self.calls.append('wait_failed')
                raise RuntimeError('synthetic own child not settled')
        child,foreign=FailedWait(),OwnChildLifecycleSpy(foreign=True)
        async with SDKMemoryPeer() as peer:
            ws=FakeWS()
            p=provider.Provider(FIXTURE_CONFIG,FIXTURE_BINDING,mcp_session=peer.client_session, relay_port=peer.relay_port,
                ws=ws,policy=policy.default_policy(),own_child=child,native_client=foreign)
            register=asyncio.create_task(p.register())
            request=await ws.manager_recv_json(timeout=1)
            await ws.manager_reply(request['id'],result={'session_id':'synthetic-own'})
            await asyncio.wait_for(register,1)
            await ws.manager_send({'jsonrpc':'2.0','id':'shutdown-failure','method':'session.shutdown','params':{}})
            with self.assertRaises(asyncio.TimeoutError):
                await asyncio.wait_for(p.wait_closed(),.1)
            self.assertIn('wait_failed',child.calls)
            self.assertIsNone(child.returncode)
            self.assertEqual(foreign.calls,[])
if __name__=='__main__':unittest.main()
