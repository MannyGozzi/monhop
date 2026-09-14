# Listing MonHop's Keychain records must use SecKeychainSearch, not SecItemCopyMatching attributes

MonHop's records live in the login keychain under service com.manuelgozzi.monhop with a
`partition_id` ACL entry that holds `cdhash:` values because the development signing identity has
no team id. `SecItemCopyMatching` with `kSecReturnAttributes` (with or without `kSecMatchLimitAll`)
checks that list and, when the running build's code hash is missing, SecurityAgent shows
"MonHop wants to access key "MonHop" in your keychain. To allow this, enter the login keychain
password". Deny makes every later read in that process fail at once; Always Allow appends the
cdhash. Seen 2026-09-13 on builds 75263e6 and 239b919 (every session attempt ended with the
Identity setup failure until the dialog was answered).

The single-item data read (`base_query(account)` + `kSecReturnData`) and `kSecReturnRef` lookups
never prompted across 100+ builds: the classic ACL trusts the app by its designated requirement and
the partition list is extended silently on that path. So `list_peers_after_user_action` in
`crates/monhop-platform-macos/src/identity.rs` enumerates accounts with
`SecKeychainSearchCreateFromAttributes` + `SecKeychainItemCopyAttributesAndData` (no data pointer)
and then reads each peer record through the single-item path (commit 421f045).

Inspect ACLs without prompting: `security dump-keychain -a` (attributes and ACLs only).
