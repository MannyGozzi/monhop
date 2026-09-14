# A Keychain read that needs a prompt blocks the sharing worker while the screen is locked

Verified 2026-09-13 (build aebb7c038, `sample` of the running app): the supervisor opened a
standing link, and the `monhop-sharing` thread sat in
`session_setup::prepare_endpoint -> identity::load_after_user_action -> read_record ->
SecItemCopyMatching -> securityd decrypt (mach_msg)` for minutes with no log line, because the
screen was locked (`CGSSessionScreenIsLocked` true) and the per-build Keychain prompt for the
identity record could not be shown. The worker never ends, `shutdown_ready()` stays false, and
the supervisor logs nothing until the user unlocks and answers the prompt.

Consequences: a fresh install while the Mac is locked leaves sharing off until the user is back;
quitting the app in that state waits on the same call (the install script's `pkill` fallback
covers it). The prompt itself comes from the partition list carrying cdhashes (dev signing has
no team id), so every new build asks once; see
keychain-partition-list-prompts-on-attribute-listing.md. Do not answer prompts by automation
and do not rewrite the records' ACLs; the durable fix is a Team ID signing identity.

Related: `CGGetActiveDisplayList` returns zero displays while the Mac's display sleeps, so
`local_displays_fit` fails with "Current display information could not be read." and the
supervisor retries every 11 s (10 s backoff) until the display wakes. `caffeinate -u -t 15`
wakes it for a check.
