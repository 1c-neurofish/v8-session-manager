#++agent TASK-225 [29.10.2026 18:30:00]
"""Профиль debugger существующего SDK stdio ↔ manager WS адаптера.

Словарь и secret cut выполняет только штатный downstream finalize.
"""
import asyncio
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import re
import time
import tomllib
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape as xml_escape
from urllib.parse import quote

import provider as core

SDK_ENTRY = "/workspaces/work/repos/1C Framework/1c-debug-mcp/dist/index.js"
MANAGER_CONFIG = "/home/vscode/.codex/config.toml"
POLICY_FILE = Path(__file__).with_name("debugger-policy.json")
TOOLS = ("attach", "detach", "get_targets", "set_breakpoints", "clear_breakpoints",
         "continue", "step_in", "step_out", "pause", "wait_for_stop", "get_variables", "evaluate", "raw_request", "reload_metadata", "get_performance_measurement", "get_call_stack")
ALL_TOOLS = frozenset(TOOLS)
PUBLIC = {"debug_" + name: name for name in TOOLS}
RETARGET = frozenset({"url", "infobaseAlias", "password", "autoAttach"})


def normalize(value, depth=0, budget=None):
    """Раскрывает обычные JSON/XML escapes, сохраняя actual keys для secret cut."""
    budget = [0] if budget is None else budget
    budget[0] += 1
    if depth > 24 or budget[0] > 20000:
        raise core.UnsupportedContent()
    if isinstance(value, dict):
        if not all(isinstance(k, str) for k in value):
            raise core.UnsupportedContent()
        return {k: normalize(v, depth + 1, budget) for k, v in value.items()}
    if isinstance(value, list):
        return [normalize(v, depth + 1, budget) for v in value]
    if isinstance(value, str):
        if len(value.encode("utf-8")) > 262144:
            raise core.UnsupportedContent()
        stripped = value.strip()
        if stripped.startswith(("{", "[", '"')):
            try:
                parsed = json.loads(stripped)
            except RecursionError:
                raise core.UnsupportedContent() from None
            except ValueError:
                pass
            else:
                return normalize(parsed, depth + 1, budget)
        if stripped.startswith("<"):
            if "<!DOCTYPE" in stripped.upper() or "<!ENTITY" in stripped.upper():
                raise core.UnsupportedContent()
            try:
                element = ET.fromstring(stripped)
            except (ET.ParseError, ValueError):
                pass
            else:
                def node(el, level):
                    if level > 24:
                        raise core.UnsupportedContent()
                    return {"tag": el.tag, "attributes": normalize(el.attrib, level + 1, budget),
                            "text": normalize(el.text or "", level + 1, budget),
                            "children": [node(c, level + 1) for c in el],
                            "tail": normalize(el.tail or "", level + 1, budget)}
                return node(element, depth + 1)
        return value
    if value is None or isinstance(value, (bool, int)):
        return value
    if isinstance(value, float) and math.isfinite(value):
        return value
    raise core.UnsupportedContent()


def payload(raw):
    """Не возвращает raw дубль вне единого logical result."""
    if len(json.dumps(raw, ensure_ascii=False).encode()) > 1048576:
        raise core.UnsupportedContent()
    if set(raw) - {"content", "structuredContent", "isError", "_meta"}:
        raise core.UnsupportedContent()
    result = {"content": [], "isError": bool(raw.get("isError", False))}
    for block in raw.get("content", []):
        if block.get("type") == "text":
            result["content"].append(normalize(block.get("text", "")))
        elif block.get("type") == "json":
            result["content"].append(normalize(block.get("json")))
        else:
            raise core.UnsupportedContent()
    if "structuredContent" in raw:
        result["structuredContent"] = normalize(raw["structuredContent"])
    return result


def logical_data(raw):
    mapped = payload(raw)
    data = mapped.get("structuredContent")
    if data is None:
        objects = [v for v in mapped["content"] if isinstance(v, dict)]
        if len(objects) != 1:
            raise core.AdmissionError()
        data = objects[0]
    if not isinstance(data, dict):
        raise core.AdmissionError()
    return data


