# Share MCP connections with isolated capability views

Agents using the same MCP server configuration and credential identity may share one lazily managed transport or server process to reduce resident cost. Each Agent still receives an independent Capability Snapshot and non-expanding Delegation boundary, so connection reuse never implies shared authorization or automatic access to the full server catalog.
