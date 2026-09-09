# FrameAnchor

<p align="center"><img src="src-tauri/icons/icon.png" width="128" alt="FrameAnchor icon"></p>

A Windows GPU physical-core tuning tool. [繁體中文](README.md) · **English**

FrameAnchor compares GPU interrupt-affinity candidates using a synthetic workload, with results, history, manual application and restoration. It opens directly on the GPU page and exits when you close it while idle.

## Test workflow

1. Calibrate the FPS cap (Vulkan by default; advanced settings also offer a fixed cap).
2. Shuffle N physical-core candidates. Warm up each for 3 seconds, then capture for 10 seconds.
3. Retest the top two: 5 seconds of warmup and 20 seconds of capture each, reversing their relative screening order.
4. Restore the complete pre-test GPU policy.

The normal schedule contains `N + min(N, 2)` candidate captures; calibration, retries and final restoration are additional. Estimates come from the same backend schedule, including calibration, startup and restart costs. Waits and retries may extend the actual duration. Advanced settings expose durations, workload, resolution and FPS caps.

Each candidate includes every LP of its physical core, including SMT siblings and core 0. Hybrid CPUs offer P-cores only. Unsupported processor-group topologies have no candidates. Labels consistently show, for example, “Physical core 1 (LP 2, 3)”.

## Results and application

Screening and retest evidence are stored separately. Both use the existing competitive score; final ranking uses retest data only. Results are consistent, close, reversed or insufficient; a single candidate has no comparison candidate.

A relative retest score gap of at most 0.5% is marked close. This is a display heuristic, not FPS improvement or statistical significance. Quick tests rank this synthetic workload only and do not establish an advantage over the original Windows policy.

Either valid retested core can be selected and applied after confirmation. Close or reversed results do not preselect a winner or trigger extra testing. Failed, cancelled and insufficient results cannot be applied. Independent manual selection is available under advanced settings and labeled “Not tested in this session”.

The frontend sends core IDs only. The backend verifies the session HMAC, GPU, CPU fingerprint and complete tested LP set before building the mask, writing, restarting and reading it back. Errors trigger recovery attempts. Unfinished recovery journals are retained. Restoring an original policy is not restricted by the new candidate rules.

## Upgrade and data

- Legacy single-LP history is view-only. It cannot be applied, expanded to a whole core or automatically re-signed.
- Existing GPU restoration records remain valid. Multi-bit policies display their complete LP set.
- Legacy game CPU rules remain in the configuration, including when general settings are saved, but never execute. Restart any running game modified by an older version.
- The tray, Dashboard, game rules, autostart and minimized startup are removed. Recognizable legacy startup tasks are cleaned up; failures display a reason and a retry action.
- Language, theme, updates and the data-folder action remain available. Data lives in `%APPDATA%\FrameAnchor`.
- Closing the window exits while idle. GPU testing, application or restoration blocks exit until operations and cleanup finish.

GPU policy changes require administrator privileges and restart the display device; the display may briefly go black. ETW/CSV integrity, window checks, cancellation and crash recovery remain in place.

## Installation

### Install from Releases

