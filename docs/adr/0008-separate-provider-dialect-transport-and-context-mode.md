# Separate Provider Dialect, Transport, and Context Mode

GreenTyper will resolve Provider Dialect, Transport, and Context Mode as independent axes instead of combining them in one provider implementation. Provider capabilities are frozen for each Turn, raw provider events remain diagnostic artifacts behind canonical Items, and switching Provider Profiles starts a new Provider Epoch without reusing continuation identity; this accepts adapter complexity to preserve predictable fallback, cache stability, and provider neutrality.