def envelope(result):
    logical = {"schema_version": 1, "result": result, "field_sources": {}}
    return {"content": [{"type": "text", "text": json.dumps(logical, ensure_ascii=False)}],
            "is_error": bool(result.get("isError", False))}


def bind_raw_arguments(arguments, binding, session_id):
    """Сохраняет native XML/schema; routing поля всегда принадлежат OWN DEV UI."""
    clean = dict(arguments)
    clean["cmd"] = quote(clean["cmd"], safe="")
    clean["dbgui"] = session_id
    xml = clean["xml"]
    if not xml.strip():
        return clean, None
    if len(xml.encode()) > 262144 or "<!DOCTYPE" in xml.upper() or "<!ENTITY" in xml.upper():
        raise core.InvalidParams()
    try:
        root = ET.fromstring(xml)
    except ET.ParseError:
        raise core.InvalidParams() from None
    def local(tag):
        return tag.rsplit("}", 1)[-1]
    if local(root.tag) != "request":
        raise core.InvalidParams()
    values = {"infoBaseAlias": binding["alias"], "idOfDebuggerUI": session_id}
    for name, value in values.items():
        children = [c for c in root if local(c.tag) == name]
        if len(children) > 1:
            raise core.InvalidParams()
        pattern = r"<((?:[A-Za-z_][\w.-]*:)?" + name + r")\b[^>]*>(.*?)</\1\s*>|<((?:[A-Za-z_][\w.-]*:)?" + name + r")\b[^>]*/>"
        replacement = lambda m: "<" + (m[1] or m[3]) + ">" + xml_escape(value) + "</" + (m[1] or m[3]) + ">"
        if children:
            xml, count = re.subn(pattern, replacement, xml, count=1, flags=re.S)
            if count != 1:
                raise core.InvalidParams()
        else:
            opening = re.search(r"<((?:[A-Za-z_][\w.-]*:)?request)\b[^>]*>", xml)
            if opening is None:
                raise core.InvalidParams()
            if opening[0].endswith("/>"):
                xml = xml[:opening.end()-2] + "></" + opening[1] + ">" + xml[opening.end():]
                opening = re.search(r"<((?:[A-Za-z_][\w.-]*:)?request)\b[^>]*>", xml)
            prefix = opening[1].rsplit(":", 1)[0] + ":" if ":" in opening[1] else ""
            qualified = prefix + name
            xml = xml[:opening.end()] + "<" + qualified + ">" + xml_escape(value) + "</" + qualified + ">" + xml[opening.end():]
    verified = ET.fromstring(xml)
    if any([c.text for c in verified if local(c.tag) == name] != [value] for name, value in values.items()):
        raise core.InvalidParams()
    clean["xml"] = xml
    mode = next((c.text for c in root if local(c.tag) == "measureModeSeanceID"), None)
    return clean, mode


def validate_config(data):
    required = {"manager_ws_url", "binding_file", "descriptor_sha256", "sdk_entry_sha256", "node_command"}
    optional = {"startup_timeout_s", "call_timeout_s", "shutdown_timeout_s", "max_frame_bytes", "config_id"}
    if not isinstance(data, dict) or set(data) - required - optional or not required <= set(data):
        raise core.AdmissionError()
    reviewed = json.loads(POLICY_FILE.read_text())
    if reviewed.get("upstream_tools") != list(TOOLS) or reviewed.get("caller_retarget") is not False:
        raise core.AdmissionError()
    config = dict(data)
    for key in ("descriptor_sha256", "sdk_entry_sha256"):
        if not isinstance(config[key], str) or not re.fullmatch(r"[a-f0-9]{64}", config[key]) or config[key] == "0" * 64:
            raise core.AdmissionError()
    if hashlib.sha256(Path(SDK_ENTRY).read_bytes()).hexdigest() != config["sdk_entry_sha256"]:
        raise core.AdmissionError()
    if not Path(config["node_command"]).is_absolute() or not Path(config["binding_file"]).is_absolute():
        raise core.AdmissionError()
    if not config["manager_ws_url"].startswith(("ws://127.0.0.1:", "ws://localhost:")):
        raise core.AdmissionError()
    for key in ("startup_timeout_s", "call_timeout_s", "shutdown_timeout_s"):
        value = config.get(key, 30)
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or not 0 < value <= 300:
            raise core.AdmissionError()
    if "max_frame_bytes" in config and (type(config["max_frame_bytes"]) is not int or not 1024 <= config["max_frame_bytes"] <= 1048576):
        raise core.AdmissionError()
    config.update(profile="debugger", testpilot_command=config["node_command"], testpilot_args=[SDK_ENTRY], server_version="1.0.0")
    return config


