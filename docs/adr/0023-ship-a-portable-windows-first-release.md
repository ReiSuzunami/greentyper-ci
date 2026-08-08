# Ship a portable Windows-first release

GreenTyper v1 will ship as a signed, portable Windows x64 ZIP or executable targeting x86-64-v3, without a resident updater or automatic update service. Direct `CreateProcessW`, VT, and ConPTY are first-class runtime boundaries, while PowerShell is invoked only as an explicit shell or Skill dependency; this accepts a narrower Windows surface to minimize startup, memory, packaging, and background-service cost.
