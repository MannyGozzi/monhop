# Forgetting the saved peer must use SecKeychainItemDelete, not SecItemDelete

`SecItemDelete` on a legacy-keychain password item runs Apple's `_SafeSecKeychainItemDelete`
(Security/OSX/libsecurity_keychain/lib/SecItem.cpp), which compares the basename of every
trusted application in the item's decrypt ACL with the caller's basename and returns
errSecInvalidOwnerEdit (-25244) with no dialog when none match. MonHop records created before
the product rename trust `/Applications/MonHop.app`, so `/Applications/MonHop.app` can read
them (reads check the code signature) but could not delete them. Symptom seen 2026-09-12:
Forget failed with "the Keychain operation failed", Connect stuck on "Pairing needs attention".

Fix (56a7a64): `delete_confirmed_peer_after_user_action` in
`crates/monhop-platform-macos/src/identity.rs` resolves the item with `kSecReturnRef` and
calls `SecKeychainItemDelete`, which skips the basename check and deletes silently. Verified on
a throwaway record: SecItemDelete from a differently named executable gave -25244,
SecKeychainItemDelete gave 0. Do not reintroduce a tombstone/overwrite path: it keeps the stale
ACL forever and makes two concurrent pairings able to overwrite each other.

Probes: read ACL trusted-app paths with `SecKeychainItemCopyAccess` + `SecACLCopyContents` +
`SecTrustedApplicationCopyData` (no data read, no prompt). Never run SecItemAdd/Update/SetAccess
probes from an unsigned process against the real records: they hang on SecurityAgent dialogs.