def validate_binding(binding):
    required = {"schema_version", "generation", "expires_at", "native_session_id", "infobase_name",
                "cluster_server", "ib_session_number", "endpoint", "alias", "endpoint_dev_verified", "allowed_target_ids"}
    if not isinstance(binding, dict) or set(binding) - required - {"password"} or not required <= set(binding):
        raise core.AdmissionError()
    if type(binding["schema_version"]) is not int or binding["schema_version"] != 1 or binding["endpoint_dev_verified"] is not True:
        raise core.AdmissionError()
    if binding["infobase_name"] != "gbig_pam_ai" or binding["cluster_server"] != "onec-infra":
        raise core.AdmissionError()
    if type(binding["ib_session_number"]) is not int or not 0 < binding["ib_session_number"] <= 0xffffffff:
        raise core.AdmissionError()
    if type(binding["expires_at"]) not in (float, int) or not math.isfinite(binding["expires_at"]) or not time.time() < binding["expires_at"] <= time.time() + 3600:
        raise core.AdmissionError()
    for key in ("generation", "native_session_id", "endpoint", "alias"):
        if not isinstance(binding[key], str) or not binding[key]:
            raise core.AdmissionError()
    if not binding["endpoint"].startswith("http://onec-infra:1550") or binding["endpoint"].rstrip("/") != "http://onec-infra:1550":
        raise core.AdmissionError()
    if not isinstance(binding["allowed_target_ids"], list) or not all(isinstance(t, str) and t for t in binding["allowed_target_ids"]) or len(set(binding["allowed_target_ids"])) != len(binding["allowed_target_ids"]):
        raise core.AdmissionError()
    if "password" in binding and not isinstance(binding["password"], str):
        raise core.AdmissionError()
    return binding


