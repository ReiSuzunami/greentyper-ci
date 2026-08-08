# Isolate Compaction from authority and Memory

The Compactor will run without tools, MCP capabilities, Agent credentials, or permission to write Durable Memory, using a separate cache identity that cannot disturb normal Agent affinity. Under stale input or hard Context Pressure it may discard its work or stop new admission, but it may never delete the Event Ledger, guess runtime state, or convert an unverified summary into authority.
