# /// script
# requires-python = ">=3.13,<3.14"
# dependencies = ["mcp==2.3.0"]
# ///
"""The Python MCP SDK client against the Rust server: connect in each mode, list, call, read.

argv: base url, token. Prints one JSON object per mode on stdout.
"""

from __future__ import annotations

import asyncio
import json
import sys

import httpx2
from mcp.client.client import Client
from mcp.client.streamable_http import streamable_http_client


async def one(url: str, token: str, mode: str) -> dict:
    hdrs = {"Authorization": f"Bearer {token}"}
    async with (
        httpx2.AsyncClient(base_url=url, headers=hdrs) as hc,
        Client(streamable_http_client(f"{url}/mcp", http_client=hc), mode=mode) as c,
    ):
        tools = (await c.list_tools()).tools
        r = await c.call_tool("get_health_status", {"profile": "alex"})
        bad = await c.call_tool("get_health_status", {"profile": "carmen"})
        res = await c.read_resource("ownhealth://profiles")
        prompts = (await c.list_prompts()).prompts
        return {
            "mode": mode,
            "protocol_version": c.protocol_version,
            "tools": [t.name for t in tools],
            "annotations": {t.name: t.annotations.read_only_hint for t in tools},
            "is_error": r.is_error,
            "structured": r.structured_content,
            "text": r.content[0].text,
            "bad": {"is_error": bad.is_error, "code": (bad.structured_content or {}).get("code")},
            "profiles": json.loads(res.contents[0].text),
            "prompts": sorted(p.name for p in prompts),
        }


async def main() -> None:
    url, token = sys.argv[1], sys.argv[2]
    for mode in ("2026-07-28", "legacy", "auto"):
        print(json.dumps(await one(url, token, mode)), flush=True)


asyncio.run(main())
