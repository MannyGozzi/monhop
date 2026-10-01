# Windows code type-checks from the Mac only outside the desktop app

`cargo clippy --target x86_64-pc-windows-msvc -p monhop-platform-windows --all-targets --locked
-- -D warnings` runs on the Mac in seconds and catches Win32 signature mistakes before a push.
`monhop-desktop` cannot be checked that way: its build script dependency `ring` compiles C for the
target and fails without MSVC, so Windows-only code in the app is first compiled by the PC build
(`scripts/pc/pc.sh build`) after a push.

So the page-turn chevron (ec94ac4) keeps its Win32 renderer in
`crates/monhop-platform-windows/src/page_turn/` rather than in the app beside
`dimming/windows.rs`. Its rasterizer is platform-neutral (`#[cfg(any(windows, test))]`), so its
tests run on the Mac too. New Windows-only code that does not need app state belongs in that crate
for the same reason.
