"""RED: MCP<->WS codec: argument stripping, content mapping, fail-closed (R1/R4/R7)."""
import asyncio
import base64
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import FIXTURE_BINDING, FIXTURE_CONFIG, FakeWS, load


class ArgumentStripping(unittest.TestCase):
    def test_only_routing_keys_stripped(self):
        provider = load("provider")
        args = {"action": "get_text", "ref": "r1",
                "session_id": "s-9", "_v8_call_options": {"x": 1}}
        clean = provider.strip_routing_arguments(dict(args))
        self.assertNotIn("session_id", clean)
        self.assertNotIn("_v8_call_options", clean)
        self.assertEqual(clean["action"], "get_text")
        self.assertEqual(clean["ref"], "r1")
        self.assertEqual(args["session_id"], "s-9")  # input not mutated

    def test_business_keys_survive(self):
        provider = load("provider")
        args = {"action": "input_text", "ref": "r", "text": "x"}
        self.assertEqual(provider.strip_routing_arguments(args), args)


class ContentMapping(unittest.TestCase):
    def test_iserror_camel_to_snake(self):
        provider = load("provider")
        mcp_result = {"content": [{"type": "text", "text": "boom"}],
                      "isError": True}
        out = provider.map_tool_result(mcp_result)
        self.assertTrue(out["is_error"])
        self.assertEqual(out["content"][0]["type"], "text")

    def test_structured_content_mapped_verbatim(self):
        provider = load("provider")
        mcp_result = {"content": [{"type": "text", "text": "{}"}],
                      "structuredContent": {"ok": True}}
        out = provider.map_tool_result(mcp_result)
        self.assertEqual(out["structured_content"], {"ok": True})
        # adapter must not invent envelope/provenance fields
        self.assertNotIn("field_sources", out.get("structured_content", {}))
        self.assertNotIn("schema_version", out.get("structured_content", {}))

    def test_business_iserror_not_transport_success(self):
        provider = load("provider")
        out = provider.map_tool_result(
            {"content": [{"type": "text", "text": "denied"}], "isError": True})
        self.assertTrue(out["is_error"])

    def test_json_content_block_passes(self):
        provider = load("provider")
        # v8sm WS Json variant is exactly {"type":"json","json":<Value>} (F1)
        out = provider.map_tool_result(
            {"content": [{"type": "json", "json": {"a": 1}}]})
        self.assertEqual(out["content"][0]["json"], {"a": 1})
        self.assertNotIn("data", out["content"][0])


class UnsupportedContentFailClosed(unittest.TestCase):
    def test_image_audio_resource_rejected_sanitized(self):
        provider = load("provider")
        for block in ({"type": "image", "mimeType": "image/png",
                       "data": base64.b64encode(b"png").decode()},
                      {"type": "audio", "mimeType": "audio/wav", "data": "AA=="},
                      {"type": "resource", "resource": {"uri": "x"}}):
            with self.assertRaises(provider.UnsupportedContent) as cm:
                provider.map_tool_result({"content": [block]})
            # sanitized: no payload bytes/base64 leak into the error
            self.assertNotIn("png", str(cm.exception))
            self.assertNotIn("AA==", str(cm.exception))

    def test_unsupported_maps_to_minus_32603(self):
        provider = load("provider")
        err = provider.error_for(provider.UnsupportedContent("x"))
        self.assertEqual(err["code"], -32603)


class ErrorCodes(unittest.TestCase):
    def test_code_mapping(self):
        provider = load("provider")
        self.assertEqual(provider.error_for(provider.UnknownMethod("m"))["code"],
                         -32601)
        self.assertEqual(provider.error_for(provider.InvalidParams("a"))["code"],
                         -32602)
        self.assertEqual(provider.error_for(provider.CallCancelled("c"))["code"],
                         -32800)
        self.assertEqual(provider.error_for(provider.TransportError("t"))["code"],
                         -32603)


if __name__ == "__main__":
    unittest.main()
