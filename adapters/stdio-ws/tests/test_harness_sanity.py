"""Offline fixture sanity; production imports intentionally absent here."""
import asyncio
import unittest
from _harness import SDKMemoryPeer, OwnChildLifecycleSpy

class FixtureSanity(unittest.IsolatedAsyncioTestCase):
    async def test_real_sdk_cancel_observed_before_reply(self):
        from mcp import types
        async with SDKMemoryPeer() as peer:
            await peer.client_session.initialize()
            await peer.client_session.list_tools()
            observed=[]
            peer.relay_port.register_call_observer(observed.append)
            call=asyncio.create_task(peer.client_session.call_tool('tc_find',{'action':'find_object'}))
            for _ in range(100):
                if peer.seen_call_ids:break
                await asyncio.sleep(.01)
            self.assertTrue(peer.seen_call_ids)
            rid=peer.seen_call_ids[0]
            self.assertEqual(observed,[rid])
            await peer.client_session.send_notification(types.ClientNotification(types.CancelledNotification(
                params=types.CancelledNotificationParams(requestId=rid,reason='offline sanity'))))
            await asyncio.wait_for(peer.cancel_observed.wait(),1)
            self.assertFalse(call.done())
            self.assertIn(rid,peer.pending_call_replies)
            self.assertEqual(peer.cancelled_notifications[0]['requestId'],rid)
            peer.answer_call(rid,{'content':[]})
            await asyncio.wait_for(call,1)
        self.assertFalse(peer._request_tasks)
        self.assertEqual(peer.request_errors,[])
    async def test_owned_wait_and_foreign_not_touched(self):
        child,foreign=OwnChildLifecycleSpy(),OwnChildLifecycleSpy(foreign=True)
        child.terminate();self.assertIsNone(child.returncode)
        self.assertEqual(await child.wait(),0)
        self.assertEqual(child.calls,['terminate','wait_started','wait_finished'])
        self.assertEqual(foreign.calls,[])
if __name__=='__main__':unittest.main()
