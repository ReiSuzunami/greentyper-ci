# Organize code as a three-package Cargo workspace

GreenTyper will begin with `greentyper-core` as its only library package, `greentyper` as the product executable, and `greentyper-acceptance` as the independently delivered Target Machine runner. Architecture modules remain internal to the deep core or product implementation until a real compile, safety, or delivery seam justifies another package; this keeps dependency direction toward canonical policy, isolates acceptance code from the product, and avoids a shallow crate per module.
