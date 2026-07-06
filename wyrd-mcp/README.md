# wyrd-mcp

A [Model Context Protocol](https://modelcontextprotocol.io) server over
[wyrd](https://github.com/aoprisan/wyrd) recordings, so AI agents can ask
*why is this async task stuck?* against a `.wyrd` file.

Speaks MCP's stdio transport (newline-delimited JSON-RPC 2.0) and exposes
three tools, all taking a `recording` path:

- **`why_blocked`** — walk a task's park → resource → holder chain and name
  the outcome: a deadlock cycle, an active holder that hasn't released, or a
  backpressure/timer root. Auto-picks an interesting parked task if none is
  named.
- **`stats`** — recording span, task count, poll-time percentiles, longest
  parks, peak channel depths.
- **`world_state`** — every task and resource with its status, optionally at
  a timestamp.

Results carry both pretty-printed text and `structuredContent` (the same
serde structs `wyrd-core` returns).

Register it with Claude Code (this repo's `.mcp.json` already does):

```json
{
  "mcpServers": {
    "wyrd": { "type": "stdio", "command": "cargo", "args": ["run", "--quiet", "-p", "wyrd-mcp"] }
  }
}
```

Or with an installed binary: `claude mcp add wyrd -- wyrd-mcp`.

The server itself is pure stable Rust — only the *recorded* app needs
`tokio_unstable` (or none at all with
[`wyrd-shim`](https://crates.io/crates/wyrd-shim)).

License: MIT OR Apache-2.0
