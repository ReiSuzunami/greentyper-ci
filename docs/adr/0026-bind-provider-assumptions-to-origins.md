# Bind Provider assumptions to origins

Provider Template endpoints remain user-overridable, but changing a Provider Origin never silently carries an official credential or Price Schedule to the new authority. Custom origins require an explicit credential binding, default to unknown pricing unless the user selects or defines a schedule, use HTTPS except for loopback or an explicit insecure opt-in, and remain distinct Provider Profiles in usage statistics; GreenTyper does not invent a gateway's hidden backend identity.
