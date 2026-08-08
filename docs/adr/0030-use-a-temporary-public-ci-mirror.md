# Use a temporary public CI mirror

GreenTyper will keep `ReiSuzunami/greentyper` private and authoritative while `ReiSuzunami/greentyper-ci` temporarily mirrors approved `main` commits for public hosted CI and build artifacts. The mirror accepts no independent development, secrets, releases, or user data; after feature implementation is substantially complete and the security, history, and CI gates pass, changing the canonical repository to public and deleting the mirror remain separate release actions requiring fresh, explicit approval from `ReiSuzunami`.
