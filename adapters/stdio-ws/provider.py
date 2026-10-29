#++agent TASK-225 [06.10.2026 00:00:00]
"""Testpilot externalWS adapter: official MCP SDK stdio child <-> v8sm WS.

Real production path: spawn own 1c-testpilot stdio child via SDK stdio_client,
register on the v8-session-manager WS route with a genuine owner binding
receipt (PID/birth/port admission is fail-closed), then relay tool.call /
tool.cancel / session.shutdown. Request-id correlation uses the actual
SDK-serialized outgoing tools/call id observed by wrapping the public write
stream — never a guessed private counter.
"""
import argparse
import asyncio
import base64
import collections
import contextlib
import hashlib
import json
import logging
import os
import re
import secrets
import struct
import sys
import inspect
import socket
import stat
import tomllib
from pathlib import Path
import uuid

import policy

logger = logging.getLogger("testpilot_adapter")

#++agent TASK-225 [09.10.2026]
# Только статические стадии: ошибки не раскрывают приватные входы или payload.
_STARTUP_STAGES = frozenset({"CONFIG_LOAD", "CONFIG_VALIDATE", "STATIC_ADMISSION", "RECEIPTS_LOAD", "MANAGER_INIT", "OWNER_ADMISSION", "WS_CONNECT", "CHILD_STDIO", "UPSTREAM_INIT", "DESCRIPTORS_VALIDATE", "REGISTER_SEND", "REGISTER_SENT"})
_startup_stage = "UNKNOWN"
def _mark_startup(stage):
    global _startup_stage
    _startup_stage = stage if stage in _STARTUP_STAGES else "UNKNOWN"
#--agent TASK-225


# Keys that route the call but must never reach the SDK child.
ROUTING_KEYS = frozenset(
    {"session_id", "connection_id", "database_key", "_v8_call_options"})

# Публичная поверхность manager не наследует имена SDK и не расширяет policy.
UPSTREAM_TO_PUBLIC = {
    "tc_session": "testpilot_session", "tc_app": "testpilot_app",
    "tc_window": "testpilot_window", "tc_field": "testpilot_field",
    "tc_doc": "testpilot_doc", "tc_table": "testpilot_table",
    "tc_calendar": "testpilot_calendar", "tc_form": "testpilot_form",
    "tc_scenario": "testpilot_scenario", "tc_find": "testpilot_find",
}
PUBLIC_TO_UPSTREAM = {public: upstream for upstream, public in UPSTREAM_TO_PUBLIC.items()}


_SAFE_TOKEN = re.compile(r"[a-z0-9_.-]+$")


class AdapterError(Exception):
    code = -32603
    safe_message = "internal error"


class UnknownMethod(AdapterError):
    code = -32601
    safe_message = "method not found"


class InvalidParams(AdapterError):
    code = -32602
    safe_message = "invalid params"


class PolicyDenied(AdapterError):
    code = -32602
    safe_message = "denied"


class CallCancelled(AdapterError):
    code = -32800
    safe_message = "cancelled"


class TransportError(AdapterError):
    code = -32603
    safe_message = "transport error"


class UnsupportedContent(AdapterError):
    code = -32603
    safe_message = "unsupported content block"


class AdmissionError(AdapterError):
    code = -32603
    safe_message = "admission denied"


def error_for(exc):
    """Stable JSON-RPC error object; constant safe message, no raw error.data."""
    if isinstance(exc, AdapterError):
        return {"code": exc.code, "message": exc.safe_message}
    return {"code": -32603, "message": "internal error"}


#++agent TASK-225 [19.10.2026]
def _log_session_failure(stage, action, exc, response_shape):
    # Техническая причина нужна для восстановления OWN-сеанса; значения ответа и текст ошибки не журналируются.
    try:
        frames = collections.deque(maxlen=3)
        trace = exc.__traceback__
        while trace is not None:
            code = trace.tb_frame.f_code
            frames.append({"file": Path(code.co_filename).name,
                           "function": code.co_name, "line": trace.tb_lineno})
            trace = trace.tb_next
        logger.warning("session_diagnostic %s", json.dumps({
            "stage": stage, "method": "testpilot_session", "action": action,
            "exception_class": type(exc).__name__, "frames": list(frames),
            "chain_complete": False, "response_shape": response_shape},
            ensure_ascii=True, separators=(",", ":")))
    except Exception:
        pass  # Отказ диагностики не заменяет исходный terminal/error_for.
#--agent TASK-225


def strip_routing_arguments(arguments):
    """Return a copy without source-routing keys; never mutates the input."""
    return {k: v for k, v in arguments.items() if k not in ROUTING_KEYS}


#**agent TASK-225 [13.10.2026 11:13:42]
def map_tool_result(mcp_result, own_connection=None):
    """Переводит результат MCP camelCase в формат WS snake_case.

    Представления списка подключений ограничиваются своей привязкой;
    неподдерживаемые блоки запрещаются без раскрытия их содержимого.
    """
    #**agent TASK-225
    content = []
    for block in (mcp_result or {}).get("content") or []:
        btype = block.get("type")
        if btype == "text":
            content.append({"type": "text", "text": block.get("text", "")})
        elif btype == "json":
            content.append({"type": "json", "json": block.get("json")})
        else:
            # Sanitized: only the category name, never payload bytes/base64.
            raise UnsupportedContent(str(btype or "unknown"))
    out = {"content": content,
           "is_error": bool((mcp_result or {}).get("isError", False))}
    if "structuredContent" in (mcp_result or {}):
        out["structured_content"] = mcp_result["structuredContent"]
    #++agent TASK-225 [13.10.2026 11:13:42]
    if own_connection is not None:
        # SDK возвращает общий пул; наружу допускается только текущая привязка.
        representations = []
        if "structured_content" in out:
            representations.append(("structured", None, out["structured_content"]))
        for index, block in enumerate(out["content"]):
            if block["type"] == "json":
                representations.append(("json", index, block["json"]))
            else:
                try:
                    value = json.loads(block["text"])
                except (TypeError, ValueError):
                    value = None
                representations.append(("text", index, value))

        successful = [item for item in representations
                      if isinstance(item[2], dict) and item[2].get("ok") is True]
        if not successful:
            known_error = bool(representations) and all(
                isinstance(item[2], dict)
                and item[2].get("ok") is False
                and set(item[2]) == {"ok", "code", "error"}
                for item in representations)
            if known_error and not out["is_error"]:
                return out
            raise AdmissionError()

        if out["is_error"]:
            raise AdmissionError()
        connection_id = own_connection.get("connection_id")
        host = own_connection.get("testclient_host")
        port = own_connection.get("testclient_port")
        if not isinstance(connection_id, str) or not connection_id:
            raise AdmissionError()
        for kind, index, value in representations:
            if (not isinstance(value, dict) or value.get("ok") is not True
                    or set(value) != {"ok", "connections"}
                    or not isinstance(value.get("connections"), list)):
                raise AdmissionError()
            rows = value["connections"]
            if any(not isinstance(row, dict) for row in rows):
                raise AdmissionError()
            matches = [row for row in rows
                       if row.get("connection_id") == connection_id]
            if (len(matches) != 1
                    or matches[0].get("host") != host
                    or type(matches[0].get("port")) is not int
                    or matches[0]["port"] != port):
                raise AdmissionError()
            projected = dict(value)
            projected["connections"] = [matches[0]]
            if kind == "structured":
                out["structured_content"] = projected
            elif kind == "json":
                out["content"][index]["json"] = projected
            else:
                out["content"][index]["text"] = json.dumps(
                    projected, ensure_ascii=False)
    #--agent TASK-225
    return out


