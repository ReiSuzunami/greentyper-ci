# Pin Skill Invocations by content identity

Every Skill Invocation will freeze its source identity, version, and content hash, and a resumed invocation must rehydrate the same reviewed content or stop for explicit migration. Skills load progressively and their scripts use the ordinary Tool Runtime, accepting blocked recovery after a changed Skill to prevent silent workflow drift or capability escalation.
