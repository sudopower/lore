# lore

Ask questions of your agents' traces, your coding sessions and your codebases, and get answers with citations.

- **fast** mode: one retrieval and one model call, an answer in seconds (for standups and meetings).
- **deep** mode: an agent loop with `search`, `grep` and `read_file` tools, under a time budget.

Inputs: OpenTelemetry GenAI traces over OTLP/HTTP, git repositories, and a plain documents API.
Surfaces: a `lore` CLI, an MCP server, and a small built-in web page.

Written in Rust. Status: being planned; nothing works yet.