def _sanitize(text):
    """Keep only plain lowercase tokens; redact everything else."""
    out = []
    for tok in str(text).split():
        out.append(tok if len(tok) <= 64 and _SAFE_TOKEN.match(tok)
                   else "<redacted>")
    return " ".join(out)


def log_safe_error(text):
    """WARNING-level log without credentials, payloads or argv echo."""
    logger.warning("adapter error: operation_failed")


def check_admission(binding, pid_alive, port_owner, identity_probe=None):
    """Fail-closed liveness/port admission; probes are injected, never real
    OS/RAS calls inside this function (shape alone is not admission, F4)."""
    policy.validate_binding(binding)
    pid = binding["own_pid"]
    if not pid_alive(pid):
        raise AdmissionError("own pid dead")
    if port_owner(binding["testclient_host"],
                  binding["testclient_port"]) != pid:
        raise AdmissionError("port owner mismatch")
    if identity_probe is not None:
        actual = identity_probe(binding)
        if not isinstance(actual, dict) or any(actual.get(k) != binding[k] for k in (
                "own_pid", "process_birth", "native_client_uid",
                "testclient_host", "testclient_port")):
            raise AdmissionError()
    return True


def validate_descriptors(descriptors, target_version):
    from jsonschema.validators import validator_for
    if target_version != "8.3.27.2074":
        raise AdmissionError()
    inventory = policy.load_inventory()
    if len(descriptors) != len(inventory) or {d.get("name") for d in descriptors} != set(inventory):
        raise AdmissionError()
    for descriptor in descriptors:
        schema = descriptor.get("inputSchema", descriptor.get("input_schema"))
        try:
            validator_for(schema).check_schema(schema)
            actions = schema["properties"]["action"]["enum"]
            if schema.get("type") != "object" or set(actions) != set(inventory[descriptor["name"]]) or len(actions) != len(set(actions)):
                raise AdmissionError()
        except Exception:
            raise AdmissionError() from None
    return descriptors


class _TapWriteStream:
    """AnyIO-compatible delegate over the SDK write stream.

    Observes the actual SDK-serialized outgoing JSONRPC request ids (the real
    wire identity) instead of reading private counters.
    """

    def __init__(self, inner, on_request=None):
        self._inner = inner
        self._on_request = on_request

    async def send(self, item):
        from mcp.types import JSONRPCRequest
        root = getattr(getattr(item, "message", item), "root", None)
        if isinstance(root, JSONRPCRequest) and root.method == "tools/call":
            if self._on_request:
                self._on_request(root.id)
        await self._inner.send(item)

    def register_call_observer(self, callback):
        self._on_request = callback

    async def invoke(self, name, arguments):
        return await self.session.call_tool(name, arguments)

    async def cancel(self, request_id):
        from mcp import types
        await self.session.send_notification(types.ClientNotification(types.CancelledNotification(
            params=types.CancelledNotificationParams(requestId=request_id))))

    # Dunder protocols are resolved on the type, so they are defined
    # explicitly instead of relying on __getattr__ delegation.
    async def __aenter__(self):
        await self._inner.__aenter__()
        return self

    async def __aexit__(self, *exc):
        return await self._inner.__aexit__(*exc)

    async def aclose(self):
        return await self._inner.aclose()

    def __getattr__(self, name):
        return getattr(self._inner, name)


class _PendingCall:
    __slots__ = ("ws_id", "task", "sdk_request_id", "cancel_requested",
                 "cancel_sent", "terminal_sent")

    def __init__(self, ws_id):
        self.ws_id = ws_id
        self.task = None
        self.sdk_request_id = None
        self.cancel_requested = False
        self.cancel_sent = False
        self.terminal_sent = False


