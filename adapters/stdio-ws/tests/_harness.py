"""Offline test harness for the Testpilot->v8sm adapter.

Two fake levels, both in-memory:
* FakeWS — manager-side JSON-RPC2 text frames (v8-session-manager peer).
* SDKMemoryPeer — REAL official mcp 1.30.0 ClientSession over public memory
  streams with a raw JSON-RPC peer on the other end. It observes the actual
  SDK-serialized request ids and cancelled notifications — queue-only fakes
  would not prove SDK wire behavior (F3).

No real manager, client, credentials, helper or admin surface is ever touched.
Production modules are imported lazily; while they do not exist every test is
genuine RED (ModuleNotFoundError propagates, no skip/xfail).
"""
import asyncio
import contextlib
import importlib
import json
import os
import sys
import unittest

ADAPTER_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if ADAPTER_DIR not in sys.path:
    sys.path.insert(0, ADAPTER_DIR)

REPO_ROOT = os.path.abspath(os.path.join(ADAPTER_DIR, "..", ".."))
#**agent TASK-225 [29.10.2026 20:44:00]
# Bundled inventory устраняет зависимость тестов от GBIG tasks.
# INVENTORY_PATH = os.path.join(
#     REPO_ROOT, "tasks", "225-masking-rules-import-export", ".context",
#     "phase-decisions-2", "TESTPILOT-methods-inventory.json")
INVENTORY_PATH = os.path.join(ADAPTER_DIR, "methods-inventory.json")
#**agent TASK-225


def load(name):
    """Import a production module of the adapter; RED until it exists."""
    return importlib.import_module(name)


TARGET_VERSION = "8.3.27.2074"
EXPECTED_TOOLS = {
    "tc_session": 6, "tc_app": 9, "tc_window": 12, "tc_field": 43,
    "tc_doc": 21, "tc_table": 35, "tc_calendar": 5, "tc_form": 13,
    "tc_scenario": 6, "tc_find": 3,
}
EXPECTED_ACTION_TOTAL = 153

# Synthetic fixture binding: own_pid deliberately differs from the actual
# provider process pid (F4) — a shape-valid binding is NOT admission proof.
SYNTHETIC_OWN_PID = 424242
FIXTURE_BINDING = {
    "schema_version": 1,
    "own_pid": SYNTHETIC_OWN_PID,
    "process_birth": "2026-10-06T00:00:00+00:00",
    "testclient_host": "127.0.0.1",
    "testclient_port": 15550,
    "connection_id": "conn-own-1",
    "native_client_uid": "uid-own-1",
    "infobase_name": "gbig_pam_ai",
    "cluster_server": "onec-infra",
    "ib_session_number": 4242,
    "native_evidence_digest": "sha256:synthetic",
}

FIXTURE_CONFIG = {
    "manager_ws_url": "ws://127.0.0.1:0/manager",
    "testpilot_command": "/home/vscode/.local/share/1c-testpilot/venv/bin/1c-testpilot",
    "testpilot_args": [],
    "binding_file": "/nonexistent/binding.json",
    "action_policy_file": "/nonexistent/policy.json",
    "startup_timeout_s": 30,
    "call_timeout_s": 60,
    "shutdown_timeout_s": 10,
    "max_frame_bytes": 1048576,
}


def real_descriptors():
    """Actual published descriptors derived from the pinned 10/153 inventory."""
    with open(INVENTORY_PATH, encoding="utf-8") as f:
        inv = json.load(f)
    tools = []
    for name, tool in inv["tools"].items():
        enum = [a["action"] for a in tool["actions"]]
        required = {"action"}
        for a in tool["actions"]:
            for r in a["required_args"]:
                required.add(r)
        tools.append({
            "name": name,
            "description": "synthetic descriptor for " + name,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": enum},
                    "connection_id": {"type": "string"},
                },
                "required": sorted(required),
            },
        })
    return tools


class FakeWS:
    """In-memory JSON-RPC2 text-frame WS endpoint pair (manager side)."""

    def __init__(self):
        self.incoming = asyncio.Queue()   # frames adapter -> manager
        self.outgoing = asyncio.Queue()   # frames manager -> adapter
        self.closed = False

    async def send(self, text):
        await self.incoming.put(text)

    async def recv(self):
        item = await self.outgoing.get()
        if isinstance(item, Exception):
            raise item
        return item

    async def close(self):
        self.closed = True
        await self.outgoing.put(ConnectionError("fake ws closed"))

    async def manager_recv_json(self, timeout=5):
        text = await asyncio.wait_for(self.incoming.get(), timeout)
        return json.loads(text)

    async def manager_send(self, obj):
        await self.outgoing.put(json.dumps(obj))

    async def manager_reply(self, req_id, result=None, error=None):
        msg = {"jsonrpc": "2.0", "id": req_id}
        if error is not None:
            msg["error"] = error
        else:
            msg["result"] = result if result is not None else {}
        await self.manager_send(msg)


class SDKObserverRelay:
    """Public stream observer installed before ClientSession construction."""
    def __init__(self,write):
        self.write=write
        self.observer=None
        self.session=None
    def register_call_observer(self,callback):
        self.observer=callback
    async def send(self,item):
        from mcp.types import JSONRPCRequest
        root=getattr(item.message,"root",item.message)
        if isinstance(root,JSONRPCRequest) and root.method=="tools/call" and self.observer:
            self.observer(root.id)
        await self.write.send(item)
    async def aclose(self):
        await self.write.aclose()
    async def __aenter__(self):
        await self.write.__aenter__()
        return self
    async def __aexit__(self,*exc):
        return await self.write.__aexit__(*exc)
    async def invoke(self,name,arguments):
        return await self.session.call_tool(name,arguments)
    async def cancel(self,request_id):
        from mcp import types
        await self.session.send_notification(types.ClientNotification(types.CancelledNotification(
            params=types.CancelledNotificationParams(requestId=request_id,reason="offline cancellation"))))


