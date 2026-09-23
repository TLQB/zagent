> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Zagent

A high-performance, multiplayer code editor built on [Zed](https://github.com/zed-industries/zed),
extended with multi-provider AI routing and a self-hosted model proxy. Zagent is a
**snapshot fork**: it does not share git history with upstream, so upstream fixes are
ported selectively instead of merged wholesale.

[![Build Linux](https://github.com/TLQB/zagent/actions/workflows/build-linux.yml/badge.svg)](https://github.com/TLQB/zagent/actions/workflows/build-linux.yml)
[![Build Windows](https://github.com/TLQB/zagent/actions/workflows/build-windows.yml/badge.svg)](https://github.com/TLQB/zagent/actions/workflows/build-windows.yml)

---

### Z.AI gateway worker (built-in provider)

Zagent ships a built-in `Zagent-GLM` language model provider that talks to a
self-hosted [Cloudflare Worker gateway](https://github.com/TLQB/zai-proxy) exposing an
OpenAI-compatible API (`/v1/models`, `/v1/chat/completions`). No local proxy, sidecar
binary or Go toolchain is involved: the editor calls the worker directly.

Signing in (Settings, AI, LLM Providers, Zagent-GLM):

- paste a pre-issued gateway **token**, or
- enter the gateway account **email + password** — the editor logs in via
  `POST /auth/login`, caches the 24h JWT and refreshes it automatically.

Credentials are stored in the OS keychain and persist across restarts.

Headless alternatives (env vars, first match wins):

- `ZAI_WORKER_EMAIL` + `ZAI_WORKER_PASSWORD` — auto-login
- `ZAGENT_GLM_API_KEY` — pre-issued bearer token
- `ZAI_WORKER_URL` — override the gateway base URL

### Additional language model providers

Beyond upstream Zed, Zagent wires extra language model providers into the agent panel,
each with its own model registry and streaming adapter:

| Provider | Crate |
| --- | --- |
| GLM (via embedded proxy) | `crates/language_models/src/provider/glm.rs` |
| JustWoker | `crates/language_models/src/provider/justwoker.rs` |
| Kilo | `crates/language_models/src/provider/kilo.rs` |
| NextRouter | `crates/language_models/src/provider/nextrouter.rs` |
| NVIDIA NIM | `crates/language_models/src/provider/nim.rs` |
| OpenCode | `crates/language_models/src/provider/opencode.rs` |
| TokenRouter | `crates/language_models/src/provider/tokenrouter.rs` |
| Zen | `crates/language_models/src/provider/zen.rs` |

Plus a thread sidebar ([`crates/sidebar`](./crates/sidebar)) for switching between
agent threads.

### Installation

Linux and Windows builds are produced by CI:

- **Linux**: download the `zagent-linux-x86_64` artifact from a successful
  [Build Linux](https://github.com/TLQB/zagent/actions/workflows/build-linux.yml) run, then:

  ```sh
  unzip zagent-linux-x86_64.zip
  install -m 755 zagent ~/.local/bin/zagent
  ```

- **Windows**: download the `zagent-windows-x86_64` artifact (contains `zed.exe` plus
  the bundled `OpenConsole.exe` / `conpty.dll` for the integrated terminal) from a
  [Build Windows](https://github.com/TLQB/zagent/actions/workflows/build-windows.yml) run.

Artifacts expire 14 days after the build. The editor binary is named `zagent` (the crate remains `zed` internally).

### Building from source

Zagent uses the same build system as upstream Zed:

- [Building for macOS](./docs/src/development/macos.md)
- [Building for Linux](./docs/src/development/linux.md)
- [Building for Windows](./docs/src/development/windows.md)

To build the editor only, skipping the collab server and other workspace members:

```sh
cargo build --release -p zed
```

### Vendored dependencies

- `vendor/xim-ctext` is a patched copy of the upstream crate. It fixes a
  COMPOUND_TEXT decoding bug where the `ESC % @` ("UTF-8 End") escape sequence was
  treated as a terminator, discarding the remaining bytes in the chunk. This broke
  X11 input methods that interleave UTF-8 and Latin-1 segments — notably Vietnamese
  Telex via `ibus-unikey`, where pressing Space dropped the last character of a
  syllable. The patch is applied through `[patch]` in the workspace `Cargo.toml`.

### Licensing

Zagent source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0
components where marked, following upstream Zed.

License information for third party dependencies must be correctly provided for CI
to pass. We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to
automatically comply with open source licenses. If CI is failing, check the
following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

### Upstream

For documentation, contribution guidelines, and general editor features, see the
[upstream Zed repository](https://github.com/zed-industries/zed). Because this
repository is a snapshot fork, upstream improvements are ported as targeted
cherry-picks (see the commit history for recently ported fixes) rather than merged
branch-wide.