class Provider:
    """One WS route <-> one owned stdio SDK session; bounded pending map."""

    def __init__(self, config, binding, mcp_session=None, ws=None,
                 policy_obj=None, policy=None, own_child=None,
                 native_client=None, relay_port=None, admission_check=None, defer_closed=False, startup_receipt=None):
        self._config = dict(config or {})
        self._binding = dict(binding)
        self._startup_receipt = startup_receipt
        self._receipt_sent = False
        self._session = mcp_session
        self._relay = relay_port
        self._admission_check = admission_check
        self._tasks = set()
        self._dispatch_ready = True
        self._cleanup_failed = False
        self._defer_closed = defer_closed
        self._route_ended = asyncio.Event()
        self._ws = ws
        self._policy = policy_obj or policy or _default_policy()
        self._own_child = own_child
        # Attached native client is foreign: stored for reference only, never
        # terminated by the adapter (owner cleanup contract).
        self._native_client = native_client
        self._pending = {}
        self._outbound = {}
        self._capture_queue = collections.deque()
        self._send_lock = asyncio.Lock()
        self._closed = asyncio.Event()
        self._loop_task = None
        self._ended = False
        self._shutdown_started = False
        self._shutdown_reply = None
        self._begin_cleanup = lambda: None
        self._tools = None
        self._server_version = None
        self._session_id = None
        self._client_uid = uuid.uuid4().hex
        self._call_timeout = float(self._config.get("call_timeout_s", 60))
        self._shutdown_timeout = float(
            self._config.get("shutdown_timeout_s", 10))
        self._max_frame = int(self._config.get("max_frame_bytes", 1048576))
        if self._relay is not None:
            self._relay.register_call_observer(self._on_sdk_call_id)

    def _on_sdk_call_id(self, request_id):
        try:
            entry = self._capture_queue.popleft()
        except IndexError:
            return
        entry.sdk_request_id = request_id
        if entry.cancel_requested:
            task = asyncio.create_task(self._send_sdk_cancel(entry))
            self._tasks.add(task)
            task.add_done_callback(self._tasks.discard)

    async def _send_sdk_cancel(self, entry):
        if entry.sdk_request_id is None or entry.cancel_sent:
            return
        entry.cancel_sent = True
        try:
            if self._relay is None:
                raise TransportError()
            await self._relay.cancel(entry.sdk_request_id)
        except Exception:
            log_safe_error("sdk cancel notification failed")

    # -- WS frames ----------------------------------------------------------

    async def _send(self, obj):
        async with self._send_lock:
            await self._ws.send(json.dumps(obj))

    async def _respond(self, msg_id, result=None, error=None):
        frame = {"jsonrpc": "2.0", "id": msg_id}
        if error is not None:
            frame["error"] = error
        else:
            frame["result"] = result if result is not None else {}
        await self._send(frame)

    async def _terminal(self, entry, exc):
        if entry.terminal_sent:
            return
        entry.terminal_sent = True
        await self._respond(entry.ws_id, error=error_for(exc))

    # -- registration -------------------------------------------------------

    async def _fetch_tools(self):
        if self._tools is not None:
            return self._tools
        tools = []
        full_descriptors = []
        if self._session is not None:
            _mark_startup("UPSTREAM_INIT")
            init = await self._session.initialize() if self._server_version is None else None
            if init is not None:
                info = getattr(init, "serverInfo", None)
                self._server_version = getattr(info, "version", None)
            cursor = None
            while True:
                page = await self._session.list_tools(cursor=cursor)
                for t in page.tools:
                    full_descriptors.append(t.model_dump(mode="json", by_alias=True, exclude_none=True))
                    tools.append({"name": t.name,
                                  "description": t.description,
                                  "input_schema": t.inputSchema})
                cursor = getattr(page, "nextCursor", None)
                if not cursor:
                    break
        _mark_startup("DESCRIPTORS_VALIDATE")
        validate_descriptors(tools, "8.3.27.2074")
        expected = self._config.get("descriptor_sha256")
        if expected and (descriptor_digest(full_descriptors) != expected or self._server_version != self._config.get("server_version")):
            raise AdmissionError()
        if set(UPSTREAM_TO_PUBLIC) != {descriptor["name"] for descriptor in tools}:
            raise AdmissionError()
        # Fingerprint и schema admission выше проверяют исходные upstream descriptors.
        self._tools = [{**descriptor, "name": UPSTREAM_TO_PUBLIC[descriptor["name"]]}
                       for descriptor in tools]
        return self._tools

    async def register(self):
        """Send session.register and await the manager response."""
        if self._admission_check:
            _mark_startup("OWNER_ADMISSION")
            checked = self._admission_check()
            if inspect.isawaitable(checked):
                await checked
        if self._loop_task is None:
            self._loop_task = asyncio.ensure_future(self._run())
        tools = await self._fetch_tools()
        rid = "reg-" + secrets.token_hex(8)
        fut = asyncio.get_running_loop().create_future()
        self._outbound[rid] = fut
        params = {
            "client_uid": self._client_uid,
            #**agent TASK-225 [29.10.2026 18:30:00]
            # "kind": "testpilot",
            "kind": getattr(self, "kind", "testpilot"),
            #**agent TASK-225
            "version": self._server_version,
            "infobase_name": self._binding["infobase_name"],
            "cluster_server": self._binding["cluster_server"],
            "ib_session_number": int(self._binding["ib_session_number"]),
            # pid/host_id describe THIS provider process; the native
            # TestClient identity stays inside the binding receipt (F4).
            "pid": os.getpid(),
            "tools": tools,
        }
        if self._config.get("config_id"):
            params["config_id"] = self._config["config_id"]
        _mark_startup("REGISTER_SEND")
        await self._send({"jsonrpc": "2.0", "id": rid,
                          "method": "session.register", "params": params})
        _mark_startup("REGISTER_SENT")
        if self._startup_receipt is not None and not self._receipt_sent:
            self._receipt_sent = True
            self._startup_receipt(self._client_uid)
        result = await asyncio.wait_for(fut, self._config.get("startup_timeout_s", 30))
        self._session_id = result.get("session_id")
        return result

    # -- inbound dispatch ----------------------------------------------------

    async def _run(self):
        try:
            while not self._closed.is_set():
                text = await self._ws.recv()
                if len(text.encode("utf-8", "replace")) > self._max_frame:
                    raise TransportError("oversized frame")
                try:
                    msg = json.loads(text)
                except ValueError:
                    continue  # truncated/invalid frames are dropped
                if not isinstance(msg, dict):
                    continue
                if "method" in msg:
                    await self._on_request(msg)
                elif "id" in msg:
                    self._on_response(msg)
        except Exception:
            pass
        finally:
            await self._on_transport_end()

    def _on_response(self, msg):
        fut = self._outbound.pop(msg.get("id"), None)
        if fut is None or fut.done():
            return
        if "error" in msg:
            fut.set_exception(TransportError("register rejected"))
        else:
            fut.set_result(msg.get("result") or {})

    async def _on_request(self, msg):
        method = msg.get("method")
        mid = msg.get("id")
        if method == "tool.call":
            if mid is None:
                return
            if self._pending or self._tasks or self._shutdown_started:
                await self._respond(mid, error=error_for(InvalidParams()))
                return
            task = asyncio.create_task(self._dispatch_call(mid, msg.get("params")))
            self._tasks.add(task)
            task.add_done_callback(self._tasks.discard)
        elif method == "tool.cancel":
            await self._handle_cancel(msg.get("params") or {})
        elif method == "session.shutdown":
            await self._handle_shutdown(mid)
        elif method == "ping":
            if mid is not None:
                await self._respond(mid)
        elif mid is not None:
            await self._respond(mid, error=error_for(UnknownMethod(method)))

    async def _handle_cancel(self, params):
        # Cancel carries no WS response; unknown ids are a no-op.
        entry = self._pending.get(params.get("id"))
        if entry is None:
            return
        entry.cancel_requested = True
        if entry.sdk_request_id is not None:
            await self._send_sdk_cancel(entry)
        if entry.task is not None:
            entry.task.cancel()
        await self._terminal(entry, CallCancelled("cancelled"))
        if entry.task:
            await asyncio.gather(entry.task, return_exceptions=True)

    async def _dispatch_call(self, ws_id, params):
        params = params or {}
        name = params.get("name")
        args = dict(params.get("arguments") or {})
        entry = _PendingCall(ws_id)
        self._pending[ws_id] = entry
        entry.task = asyncio.current_task()
        #++agent TASK-225 [19.10.2026]
        diagnostic_action = args.get("action") if name == "testpilot_session" else None
        diagnostic_action = diagnostic_action if diagnostic_action in ("connect", "list_connections") else None
        diagnostic_stage = "DISPATCH_ADMISSION"
        response_shape = {"sdk_invoked": False}
        #--agent TASK-225
        try:
            if not self._dispatch_ready:
                raise AdmissionError()
            diagnostic_stage = "OWNER_ADMISSION"
            if self._admission_check:
                checked = self._admission_check()
                if inspect.isawaitable(checked):
                    await checked
            diagnostic_stage = "TOOL_POLICY"
            public_name = name
            name = PUBLIC_TO_UPSTREAM.get(public_name)
            if name is None:
                raise InvalidParams("unpublished tool")
            inventory = policy.load_inventory()
            if name not in inventory:
                raise InvalidParams("unpublished tool")
            action = args.get("action")
            if action not in inventory[name]:
                raise InvalidParams("unknown action")
            if not self._policy.allows(name, action):
                raise PolicyDenied("denied")
            # Caller cannot retarget: a foreign connection_id is rejected and
            # only the actual owned connection_id is forwarded.
            diagnostic_stage = "CONNECTION_BINDING"
            supplied = args.get("connection_id")
            if supplied is not None and supplied != \
                    self._binding["connection_id"]:
                raise InvalidParams("foreign connection_id")
            clean = strip_routing_arguments(args)
            connecting = name == "tc_session" and action == "connect"
            listing_profiles = name == "tc_session" and action == "list_profiles"
            #++agent TASK-225 [13.10.2026 11:13:42]
            listing_connections = name == "tc_session" and action == "list_connections"
            #--agent TASK-225
            if connecting:
                #++agent TASK-225 [19.10.2026]
                # Публичный default применяется лишь при пропуске ключа; явный чужой/null host проверяется прежним guard.
                if "host" not in clean:
                    clean["host"] = "127.0.0.1"
                #--agent TASK-225
                if set(clean) - {"action", "host", "port", "version"} or self._binding.get("connection_id") or supplied:
                    raise InvalidParams()
                if clean.get("host") != self._binding["testclient_host"] or clean.get("port") != self._binding["testclient_port"] or clean.get("version") != "8.3.27.2074":
                    raise InvalidParams()
            elif listing_profiles:
                if set(clean) != {"action"} or supplied is not None:
                    raise InvalidParams()
            #++agent TASK-225 [13.10.2026 11:13:42]
            elif listing_connections:
                if set(clean) != {"action"} or "connection_id" in args:
                    raise InvalidParams()
                if not self._binding.get("connection_id"):
                    raise AdmissionError()
            #--agent TASK-225
            elif not self._binding.get("connection_id"):
                raise AdmissionError()
            #**agent TASK-225 [13.10.2026 11:13:42]
            else:
                clean["connection_id"] = self._binding["connection_id"]
            #**agent TASK-225
            diagnostic_stage = "ARGUMENT_SCHEMA"
            from jsonschema.validators import validator_for
            descriptor = next(d for d in self._tools if d["name"] == public_name)
            schema = descriptor["input_schema"]
            if not validator_for(schema)(schema).is_valid(clean):
                raise InvalidParams()
            self._capture_queue.append(entry)
            diagnostic_stage = "SDK_INVOKE"
            response_shape["sdk_invoked"] = True
            raw = await asyncio.wait_for(
                self._relay.invoke(name, clean), self._call_timeout)
            #**agent TASK-225 [13.10.2026 11:13:42]
            diagnostic_stage = "SDK_RESULT_MAP"
            raw_payload = raw.model_dump(by_alias=True, exclude_none=True)
            response_shape["sdk_is_error"] = bool(raw_payload.get("isError", False))
            response_shape["structured_content_present"] = "structuredContent" in raw_payload
            mapped = map_tool_result(
                raw_payload,
                self._binding if listing_connections else None)
            #**agent TASK-225
            if connecting:
                diagnostic_stage = "CONNECT_RESPONSE"
                response = mapped.get("structured_content")
                if response is None:
                    for block in mapped["content"]:
                        if block["type"] == "text":
                            try:
                                response = json.loads(block["text"])
                            except ValueError:
                                continue
                response_shape["response_is_dict"] = isinstance(response, dict)
                if isinstance(response, dict):
                    known_keys = {"ok", "connection_id", "message", "code", "error", "host", "port", "profile"}
                    response_shape["known_keys"] = sorted(k for k in response if k in known_keys)
                    response_shape["unknown_key_count"] = sum(k not in known_keys for k in response)
                    response_shape["ok_type"] = type(response.get("ok")).__name__
                    response_shape["ok_true"] = response.get("ok") is True
                    response_shape["cid_present"] = "connection_id" in response
                    response_shape["cid_string"] = isinstance(response.get("connection_id"), str)
                    response_shape["cid_nonempty"] = isinstance(response.get("connection_id"), str) and bool(response["connection_id"])
                if mapped["is_error"] or not isinstance(response, dict) or response.get("ok") is not True or not isinstance(response.get("connection_id"), str) or not response["connection_id"]:
                    raise AdmissionError()
                self._binding["connection_id"] = response["connection_id"]
            if name == "tc_session" and action == "disconnect":
                self._binding["connection_id"] = ""
            diagnostic_stage = "PUBLIC_RESPONSE"
            if not entry.terminal_sent:
                entry.terminal_sent = True
                await self._respond(ws_id, result=mapped)
        except asyncio.CancelledError:
            pass  # terminal response already emitted by the cancel path
        except asyncio.TimeoutError:
            entry.cancel_requested = True
            await self._send_sdk_cancel(entry)
            await self._terminal(entry, CallCancelled("timeout"))
        except Exception as exc:
            #++agent TASK-225 [19.10.2026]
            if diagnostic_action is not None:
                _log_session_failure(diagnostic_stage, diagnostic_action, exc, response_shape)
            #--agent TASK-225
            await self._terminal(entry, exc)
        finally:
            self._pending.pop(ws_id, None)
            with contextlib.suppress(ValueError):
                self._capture_queue.remove(entry)

    # -- shutdown / lifecycle -----------------------------------------------

    async def _handle_shutdown(self, mid):
        if self._shutdown_started:
            return
        self._shutdown_started = True
        self._begin_cleanup()
        for entry in list(self._pending.values()):
            entry.cancel_requested = True
            await self._send_sdk_cancel(entry)
            if entry.task is not None:
                entry.task.cancel()
            await self._terminal(entry, CallCancelled("shutdown"))
        try:
            await asyncio.wait_for(asyncio.shield(asyncio.gather(*self._tasks, return_exceptions=True)), self._shutdown_timeout)
        except (TimeoutError, asyncio.CancelledError):
            self._cleanup_failed = True
            raise
        self._pending.clear()
        await self._cleanup_own_child()
        if self._defer_closed:
            self._shutdown_reply = mid
            self._route_ended.set()
            return
        if mid is not None:
            await self._respond(mid, error=error_for(TransportError()) if self._cleanup_failed else None)
        if not self._cleanup_failed:
            await self._send({"jsonrpc": "2.0", "method": "session.bye"})
        await self._close_transport()

    async def _on_transport_end(self):
        """Disconnect: terminate pending calls with a safe failure, never
        blindly replay; clean up only our own spawned child."""
        if self._ended:
            return
        self._begin_cleanup()
        self._ended = True
        for entry in list(self._pending.values()):
            entry.cancel_requested = True
            if entry.task is not None:
                entry.task.cancel()
            with contextlib.suppress(Exception):
                await self._terminal(entry, TransportError("disconnect"))
        try:
            await asyncio.wait_for(asyncio.shield(asyncio.gather(*self._tasks, return_exceptions=True)), self._shutdown_timeout)
        except (TimeoutError, asyncio.CancelledError):
            self._cleanup_failed = True
            raise
        self._pending.clear()
        await self._cleanup_own_child()
        await self._close_transport()

    async def _cleanup_own_child(self):
        """Terminate ONLY the spawned stdio child and await its exit; an
        attached foreign native client is never touched."""
        child = self._own_child
        if child is None:
            return
        with contextlib.suppress(ProcessLookupError, AssertionError):
            child.terminate()
        try:
            await asyncio.wait_for(child.wait(), self._shutdown_timeout)
        except asyncio.TimeoutError:
            with contextlib.suppress(ProcessLookupError, AssertionError):
                child.kill()
            try:
                await asyncio.wait_for(child.wait(), self._shutdown_timeout)
            except Exception:
                self._cleanup_failed = True
        except Exception:
            self._cleanup_failed = True
        if not self._cleanup_failed:
            self._own_child = None

    async def _close_transport(self):
        with contextlib.suppress(Exception):
            await self._ws.close()
        if not self._cleanup_failed:
            self._route_ended.set()
            if not self._defer_closed:
                self._closed.set()

    async def wait_closed(self):
        await self._closed.wait()