Download the latest version from [GitHub Releases](https://github.com/LiuTouo/FrameAnchor/releases). Two distribution forms are provided:

- **NSIS installer** (`FrameAnchor_X.Y.Z_x64-setup.exe`): Standard install mode. Supports automatic updates via the Tauri updater plugin.
- **Portable** (`FrameAnchor_X.Y.Z_x64-portable.zip`): Extract to any directory and run. Supports online update checking at startup and on manual request; can download a new version, ask for confirmation, replace the executable, and restart.

Every release asset includes a SHA256 checksum file (`.sha256`).

### Build from Source

#### Prerequisites

- Windows 11
- [Node.js](https://nodejs.org/) 20 or later
- [Rust](https://www.rust-lang.org/tools/install) 1.80 or later with the MSVC toolchain
- Visual Studio Build Tools with the **Desktop development with C++** workload
- Microsoft Edge WebView2 Runtime, included with Windows 11 by default

#### Build Steps

```bash
npm ci
npm run build:app
```

`npm run build:app` is the canonical local full desktop application build. It runs `tauri build --no-sign` and produces an unsigned release executable and NSIS installer. Signed releases continue through `npm run tauri build` or the GitHub release workflow (which provides the signing secrets).

The NSIS installer is written to:

```text
src-tauri/target/release/bundle/nsis/
```

## Development and Building

Install dependencies:

```bash
npm ci
```

Common commands:

```bash
npm run dev
# Starts only the Vite frontend at http://localhost:1420
# The Rust backend and Tauri IPC are not available

npm run tauri dev
# Starts the complete Tauri application

npm run check
# Runs svelte-check and TypeScript checks

npm run build
# Builds the frontend into dist/

npm run build:app
# Local full desktop build (unsigned release executable and NSIS installer);
# runs tauri build --no-sign, no updater signing secret required

npm run tauri build
# Full build including updater signing; requires TAURI_SIGNING_PRIVATE_KEY,
# used mainly by the GitHub release workflow

npm run gen-icons
# Regenerates src-tauri/icons/*

npm run build:benchmark-assets
# Builds the D3D9 workload sidecar (Rust + Direct3D 9) and copies it into the resources dir

npm run verify:benchmark-assets
# Verifies bundled benchmark resources (SHA-256 of PresentMon/liblava and D3D9 sidecar presence)

npm run fetch:benchmark-assets
# Re-downloads PresentMon and the liblava workload and updates SHA256SUMS
```

`npm run tauri build` automatically runs the frontend build, the D3D9 sidecar build, and the resource verification in order, so the bundle always includes the bundled tools and their license notices.

Rust checks and tests:

```bash
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

The complete application and its process operations depend on Windows APIs. Changes involving live processes, affinity, priority, CPU Sets, the tray, Task Scheduler, or WebView2 still require manual verification on Windows against a disposable test process.

## Release Process

Maintainers trigger automated builds and releases by pushing a semantic version tag. The CI workflow validates version consistency across all files, checks the updater signing key, runs frontend type checks and Rust tests, then builds all artifacts:

- NSIS installer (`FrameAnchor_X.Y.Z_x64-setup.exe`) with `.sha256`
- Portable ZIP (`FrameAnchor_X.Y.Z_x64-portable.zip`) with `.sha256`
- Updater `latest.json` and signature files

### Updater Signing Key Setup

Before the first release, generate an Ed25519 key pair:

```bash
npm run tauri signer generate -- -w src-tauri
```

Set the private key content as the GitHub Actions secret `TAURI_SIGNING_PRIVATE_KEY` (and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` if password-protected). Write the public key into `src-tauri/tauri.conf.json` under `plugins.updater.pubkey`, replacing the placeholder `REPLACE_ME_WITH_YOUR_PUBLIC_KEY_BASE64`.

**Never commit the private key.** The CI workflow validates that the public key has been replaced and the secret is present; builds fail with a clear error otherwise.

### Release Steps

1. Sync version numbers in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json`.
2. Commit and tag with `vX.Y.Z` format.
3. Push the tag to trigger the workflow.
4. Download assets from [GitHub Releases](https://github.com/LiuTouo/FrameAnchor/releases).

Windows binaries are **not code-signed**. Windows Defender SmartScreen may show a warning on download and first launch. This is expected and does not affect functionality.

## Architecture

Tauri v2, Svelte 5, TypeScript and Rust. The backend constructs topology-derived candidates and captures frametimes through PresentMon. A single operation manager coordinates GPU changes, snapshots, HMAC verification, cancellation and crash recovery. Method version 3 stores complete core evidence in `summary.quick`; legacy `lp` and `bestLp` fields retain their logical-processor meaning.

## License

This project is licensed under the [GNU General Public License v3.0](LICENSE).

### Third-party notices

The GPU benchmark feature bundles and redistributes the following third-party components (each under its own license):

- **PresentMon 2.5.1** (Intel) — [MIT License](src-tauri/resources/benchmark/LICENSE-PresentMon.txt). Frame-time collection tool; verified by a fixed SHA-256 before execution.
- **liblava Vulkan workload** (`lava-triangle.exe`, distributed via valleyofdoom/AutoGpuAffinity and built on the liblava framework) — [MIT License](src-tauri/resources/benchmark/LICENSE-liblava.txt). Vulkan test workload; also SHA-256 verified before execution.
- **Direct3D 9 workload** (`d3d9-workload.exe`) — a sidecar written in Rust directly against the Win32 Direct3D 9 API by this project (see `src-tauri/d3d9-workload/`), licensed under GPL-3.0 like the project itself.

Full license texts and the SHA-256 manifest live in `src-tauri/resources/benchmark/`.
