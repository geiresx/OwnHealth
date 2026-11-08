"""Spike (a) client: the Python MCP SDK (mcp 2.3.0) against a streamable-HTTP server.

    uv run --with mcp==2.3.0 python server/scripts/spike_mcp_client.py http://127.0.0.1:18765/mcp
"""

import asyncio
import sys

import httpx2
from mcp.client.client import Client
from mcp.client.streamable_http import streamable_http_client


async def run(url: str, mode: str) -> None:
    async with (
        httpx2.AsyncClient(headers={"Authorization": "Bearer oh_pat_spike"}) as hc,
        Client(streamable_http_client(url, http_client=hc), mode=mode) as c,
    ):
        tools = await c.list_tools()
        res = await c.call_tool("ping", {"echo": "hola"})
        print(mode, "->", c.protocol_version, [t.name for t in tools.tools], res.structured_content, res.is_error)


async def main() -> None:
    url = sys.argv[1]
    for mode in ("2026-07-28", "legacy", "auto"):
        await run(url, mode)


asyncio.run(main())