def _default_policy():
    return policy.default_policy()


# -- real OS probes (production admission only; tests inject fakes) ----------

def _pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _linux_port_owner(host, port):
    """Return the pid owning the listening socket host:port, else None.

    Fail-closed: any lookup error yields None so admission is denied.
    """
    try:
        want = "%04X" % int(port)
        inodes = set()
        for table in ("/proc/net/tcp", "/proc/net/tcp6"):
            try:
                with open(table, encoding="ascii") as f:
                    next(f)
                    for line in f:
                        parts = line.split()
                        if len(parts) > 9 and parts[3] == "0A" and \
                                parts[1].rsplit(":", 1)[1] == want:
                            inodes.add(parts[9])
            except OSError:
                continue
        if not inodes:
            return None
        for pid in filter(str.isdigit, os.listdir("/proc")):
            fd_dir = os.path.join("/proc", pid, "fd")
            try:
                for fd in os.listdir(fd_dir):
                    try:
                        if os.readlink(os.path.join(fd_dir, fd)).startswith(
                                "socket:[") and os.readlink(
                                os.path.join(fd_dir, fd))[8:-1] in inodes:
                            return int(pid)
                    except OSError:
                        continue
            except OSError:
                continue
    except Exception:
        return None
    return None


# -- minimal RFC 6455 client transport (stdlib only) --------------------------