class DebuggerProvider(core.Provider):
    kind = "debugger"

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.attached = False
        self.attach_attempted = False
        self.current_targets = set()
        self.stopped = set()
        self.breakpoints = False
        self.restore_running = set()
        self.target_states = {}
        self.debug_session_id = None
        self.measurement_active = False

    async def _fetch_tools(self):
        if self._tools is not None:
            return self._tools
        if self._server_version is None:
            init = await self._session.initialize()
            self._server_version = init.serverInfo.version
        descriptors = []
        cursor = None
        while True:
            page = await self._session.list_tools(cursor=cursor)
            if len(descriptors) > 100:
                raise core.AdmissionError()
            descriptors.extend(t.model_dump(mode="json", by_alias=True, exclude_none=True) for t in page.tools)
            cursor = getattr(page, "nextCursor", None)
            if not cursor:
                break
        if self._server_version != "1.0.0" or core.descriptor_digest(descriptors) != self._config["descriptor_sha256"] or {d["name"] for d in descriptors} != ALL_TOOLS:
            raise core.AdmissionError()
        self.schemas = {d["name"]: d["inputSchema"] for d in descriptors}
        self._tools = []
        for d in descriptors:
            if d["name"] not in TOOLS:
                continue
            schema = dict(d["inputSchema"])
            if d["name"] == "attach":
                schema = {"type": "object", "properties": {}, "additionalProperties": False}
            self._tools.append({"name": "debug_" + d["name"], "description": d.get("description", ""), "input_schema": schema})
        return self._tools

    async def invoke_private(self, name, args):
        return await asyncio.wait_for(self._session.call_tool(name, args), self._call_timeout)

    async def refresh_targets(self):
        raw = await self.invoke_private("get_targets", {})
        data = raw.model_dump(by_alias=True, exclude_none=True)
        if data.get("isError"):
            raise core.AdmissionError()
        rows = logical_data(data).get("targets")
        if not isinstance(rows, list):
            raise core.AdmissionError()
        allowed = set(self._binding["allowed_target_ids"])
        self.target_states = {r["targetID"].get("id"): r.get("suspended") for r in rows if isinstance(r, dict) and isinstance(r.get("targetID"), dict) and r["targetID"].get("id") in allowed}
        self.current_targets = set(self.target_states)
        return rows

    async def _dispatch_call(self, ws_id, params):
        entry = core._PendingCall(ws_id)
        entry.task = asyncio.current_task()
        self._pending[ws_id] = entry
        try:
            if not self._dispatch_ready or not isinstance(params, dict) or not isinstance(params.get("arguments", {}), dict):
                raise core.InvalidParams()
            name = PUBLIC.get(params.get("name"))
            if name is None:
                raise core.InvalidParams()
            args = core.strip_routing_arguments(params.get("arguments", {}))
            if RETARGET & set(args):
                raise core.InvalidParams()
            descriptor = next(d for d in self._tools if d["name"] == params["name"])
            from jsonschema.validators import validator_for
            schema = descriptor["input_schema"]
            if set(args) - set(schema.get("properties", {})) or not validator_for(schema)(schema).is_valid(args):
                raise core.InvalidParams()
            await self._admission_check()
            if name == "attach":
                if self.attach_attempted:
                    raise core.AdmissionError()
                self.attach_attempted = True
                args = {"url": self._binding["endpoint"], "infobaseAlias": self._binding["alias"], "autoAttach": False}
                if "password" in self._binding:
                    args["password"] = self._binding["password"]
            elif not self.attached:
                raise core.AdmissionError()
            if name == "detach":
                await self.cleanup_debugger()
                self._dispatch_ready = False
                entry.terminal_sent = True
                await self._respond(ws_id, result=envelope({"status": "detached"}))
                return
            measurement_mode = None
            if name == "raw_request":
                args, measurement_mode = bind_raw_arguments(args, self._binding, self.debug_session_id)
                if args["cmd"] == "setMeasureMode" and measurement_mode:
                    self.measurement_active = True
            target = args.get("targetId")
            if target is not None:
                await self.refresh_targets()
                if target not in self.current_targets:
                    raise core.PolicyDenied()
            if name in ("get_variables", "evaluate") and target not in self.stopped:
                raise core.AdmissionError()
            if target is not None and name in ("set_breakpoints", "pause") and self.target_states.get(target) is False:
                self.restore_running.add(target)
            if name == "wait_for_stop":
                self.stopped.clear()
            await self._admission_check()
            self._capture_queue.append(entry)
            raw = await asyncio.wait_for(self._relay.invoke(name, args), self._call_timeout)
            raw = raw.model_dump(by_alias=True, exclude_none=True)
            result = payload(raw)
            success = not raw.get("isError", False)
            if name == "attach":
                if not success or "sessionId" not in logical_data(raw):
                    raise core.AdmissionError()
                self.debug_session_id = logical_data(raw)["sessionId"]
                self.attached = True
                result = {"status": "attached"}
            elif name == "get_targets":
                if not success:
                    raise core.AdmissionError()
                rows = logical_data(raw).get("targets")
                if not isinstance(rows, list):
                    raise core.AdmissionError()
                allowed = set(self._binding["allowed_target_ids"])
                # Discovery собственной DEV alias/session не расширяет control allowlist.
                self.current_targets = {r["targetID"]["id"] for r in rows if isinstance(r, dict) and isinstance(r.get("targetID"), dict) and r["targetID"].get("id") in allowed}
                result = {"targets": normalize(rows)}
            elif name == "wait_for_stop" and success:
                data = logical_data(raw)
                stopped_target = data.get("targetId")
                await self.refresh_targets()
                if stopped_target not in self.current_targets:
                    raise core.PolicyDenied()
                self.stopped = {stopped_target}
                # Не публикуем альтернативное представление с чужим stop/event.
                for candidate in result["content"] + ([result["structuredContent"]] if "structuredContent" in result else []):
                    if isinstance(candidate, dict) and candidate.get("targetId") != stopped_target:
                        raise core.PolicyDenied()
            elif success:
                if name == "raw_request" and args["cmd"] == "setMeasureMode" and not measurement_mode:
                    response = logical_data(raw)
                    if isinstance(response.get("status"), int) and 200 <= response["status"] < 300:
                        self.measurement_active = False
                if name == "set_breakpoints":
                    self.breakpoints = True
                if name == "clear_breakpoints":
                    self.breakpoints = False
                if name in ("continue", "step_in", "step_out", "pause"):
                    self.stopped.discard(target)
                if name == "detach":
                    self.attached = False
                    self._dispatch_ready = False
            entry.terminal_sent = True
            await self._respond(ws_id, result=envelope(result))
        except asyncio.CancelledError:
            self._dispatch_ready = False
        except TimeoutError:
            self._dispatch_ready = False
            entry.cancel_requested = True
            await self._send_sdk_cancel(entry)
            await self._terminal(entry, core.CallCancelled())
        except Exception as exc:
            await self._terminal(entry, exc)
        finally:
            self._pending.pop(ws_id, None)
            with contextlib.suppress(ValueError):
                self._capture_queue.remove(entry)

    async def _close_transport(self):
        # WS держится до SDK reap и отправки session.bye; закрывает внешний owner.
        self._route_ended.set()

    async def cleanup_debugger(self):
        # Detach собственной UI registration обязателен даже при отказе восстановления target.
        if not self.attach_attempted:
            return
        failure = None
        try:
            if self.measurement_active:
                off, _ = bind_raw_arguments({"cmd": "setMeasureMode", "xml": '<request xmlns="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse"></request>'}, self._binding, self.debug_session_id)
                raw = await self.invoke_private("raw_request", off)
                response = logical_data(raw.model_dump(by_alias=True, exclude_none=True))
                if raw.isError or not isinstance(response.get("status"), int) or not 200 <= response["status"] < 300:
                    raise core.TransportError()
                self.measurement_active = False
            if self.breakpoints:
                raw = await self.invoke_private("clear_breakpoints", {})
                if raw.isError:
                    raise core.TransportError()
            for target in self.restore_running:
                await self._admission_check()
                await self.refresh_targets()
                if target not in self.current_targets:
                    raise core.AdmissionError()
                if self.target_states.get(target) is not True:
                    continue
                raw = await self.invoke_private("continue", {"targetId": target})
                if raw.isError:
                    raise core.TransportError()
        except BaseException as exc:
            self._cleanup_failed = True
            failure = exc
        finally:
            try:
                raw = await asyncio.wait_for(
                    self.invoke_private("detach", {}), self._shutdown_timeout)
                if raw.isError:
                    raise core.TransportError()
                self.attach_attempted = self.attached = False
            except BaseException as detach_error:
                self._cleanup_failed = True
                if failure is not None:
                    raise failure from detach_error
                raise
        if failure is not None:
            raise failure


