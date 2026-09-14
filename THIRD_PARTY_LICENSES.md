# Third-party dependencies

Original MonHop source is copyright Manuel Gozzi and licensed under GPL-3.0-or-later (LICENSE). Third-party code remains separately licensed. Permissive, GPL-compatible licenses are the default. The user explicitly approved a narrow MPL-2.0 exception for the Tauri setup shell on 2026-09-10: cssparser, cssparser-macros, dtoa-short, selectors and option-ext. This does not permit other copyleft packages. Exact-version cargo-deny exceptions and complete notices must accompany their admission. Do not copy code from GPL/AGPL/SSPL projects.

The core model uses the Rust standard library plus futures-util for its audited atomic wake primitive. The wire codec adds no external dependency. Native Windows bindings, audited cryptography and QUIC are introduced only where they supply necessary nontrivial platform/security functionality. Framework defaults with platform certificate lookup, runtime downloads, telemetry or HTTP clients must not be enabled without review.

Direct versions are pinned in Cargo manifests and the complete resolution is pinned in Cargo.lock. scripts/dependencies.ps1 generates license expressions, a CycloneDX SBOM and separate Windows/macOS runtime inventories. Runtime inventories exclude build dependencies and procedural macros. The complete lockfile inventory includes inactive optional and foreign-target dependencies. THIRD_PARTY_NOTICES.txt conservatively covers the union of normal/build graphs from Cargo metadata filtered to each supported target. This includes inactive optional-feature packages because metadata unifies features more broadly than the build. Inclusion in the notices does not mean a package is shipped or executed. Those notices accompany the executable archive. cargo-deny validates the active target graphs and fails on unknown or disallowed licenses. For dual-licensed dependencies, distribution uses an allowed permissive alternative.

Use the dependency report and SBOM for the exact versions in this build. Regenerate the Cargo reports after every lockfile change with `python3 scripts/dependencies.py`; `--check` verifies them without writes. This portable generator normalizes local paths, sorts the graph and omits generation timestamps. It does not infer native executable imports. The Windows PowerShell script additionally records actual release DLL imports, whose binary hash identifies that separate receipt.

Build-only tools are Rust/rustup 1.98.1, cargo-audit 0.22.2, cargo-deny 0.20.2, tauri-cli 2.11.4, Python 3.9+ and Node.js 22 or 24 for report/UI tests, plus the native C/C++ toolchain and OS SDK. Their executables and dependency trees are not bundled with MonHop. cargo-audit downloads the public RustSec advisory database during verification. Application runtime has no update or dependency-download mechanism.

The Tauri checkpoint audit has no blocking advisories, but reports six unmaintained build-chain packages and an unsoundness advisory for glib 0.18.5. glib is absent from both supported Windows/macOS normal and build graphs. No advisory is ignored in configuration. Multiple-version warnings are recorded by cargo-deny and are not license exceptions.

The two modified permissive runtime crates are preserved under `vendor/`, with upstream checksums, licenses and exact changes recorded in `vendor/README.md`. Distribution reports explicitly distinguish them from original MonHop source. The app bundle includes the original license, this notice, third-party license texts and the SBOM.

The setup app uses arboard 3.6.1 without image support for the explicit, write-only public pairing-code copy action. No clipboard reader, monitor, or synchronization is exposed. Its Windows dependency clipboard-win 5.4.1 omits its license text from the crate archive, so the exact repository-revision BSL-1.0 text is included with hash-checked provenance under `docs/dependencies/license-supplements/`.

The five unchanged MPL-covered packages use these exact source archives. Their complete license texts are included in THIRD_PARTY_NOTICES.txt:

| Package | Version | Corresponding source |
|---|---|---|
| cssparser | 0.36.0 | https://crates.io/api/v1/crates/cssparser/0.36.0/download |
| cssparser-macros | 0.6.1 | https://crates.io/api/v1/crates/cssparser-macros/0.6.1/download |
| dtoa-short | 0.3.5 | https://crates.io/api/v1/crates/dtoa-short/0.3.5/download |
| selectors | 0.36.1 | https://crates.io/api/v1/crates/selectors/0.36.1/download |
| option-ext | 0.2.0 | https://crates.io/api/v1/crates/option-ext/0.2.0/download |

## MIT terms referenced by the objc2 notices

The exact upstream objc2-family notices are preserved in THIRD_PARTY_NOTICES.txt, including their Apple SDK qualification. They link to the [MIT license](https://opensource.org/license/mit) rather than embedding its terms. The permission and warranty terms are reproduced below for offline reading. Copyright attribution remains as supplied upstream; this section does not invent a holder or year.

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

## Lucide icons (ISC)

The setup app's interface icons are Lucide 0.577.0 (https://lucide.dev), vendored as element data in `apps/monhop-desktop/ui/icons.mjs` so the app loads nothing over the network. Lucide is ISC licensed; portions derive from Feather under MIT. The generated Cargo reports do not cover this asset, so both notices are kept here.

ISC License

Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2026 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2026.

Permission to use, copy, modify, and/or distribute this software for any purpose with or without fee is hereby granted, provided that the above copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

The MIT License (MIT), for portions derived from Feather: Copyright (c) 2013-2026 Cole Bemis. The MIT terms are the ones quoted in the section above.