class WebSocketTransport:
    @classmethod
    async def connect(cls, url, timeout=30, max_size=1048576):
        from websockets.asyncio.client import connect
        return await connect(url, max_size=max_size, open_timeout=timeout,
                             close_timeout=10, ping_interval=20, ping_timeout=20)


# -- production entry ---------------------------------------------------------

def _child_env(config=None):
    """Explicit env allowlist baseline: dangerous capabilities stay OFF,
    logging/screenshots are not inherited enabled."""
    #++agent TASK-225 [29.10.2026 18:30:00]
    if (config or {}).get("profile") == "debugger":
        return {"PATH": os.environ.get("PATH", ""), **(config or {}).get("_debugger_env", {})}
    #++agent TASK-225
    env = {
        "TC1C_TRANSPORT": "stdio",
        "TC_PLATFORM_VERSION": "8.3.27.2074",
        "TC1C_CODE_EXECUTION": "false",
        "TC1C_QUERY_EXECUTION": "false",
        "TC1C_FUNCTIONS": "false",
        "TC1C_METADATA": "false",
        "TC1C_CODE_EPF": "",
        "TC1C_PROFILES_FILE": "",
        "TC1C_LOGGING": "false",
        "TC1C_LOG_AUTO_START": "false",
        "TC1C_LOG_SCREENSHOTS": "false",
        "TC1C_SCREENSHOTS": "false",
        "TC1C_RESPONSE_FORMAT": "json",
    }
    path = (config or {}).get("profiles_file")
    if path:
        expected_path = os.path.abspath("tasks/225-masking-rules-import-export/.context/phase-decisions-2/private/testpilot-canary/profiles.yaml")
        if path != expected_path or os.path.realpath(path) != path:
            raise AdmissionError()
        with open(path, "rb") as stream:
            info = os.fstat(stream.fileno())
            raw = stream.read(65537)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077 or len(raw) > 65536 or hashlib.sha256(raw).hexdigest() != config.get("profiles_sha256"):
            raise AdmissionError()
        env["TC1C_PROFILES_FILE"] = path
    env["PATH"] = os.environ.get("PATH", "")
    # ++agent TASK225 [09.10.2026 10:00:19]
    # Дочерний SDK использует тот же локальный desktop собственного клиента.
    for key in ("DISPLAY", "XAUTHORITY"):
        if key in os.environ:
            env[key] = os.environ[key]
    # ++agent TASK225
    return env


def descriptor_digest(descriptors):
    canonical = json.dumps(sorted(descriptors, key=lambda d: d["name"]),
        ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode()).hexdigest()


class FrozenReceipt:
    """Локальный owner receipt: inode/owner/digest не создают полномочий native."""
    def __init__(self, path, strict=False):
        self.path = path
        self.strict = strict
        self.data, self.identity = self.read()
    def read(self):
        with private_open(self.path, strict=self.strict) as stream:
            st = os.fstat(stream.fileno())
            if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o077 or st.st_size > 65536:
                raise AdmissionError()
            raw = stream.read(65537)
        return json.loads(raw), (st.st_dev, st.st_ino, st.st_uid, st.st_mode, hashlib.sha256(raw).hexdigest())
    def fresh(self):
        data, identity = self.read()
        if identity != self.identity:
            raise AdmissionError()
        return data


def owned_loopback_listener(host, port, inode, rows, fd_targets):
    """Wildcard bind обслуживает loopback; endpoint не меняется, inode обязан быть OWN."""
    if host != "127.0.0.1" or not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
        return False
    if not str(inode).isdigit() or int(inode) <= 0:
        return False
    accepted = {"0100007F:%04X" % port, "00000000:%04X" % port}
    owned = [row.split() for row in rows if len(row.split()) > 9 and row.split()[9] == str(inode)]
    return len(owned) == 1 and owned[0][1] in accepted and owned[0][3] == "0A" and "socket:[" + str(inode) + "]" in fd_targets


