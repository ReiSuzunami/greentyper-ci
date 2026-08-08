# Run Agents as resource-budgeted state machines

Logical Agents will run as state machines inside one shared GreenTyper runtime rather than as one resident operating-system process or thread per Agent; only tool execution crosses into controlled child processes. This accepts less process-level crash isolation to minimize idle memory, while a resource scheduler limits Active Agents, checkpoints Dormant Agents, and reduces concurrency under Target Load. One event loop handles I/O and at most two lazily created workers handle CPU-bound runtime work.
