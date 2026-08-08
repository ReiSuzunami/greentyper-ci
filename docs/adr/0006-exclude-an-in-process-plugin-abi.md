# Exclude an in-process plugin ABI

GreenTyper v1 will not load third-party dynamic libraries or expose an in-process plugin ABI. Extensibility stays behind the Skill, MCP, and Provider Profile boundaries, accepting some integration limits to reduce resident memory, compatibility burden, and the runtime crash and security surface. Skills may guide workflow but never grant capabilities, while MCP resources, prompts, and results remain untrusted external data that cannot change Rules, approvals, or authority.