def verify_process(binding, proof):
    pid = binding["own_pid"]
    root = "/proc/" + str(pid)
    try:
        birth = Path(root + "/stat").read_text().rsplit(")", 1)[1].split()[19]
        argv = Path(root + "/cmdline").read_bytes().rstrip(b"\0").split(b"\0")
        argv = [a.decode() for a in argv]
        exe = os.readlink(root + "/exe")
        uid = os.stat(root).st_uid
        expected = proof["process"]
        if birth != binding["process_birth"] or exe != expected["exe"] or uid != expected["os_uid"] or argv != expected["argv"]:
            raise AdmissionError()
        if binding["infobase_name"] != "gbig_pam_ai" or binding["cluster_server"] != "onec-infra":
            raise AdmissionError()
        if not any(binding["native_client_uid"] in arg for arg in argv) or not any('Srvr="onec-infra";Ref="gbig_pam_ai"' in arg for arg in argv):
            raise AdmissionError()
        with open("/proc/net/tcp") as table:
            rows = list(table)
        targets = [os.readlink(f) for f in Path(root + "/fd").iterdir()]
        if not owned_loopback_listener(binding["testclient_host"], binding["testclient_port"],
                                       expected["listener_inode"], rows, targets):
            raise AdmissionError()
    except Exception:
        raise AdmissionError() from None


async def run_async(config, startup_receipt=None, shared_sdk=None):
    task = asyncio.create_task(_run_async_locked(config, startup_receipt, shared_sdk))
    try:
        return await asyncio.shield(task)
    except asyncio.CancelledError:
        task.cancel()
        try:
            await asyncio.shield(task)
        except BaseException:
            pass
        raise


#++agent TASK-225 [17.10.2026 12:05:00]
async def _run_async_locked(config, startup_receipt, shared_sdk):
    # Ручной и постоянный маршруты делят единственный SDK lifetime.
    with project_lock() if shared_sdk is None else contextlib.nullcontext():
        await _run_async_owned(config, startup_receipt, shared_sdk)
#++agent TASK-225


async def _run_async_owned(config, startup_receipt=None, shared_sdk=None):
    from mcp.client.streamable_http import streamable_http_client
    from mcp import ClientSession
    import httpx
    _mark_startup("STATIC_ADMISSION")
    if config.get("descriptor_sha256") != "14b6d37d902dc381bc5e79b9d0b8c894990e68af244f19da62596fb6fbb318bd" or config.get("server_version") != "1.30.0":
        raise AdmissionError()
    if config.get("manager_config_file") != "/home/vscode/.codex/config.toml" or config.get("testpilot_command") != "/home/vscode/.local/share/1c-testpilot/venv/bin/1c-testpilot" or config.get("testpilot_args", []):
        raise AdmissionError()
    _child_env(config)
    profile_path = config.get("profiles_file")
    profile_stat = os.stat(profile_path) if profile_path else None
    _mark_startup("RECEIPTS_LOAD")
    receipt = FrozenReceipt(config["binding_file"])
    binding = policy.validate_binding(receipt.data)
    proof = FrozenReceipt(config["owner_receipt_file"])
    # Штатные URL/headers доступны только через существующую таблицу harness.
    with open(config["manager_config_file"], "rb") as stream:
        route = tomllib.load(stream)["mcp_servers"]["v8-session-manager"]
    headers = dict(route.get("http_headers", {}))
    token_ref = route.get("bearer_token_env_var")
    if token_ref:
        token = os.environ.get(token_ref)
        if not token:
            raise AdmissionError()
        headers["Authorization"] = "Bearer " + token
    async with asyncio.timeout(None) as deadline:
        started = False
        def arm():
            nonlocal started
            if not started:
                started = True
                deadline.reschedule(asyncio.get_running_loop().time() + config.get("shutdown_timeout_s", 10))
        async with _bounded_context(httpx.AsyncClient(headers=headers, timeout=config.get("startup_timeout_s", 30)), arm) as http:
            async with _bounded_context(streamable_http_client(route["url"], http_client=http), arm) as (read, write, _):
                async with _bounded_context(ClientSession(read, write), arm) as manager:
                    _mark_startup("MANAGER_INIT")
                    await asyncio.wait_for(manager.initialize(), config.get("startup_timeout_s", 30))
                    async def admission():
                        _mark_startup("OWNER_ADMISSION")
                        if shared_sdk is not None and shared_sdk.admission_guard:
                            shared_sdk.admission_guard()
                        if profile_path:
                            _child_env(config)
                            current = os.stat(profile_path)
                            if (current.st_dev, current.st_ino, current.st_uid, current.st_mode) != (profile_stat.st_dev, profile_stat.st_ino, profile_stat.st_uid, profile_stat.st_mode):
                                raise AdmissionError()
                        receipt.fresh()
                        evidence = proof.fresh()
                        if binding["native_evidence_digest"] != "sha256:" + proof.identity[-1]:
                            raise AdmissionError()
                        verify_process(binding, evidence)
                        result = await asyncio.wait_for(manager.call_tool("session_list", {}), config.get("startup_timeout_s", 30))
                        data = result.structuredContent
                        if result.isError or not isinstance(data, dict) or not isinstance(data.get("sessions"), list):
                            raise AdmissionError()
                        for block in result.content:
                            if getattr(block, "type", None) != "text" or json.loads(block.text) != data:
                                raise AdmissionError()
                        rows = [r for r in data["sessions"] if r.get("session_id") == binding["native_client_uid"]]
                        if len(rows) != 1:
                            raise AdmissionError()
                        row = rows[0]
                        native = evidence["native"]
                        if row.get("infobase_name") != binding["infobase_name"] or row.get("ib_session_number") != binding["ib_session_number"] or isinstance(row.get("ib_session_number"), bool) or row.get("state") != "active" or row.get("disconnected_secs_ago") is not None or row.get("inflight") != 0 or row.get("kind") != native["kind"] or row.get("version") != native["version"]:
                            raise AdmissionError()
                        verify_process(binding, evidence)
                    await run_qualified(config, binding, admission, startup_receipt, shared_sdk)


@contextlib.asynccontextmanager
async def _bounded_context(context, arm):
    value = await context.__aenter__()
    try:
        yield value
    except BaseException:
        arm()
        if not await context.__aexit__(*sys.exc_info()):
            raise
    else:
        arm()
        await context.__aexit__(None, None, None)


async def run_qualified(config, binding, admission_check, startup_receipt=None, shared_sdk=None):
    # Контексты входят/выходят в единственной owned task; shield не переносит exits.
    task = asyncio.create_task(_run_qualified_owned(config, binding, admission_check, startup_receipt, shared_sdk))
    try:
        return await asyncio.shield(task)
    except asyncio.CancelledError:
        task.cancel()
        try:
            await asyncio.shield(task)
        except BaseException:
            pass
        raise