class SDKMemoryPeer:
    """Raw JSON-RPC peer behind a real SDK ClientSession.

    Observes SessionMessage objects exactly as serialized by the official SDK:
    real outgoing request ids for tools/call and real notifications/cancelled
    params.requestId. tools/call responses are controllable per test.
    """

    def __init__(self, tools=None):
        self.tools = tools if tools is not None else real_descriptors()
        self.seen_call_ids = []          # actual MCP ids observed on the wire
        self.cancelled_notifications = []  # (requestId,) observed params
        self.other_notifications = []
        self.pending_call_replies = {}   # mcp_id -> asyncio.Queue for result
        self.call_handler = None         # optional async fn(arguments) -> result dict
        self._request_tasks = set()
        self.request_errors = []
        self.cancel_observed = asyncio.Event()
        self._task = None
        self._server_read = None
        self._server_write = None

    async def __aenter__(self):
        from mcp.shared.memory import create_client_server_memory_streams
        from mcp import ClientSession
        self._cm = create_client_server_memory_streams()
        self._client_stream, self._server_stream = await self._cm.__aenter__()
        self.relay_port = SDKObserverRelay(self._client_stream[1])
        self._session_cm = ClientSession(self._client_stream[0], self.relay_port)
        self.client_session = await self._session_cm.__aenter__()
        self.relay_port.session = self.client_session
        self._task = asyncio.create_task(self._pump())
        return self

    async def __aexit__(self, *exc):
        if self._task:
            self._task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await self._task
        for task in self._request_tasks:
            task.cancel()
        await asyncio.gather(*self._request_tasks, return_exceptions=True)
        self._request_tasks.clear()
        await self._session_cm.__aexit__(*exc)
        await self._cm.__aexit__(*exc)

    async def _pump(self):
        from mcp.shared.session import SessionMessage
        from mcp.types import JSONRPCRequest, JSONRPCNotification
        async for item in self._server_stream[0]:
            root = getattr(item.message, "root", item.message)
            if isinstance(root, JSONRPCRequest):
                if len(self._request_tasks) >= 64:
                    raise RuntimeError("bounded fixture request limit")
                task = asyncio.create_task(self._on_request(root))
                self._request_tasks.add(task)
                def finished(t):
                    self._request_tasks.discard(t)
                    if not t.cancelled() and t.exception():
                        self.request_errors.append(type(t.exception()).__name__)
                task.add_done_callback(finished)
            elif isinstance(root, JSONRPCNotification):
                if root.method == "notifications/cancelled":
                    self.cancelled_notifications.append(
                        dict(root.params) if root.params else {})
                    self.cancel_observed.set()
                else:
                    self.other_notifications.append(root.method)

    async def _on_request(self, req):
        from mcp.types import JSONRPCResponse, JSONRPCError, ErrorData
        rid = req.id
        if req.method == "initialize":
            result = {"protocolVersion": "2024-11-05",
                      "capabilities": {"tools": {"listChanged": True}},
                      "serverInfo": {"name": "1c-testpilot", "version": "1.30.0"}}
        elif req.method == "tools/list":
            result = {"tools": self.tools}
        elif req.method == "tools/call":
            self.seen_call_ids.append(rid)
            if self.call_handler:
                try:
                    result = await self.call_handler(req.params or {})
                except Exception as exc:  # synthetic transport-level error
                    await self._send(JSONRPCError(
                        jsonrpc="2.0", id=rid,
                        error=ErrorData(code=-32603, message="synthetic")))
                    return
            else:
                q = asyncio.Queue()
                self.pending_call_replies[rid] = q
                result = await q.get()      # test controls when/if it replies
        else:
            await self._send(JSONRPCError(
                jsonrpc="2.0", id=rid,
                error=ErrorData(code=-32601, message="unknown")))
            return
        await self._send(JSONRPCResponse(jsonrpc="2.0", id=rid,
                                         result=result))

    async def _send(self, msg):
        from mcp.shared.session import SessionMessage
        from mcp.types import JSONRPCMessage
        await self._server_stream[1].send(
            SessionMessage(JSONRPCMessage(msg)))

    def answer_call(self, mcp_id, result):
        self.pending_call_replies[mcp_id].put_nowait(result)


class OwnChildLifecycleSpy:
    """Synthetic owned stdio child: exit is established only by awaited wait()."""
    def __init__(self, foreign=False):
        self.pid = 313131
        self.process_birth = "synthetic-stdio-child-birth"
        self.returncode = None
        self.calls = []
        self.foreign = foreign
    def terminate(self):
        self.calls.append("terminate")
        if self.foreign: raise AssertionError("foreign process touched")
    def kill(self):
        self.calls.append("kill")
        if self.foreign: raise AssertionError("foreign process touched")
    async def wait(self):
        self.calls.append("wait_started")
        if self.foreign: raise AssertionError("foreign process touched")
        await asyncio.sleep(0)
        self.returncode = 0
        self.calls.append("wait_finished")
        return self.returncode
