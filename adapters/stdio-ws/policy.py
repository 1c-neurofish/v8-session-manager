#++agent TASK-225 [06.10.2026 00:00:00]
"""Typed config/binding validation, tool inventory and fail-closed action policy.

Offline-safe: no OS/RAS/network probes live here; admission probes are injected
by the caller (see provider.check_admission). Shape validation alone is NOT
admission proof (F4).
"""
import math
import json
import os

_ADAPTER_DIR = os.path.dirname(os.path.abspath(__file__))
_REPO_ROOT = os.path.abspath(os.path.join(_ADAPTER_DIR, "..", ".."))
#++agent TASK-225 [17.10.2026 12:25:00]
# Постоянный service не зависит от архивируемого контекста задачи.
# Прежний путь: tasks/225-masking-rules-import-export/.context/
# phase-decisions-2/TESTPILOT-methods-inventory.json; содержимое сохранено побайтно.
INVENTORY_PATH = os.path.join(_ADAPTER_DIR, "methods-inventory.json")
#++agent TASK-225


class ConfigError(Exception):
    pass


class BindingError(Exception):
    pass


# Binding receipt schema v1: exact owned fieldnames, no extras (forged
# database_key and any unknown key are rejected, fail-closed).
BINDING_KEYS = {
    "schema_version", "own_pid", "process_birth", "testclient_host",
    "testclient_port", "connection_id", "native_client_uid", "infobase_name",
    "cluster_server", "ib_session_number", "native_evidence_digest",
}
REQUIRED_BINDING_KEYS = set(BINDING_KEYS)

_CONFIG_KEYS = {
    "manager_ws_url", "testpilot_command", "testpilot_args", "binding_file",
    "action_policy_file", "startup_timeout_s", "call_timeout_s",
    "shutdown_timeout_s", "max_frame_bytes", "config_id",
    "owner_receipt_file", "manager_config_file", "descriptor_sha256", "server_version",
    "profiles_file", "profiles_sha256",
}
_REQUIRED_CONFIG_KEYS = {
    "manager_ws_url", "testpilot_command", "binding_file",
    "action_policy_file",
}
_UINT32_MAX = 0xFFFFFFFF

_inventory_cache = None


def load_inventory(path=None):
    """{tool_name: [action, ...]} from the pinned 10/153 methods inventory."""
    global _inventory_cache
    if path is None:
        if _inventory_cache is not None:
            return _inventory_cache
        path = INVENTORY_PATH
    with open(path, encoding="utf-8") as f:
        data = json.load(f)
    inv = {name: [a["action"] for a in tool["actions"]]
           for name, tool in data["tools"].items()}
    if path == INVENTORY_PATH:
        _inventory_cache = inv
    return inv


def _require_int(value, name, lo, hi):
    if not isinstance(value, int) or isinstance(value, bool):
        raise BindingError(name + " must be an integer")
    if not lo <= value <= hi:
        raise BindingError(name + " out of range")
    return value


def validate_binding(binding):
    """Fail-closed schema check of the owner receipt; raises BindingError."""
    if not isinstance(binding, dict):
        raise BindingError("binding must be an object")
    unknown = set(binding) - BINDING_KEYS
    if unknown:
        raise BindingError("unknown binding keys: " + ",".join(sorted(unknown)))
    missing = REQUIRED_BINDING_KEYS - set(binding)
    if missing:
        raise BindingError("missing binding keys: " + ",".join(sorted(missing)))
    if binding["schema_version"] != 1:
        raise BindingError("unsupported schema_version")
    _require_int(binding["own_pid"], "own_pid", 1, _UINT32_MAX)
    _require_int(binding["testclient_port"], "testclient_port", 1, 65535)
    # IBSID is a native uint32 session number, never an opaque GUID string.
    _require_int(binding["ib_session_number"], "ib_session_number",
                 1, _UINT32_MAX)
    for key in ("process_birth", "testclient_host", "connection_id",
                "native_client_uid", "infobase_name", "cluster_server",
                "native_evidence_digest"):
        if not isinstance(binding[key], str) or (key != "connection_id" and not binding[key]):
            raise BindingError(key + " must be a non-empty string")
    return binding


def validate_config(config):
    """Typed adapter config; unknown keys denied."""
    if not isinstance(config, dict):
        raise ConfigError("config must be an object")
    unknown = set(config) - _CONFIG_KEYS
    if unknown:
        raise ConfigError("unknown config keys: " + ",".join(sorted(unknown)))
    missing = _REQUIRED_CONFIG_KEYS - set(config)
    if missing:
        raise ConfigError("missing config keys: " + ",".join(sorted(missing)))
    if not str(config["manager_ws_url"]).startswith(("ws://", "wss://")):
        raise ConfigError("manager_ws_url must be ws:// or wss://")
    for key in ("testpilot_command", "binding_file", "action_policy_file"):
        if not isinstance(config[key], str) or not os.path.isabs(config[key]):
            raise ConfigError("invalid path")
    if not isinstance(config.get("testpilot_args", []), list) or not all(
            isinstance(a, str) for a in config.get("testpilot_args", [])):
        raise ConfigError("testpilot_args must be an array of strings")
    for key in ("startup_timeout_s", "call_timeout_s", "shutdown_timeout_s"):
        if key in config and not (
                isinstance(config[key], (int, float)) and not isinstance(config[key], bool) and math.isfinite(config[key]) and config[key] > 0):
            raise ConfigError(key + " must be a positive number")
    if "max_frame_bytes" in config and not (
            isinstance(config["max_frame_bytes"], int) and not isinstance(config["max_frame_bytes"], bool)
            and config["max_frame_bytes"] > 0):
        raise ConfigError("max_frame_bytes must be a positive int")
    for key in ("owner_receipt_file", "manager_config_file", "profiles_file"):
        if key in config and (not isinstance(config[key], str) or not os.path.isabs(config[key])):
            raise ConfigError("invalid path")
    return config


def load_json_file(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


class ActionPolicy:
    """Fail-closed per (tool, action) allowlist over the real inventory.

    Default = deny everything; positive branches exist only via explicit
    reviewed synthetic allow map. Unknown tool/action is always denied.
    """

    def __init__(self, allowed=None, inventory=None):
        self._inventory = inventory if inventory is not None else load_inventory()
        self._allowed = {}
        for tool, actions in (allowed or {}).items():
            self._allowed[tool] = frozenset(actions)

    def allows(self, tool, action):
        if action not in self._inventory.get(tool, ()):
            return False
        return action in self._allowed.get(tool, frozenset())

    def with_allowed(self, allowed):
        merged = {t: set(a) for t, a in self._allowed.items()}
        for tool, actions in (allowed or {}).items():
            merged.setdefault(tool, set()).update(actions)
        return ActionPolicy(merged, self._inventory)


def default_policy():
    return ActionPolicy()

#++agent TASK-225