async def _run_qualified_owned(config, binding, admission_check, startup_receipt=None, shared_sdk=None):
    """Внутренний порт квалифицированного owner; агентных обходов не публикует."""
    #++agent TASK-225 [17.10.2026 12:05:00]
    if shared_sdk is not None:
        return await shared_sdk.qualified_relay(config, binding, admission_check, startup_receipt)
    #++agent TASK-225
    from mcp.client.stdio import stdio_client
    from mcp import ClientSession, StdioServerParameters
    await admission_check()
    params = StdioServerParameters(command=config["testpilot_command"],
        args=config.get("testpilot_args", []), env=_child_env(config))
    ws = None
    provider = None
    async with asyncio.timeout(None) as cleanup_deadline:
        cleanup_started = False
        def arm():
            nonlocal cleanup_started
            if not cleanup_started:
                cleanup_started = True
                cleanup_deadline.reschedule(asyncio.get_running_loop().time() + config.get("shutdown_timeout_s", 10))
        try:
            _mark_startup("WS_CONNECT")
            ws = await WebSocketTransport.connect(config["manager_ws_url"],
                config.get("startup_timeout_s", 30), config.get("max_frame_bytes", 1048576))
            _mark_startup("CHILD_STDIO")
            with open(os.devnull, "w") as sink:
                async with _bounded_context(stdio_client(params, errlog=sink), arm) as (read, write):
                    relay = _TapWriteStream(write)
                    async def changed(message):
                        from mcp import types
                        if isinstance(message, types.ServerNotification) and isinstance(message.root, types.ToolListChangedNotification) and provider:
                            provider._dispatch_ready = False
                            # Новая классификация требуется до продолжения dispatch.
                            async def refresh():
                                try:
                                    provider._tools = None
                                    tools = await provider._fetch_tools()
                                    await provider._send({"jsonrpc":"2.0", "method":"session.tools_changed", "params":{"tools":tools}})
                                except Exception:
                                    log_safe_error("descriptor_change_failed")
                            task = asyncio.create_task(refresh())
                            provider._tasks.add(task)
                            task.add_done_callback(provider._tasks.discard)
                    async with _bounded_context(ClientSession(read, relay, message_handler=changed), arm) as session:
                        relay.session = session
                        provider = Provider(config, binding, mcp_session=session, relay_port=relay,
                            ws=ws, policy=_load_action_policy(config), admission_check=admission_check, defer_closed=True, startup_receipt=startup_receipt)
                        provider._begin_cleanup = arm
                        await asyncio.wait_for(provider.register(), config.get("startup_timeout_s", 30))
                        await provider._route_ended.wait()
                        arm()
                # Публичный stdio context уже подтвердил завершение своего child.
                if provider._shutdown_reply is not None:
                    await provider._respond(provider._shutdown_reply, result={})
                    await provider._send({"jsonrpc":"2.0", "method":"session.bye"})
                provider._closed.set()
        finally:
            arm()
            if provider:
                await asyncio.wait_for(provider._on_transport_end(), provider._shutdown_timeout)
            if ws is not None:
                await ws.close()
            if provider and provider._loop_task and provider._loop_task is not asyncio.current_task():
                await provider._loop_task


def _load_action_policy(config):
    """Explicit reviewed allow file; absent/empty keeps the deny-all default."""
    path = config.get("action_policy_file")
    allow = {}
    try:
        data = policy.load_json_file(path)
        if isinstance(data, dict):
            allow = data.get("allow") or {}
    except Exception:
        pass
    if not isinstance(allow, dict) or not all(
            isinstance(v, list) for v in allow.values()):
        allow = {}
    return policy.default_policy().with_allowed(allow)


def _emit_startup_receipt(session_id):
    # SID предназначен для exact public readback; отправка запроса не доказывает регистрацию.
    print(json.dumps({"status": "registration_requested", "session_id": session_id}), flush=True)


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="testpilot-adapter",
        description="Testpilot externalWS adapter (owned internal provider)")
    parser.add_argument("--config", required=True,
                        help="path to adapter JSON config")
    #++agent TASK-225 [29.10.2026 18:30:00]
    parser.add_argument("--profile", choices=("testpilot", "debugger"), default="testpilot")
    #++agent TASK-225
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.WARNING)
    try:
        _mark_startup("CONFIG_LOAD")
        raw_config = policy.load_json_file(args.config)
        _mark_startup("CONFIG_VALIDATE")
        #++agent TASK-225 [29.10.2026 18:30:00]
        if args.profile == "debugger":
            import debugger
            asyncio.run(debugger.run(raw_config, _emit_startup_receipt))
            return 0
        #++agent TASK-225
        config = policy.validate_config(raw_config)
        asyncio.run(run_async(config, startup_receipt=_emit_startup_receipt))
        return 0
    except BaseException as exc:
        #++agent TASK-225 [09.10.2026]
        category = next((label for cls, label in ((policy.ConfigError, "CONFIG"), (policy.BindingError, "BINDING"), (AdmissionError, "ADMISSION"), (TransportError, "TRANSPORT"), (TimeoutError, "TIMEOUT")) if type(exc) is cls), "UNKNOWN")
        logger.error("adapter error: startup_failed:%s:%s", _startup_stage if _startup_stage in _STARTUP_STAGES else "UNKNOWN", category)
        #--agent TASK-225
        return 1



#++agent TASK-225

#++agent TASK-225 [17.10.2026 12:05:00]
STATE_DIR = "/home/vscode/.local/state/testpilot-adapter/gbig-pam"


def private_open(path, strict=True):
    """Открытие через directory FD исключает symlink на любом компоненте."""
    parts = Path(path).parts
    if not parts or parts[0] != "/" or ".." in parts:
        raise AdmissionError()
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY)
    try:
        for part in parts[1:-1]:
            next_fd = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            os.close(fd)
            fd = next_fd
        leaf = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW, dir_fd=fd)
    finally:
        os.close(fd)
    stream = os.fdopen(leaf, "rb")
    info = os.fstat(leaf)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or (stat.S_IMODE(info.st_mode) != 0o600 if strict else info.st_mode & 0o077):
        stream.close()
        raise AdmissionError()
    return stream


@contextlib.contextmanager
#**agent TASK-225 [29.10.2026 18:30:00]
# def project_lock():
def project_lock(lock_name="project.lock"):
    #**agent TASK-225
    import fcntl
    Path(STATE_DIR).mkdir(mode=0o700, parents=True, exist_ok=True)
    info = os.lstat(STATE_DIR)
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise AdmissionError()
    with private_open(STATE_DIR + "/control.lock") if Path(STATE_DIR + "/control.lock").exists() else contextlib.nullcontext():
        pass
    #++agent TASK-225 [29.10.2026 18:30:00]
    if lock_name not in ("project.lock", "debugger.lock"):
        raise AdmissionError()
    # fd = os.open(STATE_DIR + "/project.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    fd = os.open(STATE_DIR + "/" + lock_name, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    #++agent TASK-225
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
            raise AdmissionError()
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise TransportError("BUSY") from None
        yield
    finally:
        os.close(fd)