async def run(data, startup_receipt=None):
    config = validate_config(data)
    async with asyncio.timeout(None) as deadline:
        armed = False
        def arm():
            nonlocal armed
            if not armed:
                armed = True
                deadline.reschedule(asyncio.get_running_loop().time() + core.relay_cleanup_budget(config))
        await _run_owned(data, startup_receipt, arm)


async def _run_owned(data, startup_receipt, arm):
    from mcp.client.streamable_http import streamable_http_client
    from mcp import ClientSession
    import httpx
    config = validate_config(data)
    receipt = core.FrozenReceipt(config["binding_file"], strict=True)
    binding = validate_binding(receipt.data)
    # Штатная harness-конфигурация остаётся единственным источником manager credentials.
    with core.private_open(MANAGER_CONFIG, strict=False) as stream:
        configured = tomllib.load(stream)["mcp_servers"]
        route = configured["v8-session-manager"]
    native_env = configured.get("1c-debug", {}).get("env", {})
    config["_debugger_env"] = {k: native_env[k] for k in ("ONEC_CF_PATH", "ONEC_CFE_PATHS", "ONEC_EPF_PATHS") if isinstance(native_env.get(k), str)}
    headers = dict(route.get("http_headers", {}))
    if route.get("bearer_token_env_var"):
        token = os.environ.get(route["bearer_token_env_var"])
        if not token:
            raise core.AdmissionError()
        headers["Authorization"] = "Bearer " + token
    with core.project_lock("debugger.lock"):
        async with core._bounded_context(httpx.AsyncClient(headers=headers, timeout=config.get("startup_timeout_s", 30)), arm) as http:
            async with core._bounded_context(streamable_http_client(route["url"], http_client=http), arm) as (read, write, _):
                async with core._bounded_context(ClientSession(read, write), arm) as manager:
                    await asyncio.wait_for(manager.initialize(), config.get("startup_timeout_s", 30))
                    provider = None
                    async def admission():
                        receipt.fresh()
                        validate_binding(binding)
                        if provider is not None and sdk.invalid.is_set():
                            raise core.AdmissionError()
                        raw = await asyncio.wait_for(manager.call_tool("session_list", {}), config.get("startup_timeout_s", 30))
                        data = raw.structuredContent
                        if raw.isError or not isinstance(data, dict) or not isinstance(data.get("sessions"), list):
                            raise core.AdmissionError()
                        rows = [r for r in data["sessions"] if r.get("session_id") == binding["native_session_id"]]
                        if len(rows) != 1:
                            raise core.AdmissionError()
                        row = rows[0]
                        if row.get("infobase_name") != binding["infobase_name"] or type(row.get("ib_session_number")) is not int or row["ib_session_number"] != binding["ib_session_number"] or row.get("state") != "active" or row.get("disconnected_secs_ago") is not None:
                            raise core.AdmissionError()
                    await admission()
                    ws = None
                    provider = None
                    try:
                        async with core._bounded_context(core.sdk_lifetime(config), arm) as sdk:
                            ws = await core.WebSocketTransport.connect(config["manager_ws_url"], config.get("startup_timeout_s", 30), config.get("max_frame_bytes", 1048576))
                            provider = DebuggerProvider(config, binding, mcp_session=sdk.session, relay_port=sdk.relay, ws=ws,
                                admission_check=admission, defer_closed=True, startup_receipt=startup_receipt)
                            provider._begin_cleanup = arm
                            provider._tools, provider._server_version = sdk.tools, sdk.version
                            sdk.provider = provider
                            try:
                                await asyncio.wait_for(provider.register(), config.get("startup_timeout_s", 30))
                                await provider._route_ended.wait()
                            finally:
                                provider._dispatch_ready = False
                                await asyncio.wait_for(provider._on_transport_end(), provider._shutdown_timeout)
                                await asyncio.wait_for(provider.cleanup_debugger(), provider._shutdown_timeout)
                        if provider._shutdown_reply is not None:
                            await provider._respond(provider._shutdown_reply, result={})
                        await provider._send({"jsonrpc": "2.0", "method": "session.bye"})
                    finally:
                        if ws is not None:
                            await ws.close()
                        if provider is not None and provider._loop_task:
                            await asyncio.wait_for(provider._loop_task, provider._shutdown_timeout)
#++agent TASK-225
