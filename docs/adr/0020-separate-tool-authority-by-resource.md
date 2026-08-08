# Separate tool authority by resource

GreenTyper will evaluate filesystem, network, and process authority independently for every tool execution, constrain child processes with Windows Job Objects, and fail closed when a required sandbox boundary cannot be enforced. Provider and explicitly authorized MCP connections may use network access, while ordinary tool processes start without it; Approval Grants remain narrow in Agent, operation, arguments, resource scope, and time rather than creating permanent blanket trust.