class SharedSDK:
    """Один initialized SDK; ровно один qualified WS relay за поколение."""
    def __init__(self, config):
        self.config = config
        self.provider = None
        self.invalid = asyncio.Event()
        self.session = None
        self.relay = None
        self.tools = None
        self.version = None
        self.dirty = False
        self.admission_guard = None

    async def changed(self, message):
        from mcp import types
        if isinstance(message, types.ServerNotification) and isinstance(message.root, types.ToolListChangedNotification):
            self.invalid.set()
            if self.provider:
                self.provider._dispatch_ready = False
                self.provider._route_ended.set()

    async def qualify(self):
        #++agent TASK-225 [29.10.2026 18:30:00]
        # probe = Provider(self.config, {}, mcp_session=self.session)
        provider_class = Provider
        if self.config.get("profile") == "debugger":
            from debugger import DebuggerProvider
            provider_class = DebuggerProvider
        probe = provider_class(self.config, {}, mcp_session=self.session)
        #++agent TASK-225
        probe._server_version = self.version
        self.tools = await probe._fetch_tools()
        self.version = probe._server_version
        self.invalid.clear()

    async def qualified_relay(self, config, binding, admission, receipt):
        if self.provider is not None or self.invalid.is_set() or self.dirty:
            raise AdmissionError()
        await admission()
        ws = await WebSocketTransport.connect(config["manager_ws_url"], config.get("startup_timeout_s", 30), config.get("max_frame_bytes", 1048576))
        route = Provider(config, binding, mcp_session=self.session, relay_port=self.relay,
            ws=ws, policy=_load_action_policy(config), admission_check=admission,
            defer_closed=True, startup_receipt=receipt)
        route._tools, route._server_version = self.tools, self.version
        self.provider = route
        try:
            await asyncio.wait_for(route.register(), config.get("startup_timeout_s", 30))
            await route._route_ended.wait()
        finally:
            route._dispatch_ready = False
            # Неизвестный результат отменённого connect нельзя передать новому owner.
            if route._pending or route._capture_queue:
                self.dirty = True
            try:
                await asyncio.wait_for(route._on_transport_end(), route._shutdown_timeout)
                if route._loop_task and route._loop_task is not asyncio.current_task():
                    await asyncio.wait_for(route._loop_task, route._shutdown_timeout)
                connection = route._binding.get("connection_id")
                if connection:
                    raw = await asyncio.wait_for(self.session.call_tool("tc_session", {"action":"disconnect", "connection_id":connection}), route._shutdown_timeout)
                    mapped = map_tool_result(raw.model_dump(by_alias=True, exclude_none=True))
                    content = mapped.get("structured_content")
                    if content is None:
                        content = next((json.loads(b["text"]) for b in mapped["content"] if b["type"] == "text"), None)
                    if mapped["is_error"] or not isinstance(content, dict) or content.get("ok") is not True:
                        raise TransportError()
                if route._cleanup_failed or route._tasks or route._pending:
                    raise TransportError()
            except BaseException:
                self.dirty = True
                raise
            finally:
                self.relay.register_call_observer(None)
                self.provider = None
                route._closed.set()
                await ws.close()


def relay_cleanup_budget(config):
    return 4 * float(config.get("shutdown_timeout_s",10)) + 2


def service_stop_budget(config):
    return (float(config.get("startup_timeout_s",30)) + relay_cleanup_budget(config)
            + float(config.get("shutdown_timeout_s",10)) + 7)


class SDKReapedError(Exception):
    def __init__(self, cause):
        self.cause = cause


def _owned_children():
    # В service нет иных дочерних процессов; snapshot не принимает PID-file за authority.
    return set(Path(f"/proc/{os.getpid()}/task/{os.getpid()}/children").read_text().split())


async def _reap_sdk(pid, birth, uid, pidfd):
    import signal
    import time
    def alive():
        try:
            root = f"/proc/{pid}"
            return os.stat(root).st_uid == uid and Path(root + "/stat").read_text().rsplit(")",1)[1].split()[19] == birth
        except FileNotFoundError:
            return False
    # context exit мог уже reap child; иначе waitpid относится только к нашему child.
    for sig in (None, signal.SIGTERM, signal.SIGKILL):
        if not alive():
            return
        try:
            got, _ = os.waitpid(pid, os.WNOHANG)
            if got == pid:
                return
        except ChildProcessError:
            if not alive():
                return
            raise TransportError("cleanup unconfirmed") from None
        if sig is not None:
            signal.pidfd_send_signal(pidfd, sig)
        until = time.monotonic() + (0.1 if sig is None else 2)
        while time.monotonic() < until:
            if not alive():
                return
            try:
                got, _ = os.waitpid(pid, os.WNOHANG)
                if got == pid:
                    return
            except ChildProcessError:
                if not alive():
                    return
                raise TransportError("cleanup unconfirmed") from None
            await asyncio.sleep(0.05)
    raise TransportError("cleanup unconfirmed")


@contextlib.asynccontextmanager
async def sdk_lifetime(config):
    # Используются публичные stdio/ClientSession; private process поля SDK не читаются.
    from mcp.client.stdio import stdio_client
    from mcp import ClientSession, StdioServerParameters
    sdk = SharedSDK(config)
    params = StdioServerParameters(command=config["testpilot_command"], args=config.get("testpilot_args", []), env=_child_env(config))
    failure = None
    owned = None
    before = _owned_children()
    try:
        async with asyncio.timeout(None) as deadline:
            armed = False
            def arm():
                nonlocal armed
                if not armed:
                    armed = True
                    deadline.reschedule(asyncio.get_running_loop().time() + config.get("shutdown_timeout_s",10))
            with open(os.devnull, "w") as sink:
                async with _bounded_context(stdio_client(params, errlog=sink), arm) as (read, write):
                    added = _owned_children() - before
                    if len(added) != 1:
                        raise TransportError("cleanup unconfirmed")
                    pid = int(added.pop())
                    proc = Path(f"/proc/{pid}")
                    argv = proc.joinpath("cmdline").read_bytes().split(b"\0")
                    if config["testpilot_command"].encode() not in argv or proc.stat().st_uid != os.getuid():
                        raise TransportError("cleanup unconfirmed")
                    owned = (pid, proc.joinpath("stat").read_text().rsplit(")",1)[1].split()[19], proc.stat().st_uid, os.pidfd_open(pid))
                    sdk.relay = _TapWriteStream(write)
                    async with _bounded_context(ClientSession(read, sdk.relay, message_handler=sdk.changed), arm) as session:
                        sdk.session = sdk.relay.session = session
                        try:
                            await asyncio.wait_for(sdk.qualify(), config.get("startup_timeout_s", 30))
                            yield sdk
                        except BaseException as exc:
                            failure = exc
    except BaseException as exc:
        failure = failure or exc
    finally:
        if owned is not None:
            try:
                await _reap_sdk(*owned)
            except BaseException as cleanup_error:
                if isinstance(failure, (asyncio.CancelledError, KeyboardInterrupt, SystemExit)):
                    raise failure from cleanup_error
                raise
            finally:
                os.close(owned[-1])
        elif _owned_children() - before:
            # Неподтверждённая identity не позволяет kill/adopt/retry.
            raise TransportError("cleanup unconfirmed")
    if isinstance(failure, (asyncio.CancelledError, KeyboardInterrupt, SystemExit)):
        raise failure
    if failure is not None:
        raise SDKReapedError(failure) from None
#++agent TASK-225

if __name__ == "__main__":
    sys.exit(main())
