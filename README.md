<p align="center">
  <a href="https://ruffle.rs"><img alt="Ruffle" src="https://ruffle.rs/logo.svg" /></a>
</p>
<p align="center">
  <a href="https://github.com/ruffle-rs/ruffle"><img alt="Fork of ruffle-rs/ruffle" src="https://img.shields.io/badge/fork%20of-ruffle--rs%2Fruffle-007acc?logo=github" /></a>
  <a href="#license"><img alt="License: MIT or Apache-2.0" src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-007acc" /></a>
  <a href="https://github.com/falpi/ruffle-enterprise/tags"><img alt="Release tags" src="https://img.shields.io/github/v/tag/falpi/ruffle-enterprise?filter=enterprise-*&label=release&color=007acc&logo=git" /></a>
  <br />
  <strong><a href="https://github.com/ruffle-rs/ruffle">upstream Ruffle</a> | <a href="https://ruffle.rs">ruffle.rs</a> | <a href="https://github.com/ruffle-rs/ruffle/wiki">upstream wiki</a></strong>
</p>

# Ruffle Enterprise

**A downstream fork of [Ruffle](https://github.com/ruffle-rs/ruffle) targeted at large-scale Adobe Flex 4.6 enterprise applications with heavy XML data workloads.**

This fork accumulates patches and experiments that address constraints typical of enterprise Flex applications running on Ruffle: hundreds-of-megabytes E4X datasets, long-lived sessions with aggressive memory pressure, MX/Spark rendering precision, AVM compatibility edge cases, native APIs missing or incomplete upstream, and the 4 GiB Wasm32 memory ceiling. Whenever a change is general-purpose, it is proposed upstream as an isolated pull request; this repository bundles them together with Flex-specific patches whose narrow applicability makes them unsuitable for upstream submission.

This is **not** a replacement of upstream Ruffle and does **not** claim official status. All trademarks and project assets (the Ruffle name and logo) are property of the upstream Ruffle authors; the "Enterprise" qualifier here only describes the workload class this fork is tuned for.

## Table of Contents
* [When this fork is for you](#when-this-fork-is-for-you)
* [Project status](#project-status)
* [Using this fork](#using-this-fork)
* [Building a release](#building-a-release)
* [Branches](#branches)
  * [Published branches](#published-branches)
  * [Memory & E4X data workloads](#memory--e4x-data-workloads)
  * [Native AS3 APIs](#native-as3-apis)
  * [MX/Spark rendering & input fixes](#mxspark-rendering--input-fixes)
  * [AVM compatibility fixes](#avm-compatibility-fixes)
  * [Experimental targets and subsystems](#experimental-targets-and-subsystems)
* [Relationship with upstream](#relationship-with-upstream)
* [Structure](#structure)
* [License](#license)
* [Contributing](#contributing)


## When this fork is for you

Use this fork if:
* You run Flash applications built with **Adobe Flex 4.6 SDK** in a production / enterprise context.
* Your SWF processes **XML datasets of tens to hundreds of thousands of E4X nodes** and you observe memory pressure or out-of-memory failures on the standard builds.
* You need native APIs that upstream does not implement or implements only partially (streaming `URLStream`, an XLSX/CSV export utility, complete E4X change-notification events).
* You see specific glitches on MX/Spark components — hairline grid separators, mouse hover on buttons, click release semantics, `scrollRect` hit-testing — that block production use.
* You ship a SWF inside an **Electron** wrapper and hit `WebAssembly.instantiateStreaming` failures.
* You are evaluating **wasm64** as a way to lift the 4 GiB linear-memory ceiling for very large sessions.

Use **upstream Ruffle** for everything else: games, animations, generic SWF content, AS1/AS2 workloads, casual use, browser extension distribution.


## Project status

The fork tracks upstream `master` on a dedicated branch with zero divergence. Fork-only meta assets (this README, the rebuild scripts, and any other content specific to the fork) live on the [`ruffle-enterprise`](../../tree/ruffle-enterprise) branch as an independent thin layer above `master`. Each `fix-*` / `add-*` feature branch is rebased on the latest upstream `master` and is intended to be submittable as an isolated pull request. The current status of each upstream PR is recorded in the branch tables below.

Release builds are produced by rebuilding a **local, ephemeral** integration branch (`local/release-merge-wasm32`, and its wasm64 sibling) that merges `master + feature branches` in a defined order. That branch is a build artefact and is not pushed to the remote — every rebuild rewrites its history from scratch. Publishable release state is captured on annotated tags (`enterprise-vX.Y.Z-wasm32` / `-wasm64`) applied to a validated integration tip and pushed to the remote as immutable references. See [Building a release](#building-a-release) for the operational procedure.

For functional Flash Player emulation status, refer to the [upstream project status](https://github.com/ruffle-rs/ruffle#project-status) — this fork does not alter the supported subset of ActionScript / SWF features.


## Using this fork

All general documentation — usage, prerequisites, contributing, project structure — is **unchanged from upstream** and lives there. Refer to:

* [Upstream README](https://github.com/ruffle-rs/ruffle/blob/master/README.md) for build prerequisites, desktop / web / Android / scanner / exporter instructions
* [Upstream Wiki](https://github.com/ruffle-rs/ruffle/wiki) for end-user guides
* [CONTRIBUTING.md](CONTRIBUTING.md) for the upstream contribution guidelines (unchanged here)

To **consume** an already-produced release, check out the release tag rather than a branch:

```shell
git checkout enterprise-vX.Y.Z-wasm32     # or enterprise-vX.Y.Z-wasm64
cargo run --release --package=ruffle_desktop
```

Tags are immutable and reproducibly point at the exact commit that was validated for that release; unlike branches, they are not rewritten by future rebuilds.

To **produce** a new release build yourself (which is the way to get the latest enterprise patch-set applied to the current upstream tip), see [Building a release](#building-a-release) below.


## Building a release

Release builds are produced by rebuilding the local, ephemeral `local/release-merge-wasm32` and `local/release-merge-wasm64` integration branches. A single run of [`scripts/Rebuild-EnterpriseIntegration.ps1`](scripts/Rebuild-EnterpriseIntegration.ps1) (which lives on the [`ruffle-enterprise`](../../tree/ruffle-enterprise) branch) produces both.

### Prerequisites

* A local clone of this repository with `origin` pointing at `falpi/ruffle-enterprise`. No `upstream` remote is required for the build procedure itself: the script reads only `origin/*` refs.
* Latest stable Rust toolchain (for the standard build) and Rust beta (used by the CI-gate checks). For actually **building** the wasm64 variant, additionally a nightly toolchain with `-Z build-std` support for the Tier-3 `wasm64-unknown-unknown` target. The rebuild script itself does not compile wasm64 — it only produces the integration branch — so nightly is only needed at build time.
* The tips on `origin` are the source of truth. In particular the script assumes that all feature branches listed in its `$MergeOrder` are rebased on the current `origin/master`. Moving to a newer upstream `master` is therefore a separate operational step (fetching upstream, fast-forwarding origin's `master`, and rebasing every feature branch on the new tip) and is intentionally not part of this build procedure.

### Rebuild procedure

1. Refresh remote-tracking refs and switch to the branch that carries the script:
    ```shell
    git fetch origin
    git checkout ruffle-enterprise
    ```
    The script merges from `origin/<feature-branch>` remote-tracking refs directly — the feature branches do **not** need to be checked out locally. `git fetch origin` is enough to bring their tips up to date (and to pick up any new branch that may have been added to origin's `$MergeOrder` since the last fetch).
2. Run the rebuild script (PowerShell 5.1+ on Windows, no dependencies beyond git and cargo):
    ```shell
    powershell.exe -File .\scripts\Rebuild-EnterpriseIntegration.ps1
    ```
    Available flags: `-DryRun` (print each command without executing), `-NoChecks` (skip fmt/clippy gate). In order, the script:
    * resets `local/release-merge-wasm32` to the local `master` branch,
    * merges each feature branch with `--no-ff` in the order defined by `$MergeOrder`,
    * runs `rustfmt --check --edition 2024` on files touched by the branch,
    * runs `cargo +beta clippy --all --tests` with JSON-parsed output, gating only findings that fall on files modified by the branch (upstream lints stay informational),
    * chains `local/release-merge-wasm64` on top of the wasm32 tip by resetting it to wasm32 and merging `origin/add-wasm64-target`.
    By construction, `git diff local/release-merge-wasm32 local/release-merge-wasm64` equals the wasm64 feature patch.
3. Test the resulting builds (desktop and/or web for wasm32; web with a Memory64-capable browser for wasm64).
4. If validated, tag each release tip and push the tags:
    ```shell
    git tag -a enterprise-vX.Y.Z-wasm32 -m "Enterprise release X.Y.Z (wasm32)"
    git tag -a enterprise-vX.Y.Z-wasm64 -m "Enterprise release X.Y.Z (wasm64)"
    git push origin enterprise-vX.Y.Z-wasm32 enterprise-vX.Y.Z-wasm64
    ```

### What the script reads and touches

* **Every merge reads `origin/<branch>` remote-tracking refs, never local branches.** If a same-named local branch exists, it is **ignored regardless of its state** — ahead of origin, behind, diverged, with uncommitted mods, whatever. A commit that has not been pushed to origin **will not appear** in the integration build, even when it exists on a local branch you are currently checked out on. This enforces a "push to publish" discipline that keeps releases fully determined by the state of `origin`: any two clones running the script against the same origin snapshot produce the same commit tree.

* **Working-tree caveat.** The script begins by checking out `local/release-merge-wasm32`. If the branch you are currently on has uncommitted modifications to a file whose contents differ on the target branch, `git checkout` will refuse with `your local changes would be overwritten by checkout`. Commit, stash, or discard those modifications and re-run. The feature branches themselves are never checked out by the script — their contents are read through the remote-tracking ref rather than via a checkout — so any local feature-branch working state remains untouched by a rebuild.

### Design notes

* Both integration branches are **local by convention** (the `local/` prefix follows the same convention used for other local-only branches in this workflow). They are intentionally never pushed: republishing artefacts whose history is rewritten on every rebuild would only add noise to the remote.
* The rebuild is deterministic given the ingredients — `master` + the tips of the feature branches + the script's merge order + `git rerere` state for pre-recorded conflict resolutions. Any two clones running the script on the same inputs produce the same commit tree.
* `git rerere` is enabled by the script (idempotent) so recurring merge conflicts (e.g. between `xml-general-improvements` and `xml-readonly-improvements`) are resolved once and replayed automatically on every subsequent rebuild.
* The wasm64 chain runs only if the wasm32 rebuild and CI gate succeeded. If either fails, the script aborts before touching the wasm64 branch, so wasm64 never reflects an unvalidated wasm32 tip.


## Branches

The repository exposes the branches listed below. Single-purpose branches (one feature or one fix each) are intentionally kept rebasable so they can be submitted upstream as isolated PRs; the rebuild script combines a curated subset of them into a local integration branch for release builds (see [Building a release](#building-a-release)).

Per-branch design notes, when present, live under [`docs/`](docs/) on the [`ruffle-enterprise`](../../tree/ruffle-enterprise) branch (this one). They are collected here rather than committed alongside the feature branches so upstream PRs stay code-only.

### Published branches

| Branch | Purpose |
| --- | --- |
| [`master`](../../tree/master) | Strict mirror of [upstream `ruffle-rs/ruffle@master`](https://github.com/ruffle-rs/ruffle). Never committed to directly; advanced only via `git fetch upstream && git merge --ff-only`. |
| [`ruffle-enterprise`](../../tree/ruffle-enterprise) | **Fork identity branch (default landing branch).** Carries only the fork-only meta assets (this README, `scripts/`, `docs/`) as a thin linear layer above `master`. Rebased on top of `master` when upstream advances (via a separate helper script). Not intended for building and not a parent of the integration branch — its content is deliberately kept isolated so the build integration stays a pure "master + features" tree. |

The integration branches (`local/release-merge-wasm32` and `local/release-merge-wasm64`) are **not published** here: they are ephemeral local artefacts rebuilt from scratch on every release. Their releases are published as tags (`enterprise-vX.Y.Z-wasm32` / `-wasm64`), which are the correct target for anyone wanting a fixed reference to a validated build.

### Memory & E4X data workloads

| Branch | Upstream PR | Summary |
| --- | --- | --- |
| [`fix-memory-leaks`](../../tree/fix-memory-leaks) | [#23897](https://github.com/ruffle-rs/ruffle/pull/23897) (open) | Five independent AVM2 leak fixes for long-running ActionScript 3 sessions: `Dictionary` weak-keys handling, `EventDispatcher` strong/weak handler refs, `System.gc()` performing an actual full GC cycle, `DispatchList` add-time prune, `Dictionary` set-time prune. Each fix is self-contained and PR-ready. |
| [`tile-memory-allocator`](../../tree/tile-memory-allocator) | [#24000](https://github.com/ruffle-rs/ruffle/pull/24000) (closed — retained here) | Size-class slab allocator (`tilemalloc`) for the small-object churn typical of heavy E4X workloads. Intrusive free lists, O(1) alloc/dealloc, built-in diagnostic instrument suite (waste tracking, request-size histogram, peak alive tiles, realloc counters). Empirical 2–3× speed-up on 100 k-row XML loads and unlocks 250 k-row scenarios. Closed upstream as too specific, retained here for production use. See [`docs/tile-memory-allocator.html`](docs/tile-memory-allocator.html). |
| [`xml-general-improvements`](../../tree/xml-general-improvements) | _(fork-only)_ | Layered E4X record shrinks: `E4XNodeKind::Element` boxed payload, `children` field as `SmallVec<[_; 1]>` with single-child inline collapse, `namespaces` field as lazy `Option<Box<Vec>>`, string interning for values and namespaces on parse. Drops per-node footprint for leaf-dominated XML trees by ≈30% with no API change. See [`docs/xml-general-improvements.html`](docs/xml-general-improvements.html). |
| [`xml-readonly-improvements`](../../tree/xml-readonly-improvements) | _(fork-only)_ | New `XMLReadOnly` built-in type with arena-backed storage and O(1) chunk tracing. Provides full E4X query parity (`..`, `@`, `for each`, indexing, `length()`, text accessors) plus a native `sortKeyed()` fast-path for row-oriented datasets. Node record reduced from 36 B → 28 B with single-child inline collapse: ≈33% memory reduction on a 250 k-row reference workload. See [`docs/xml-readonly-improvements.html`](docs/xml-readonly-improvements.html). |
| [`xml-notifications-completion`](../../tree/xml-notifications-completion) | [#24131](https://github.com/ruffle-rs/ruffle/pull/24131) (open) | Completes the E4X change-notification surface: `textSet`, `nodeAdded`, `nodeRemoved`, `namespaceAdded`, `namespaceRemoved` events that are missing upstream. Without them, Flex property binding on E4X-derived structures silently ignores certain mutations, which is the root cause of long-standing "the panel doesn't refresh" symptoms in enterprise applications. See [`docs/xml-notifications-completion.html`](docs/xml-notifications-completion.html). |

### Native AS3 APIs

| Branch | Upstream PR | Summary |
| --- | --- | --- |
| [`add-export-instruments`](../../tree/add-export-instruments) | _(fork-only)_ | Native `ExportUtils` built-in providing streaming XLSX / CSV export from AS3, sized for enterprise recordsets. Streaming ZIP with on-the-fly deflate keeps peak memory O(1) per row, so 1 M-row exports do not push the wasm32 4 GiB ceiling. Automatic per-column type detection (numeric / date / string) with correct XLSX cell formatting. Sync (`syncExport`) and async-chunked (`asyncExport*`) variants; a companion `FlexUtils.exportGridToExcel` wrapper drives an `ActiveProgress` popup on Flex `DataGrid` sources. See [`docs/add-export-instruments.html`](docs/add-export-instruments.html). |
| [`add-urlstream-implementation`](../../tree/add-urlstream-implementation) | [#24078](https://github.com/ruffle-rs/ruffle/pull/24078) (open) | Streaming implementation of `flash.net.URLStream`. Chunks are exposed to AS3 through `IDataInput` as they arrive over the wire instead of being buffered whole, enabling efficient consumption of large SOAP / binary recordset responses from long-lived enterprise sessions. Unifies the underlying transport with `URLLoader`. |
| [`add-font-text-engine-implementation`](../../tree/add-font-text-engine-implementation) | [#24153](https://github.com/ruffle-rs/ruffle/pull/24153) (open) | Implements the Flash Text Engine (`flash.text.engine`) on top of upstream's native `TextBlock` object: real line breaking driven by the core layout engine, real glyph metrics, `TextLine` chains, and content invalidation. Provides the low-level text substrate the Text Layout Framework (TLF) — and therefore Spark text components — builds on. See [`docs/add-font-text-engine-implementation.html`](docs/add-font-text-engine-implementation.html). |
| [`add-mouse-cursor-property`](../../tree/add-mouse-cursor-property) | [#24150](https://github.com/ruffle-rs/ruffle/pull/24150) (open) | Implements `flash.ui.Mouse.cursor` (a forced cursor tracked in the mouse state and applied in the player loop), enabling the I-beam and other system cursors over TLF / Spark text components that set the property. |

### MX/Spark rendering & input fixes

| Branch | Upstream PR | Summary |
| --- | --- | --- |
| [`fix-blurry-grid-separators`](../../tree/fix-blurry-grid-separators) | [#23837](https://github.com/ruffle-rs/ruffle/pull/23837) (open) | `lineStyle` 1 px orthogonal strokes are snapped to the pixel grid, fixing the blurry hairline separators rendered by `mx.controls.DataGrid` and similar Flex chrome. Closes upstream issue [#23814](https://github.com/ruffle-rs/ruffle/issues/23814). See [`docs/fix-blurry-grid-separators.html`](docs/fix-blurry-grid-separators.html). |
| [`fix-bitmapdata-filter-expansion`](../../tree/fix-bitmapdata-filter-expansion) | [#24136](https://github.com/ruffle-rs/ruffle/pull/24136) (open) | Native implementation of `BitmapData.generateFilterRect` (AVM1 + AVM2) together with dest-rectangle expansion in the wgpu `applyFilter` path. Root cause of the missing shadow on Flex `mx.controls.ToolTip` (halo `ToolTipBorder`, also under the Spark 4.6 theme): the stub returned `sourceRect` unchanged, so `mx.graphics.RectangularDropShadow` computed zero-thickness shadow slices and drew nothing. Same failure mode affects `DownloadProgressBar` / `SparkDownloadProgressBar` preloader shadows. A shared `Filter::calculate_dest_margins` helper keeps `generateFilterRect` and `applyFilter` margins consistent. See [`docs/fix-bitmapdata-filter-expansion.html`](docs/fix-bitmapdata-filter-expansion.html). |
| [`fix-mouse-events-clipping`](../../tree/fix-mouse-events-clipping) | [#23797](https://github.com/ruffle-rs/ruffle/pull/23797) (open) | Mouse hit-testing now respects `scrollRect` when picking AVM1/AVM2 children. Previously, children clipped out of view by `scrollRect` (typical of `VGroup` / `HGroup` with `clipAndEnableScrolling`) remained clickable. Closes upstream issue [#23678](https://github.com/ruffle-rs/ruffle/issues/23678). |
| [`fix-mouse-click-release-outside-buttons`](../../tree/fix-mouse-click-release-outside-buttons) | [#23803](https://github.com/ruffle-rs/ruffle/pull/23803) (open) | AVM2 no longer fires a spurious `click` when the mouse is released outside a pressed button. The structural equality fallback used by AVM1 (`check_display_object_equality`) was being incorrectly applied to AVM2, where dynamically-created objects all share depth = 0 and id = 0. Closes upstream issue [#23798](https://github.com/ruffle-rs/ruffle/issues/23798). |
| [`fix-mouse-hover-on-mx-buttons`](../../tree/fix-mouse-hover-on-mx-buttons) | [#23807](https://github.com/ruffle-rs/ruffle/pull/23807) (open) | Drag operations no longer double-dispatch `rollOver` / `rollOut` to the pressed object in AVM2. Fixes `mx.controls.Button` jumping to the `UP` skin state during outside-drag and failing to restore `DOWN` on re-entry. Closes upstream issue [#23801](https://github.com/ruffle-rs/ruffle/issues/23801). |
| [`add-text-input-event-dispatch`](../../tree/add-text-input-event-dispatch) | [#24151](https://github.com/ruffle-rs/ruffle/pull/24151) (open) | Dispatches `TextEvent.TEXT_INPUT` to focused AVM2 objects that are not native `EditText` — which is what enables keyboard entry in TLF-based Spark `TextInput` / `TextArea`. Previously only native text fields received the event, so Spark inputs silently swallowed typing. See [`docs/add-text-input-event-dispatch.html`](docs/add-text-input-event-dispatch.html). |

### AVM compatibility fixes

| Branch | Upstream PR | Summary |
| --- | --- | --- |
| [`fix-arcgis-flex-sdk-too-much-recursion`](../../tree/fix-arcgis-flex-sdk-too-much-recursion) | [#23355](https://github.com/ruffle-rs/ruffle/pull/23355) (open) | Reentrancy guard on the AVM2 `removed` / `removedFromStage` event dispatch. When an AS3 listener responds to the removal event by calling `removeChild` again on the same subtree, the recursive invocation re-enters the same dispatch and overflows the stack. A `BEING_REMOVED` flag on the display object is set at dispatch entry and cleared at exit, so the recursive call bails out immediately — matching Flash Player behaviour. Closes upstream issue [#22860](https://github.com/ruffle-rs/ruffle/issues/22860) (encountered while running the ArcGIS Flex SDK). |
| [`fix-collator-compare-normalization`](../../tree/fix-collator-compare-normalization) | [#24148](https://github.com/ruffle-rs/ruffle/pull/24148) (open) | Normalizes `flash.globalization.Collator.compare` to return exactly `-1` / `0` / `1`. Flex's `Sort.findItem` switches on the exact returned value for its binary search, so any other magnitude drove the Spark `DataGrid` sort into an infinite loop. |
| [`fix-electron-instantiate-streaming`](../../tree/fix-electron-instantiate-streaming) | [#23984](https://github.com/ruffle-rs/ruffle/pull/23984) (closed) | Detects Node-like environments and falls back to `WebAssembly.instantiate` from an `ArrayBuffer` instead of `instantiateStreaming`. Required since `wasm-bindgen` 0.2.120+ dispatches a `Response` object that Electron renderers with `nodeIntegration = true` reject. Relevant for SWFs shipped inside Electron-based desktop wrappers. |

### Experimental targets and subsystems

| Branch | Upstream PR | Summary |
| --- | --- | --- |
| [`add-wasm64-target`](../../tree/add-wasm64-target) | [#23981](https://github.com/ruffle-rs/ruffle/pull/23981) (closed — pending ecosystem maturity) | Opt-in `wasm64-unknown-unknown` (Memory64) build target alongside the standard `wasm32` builds. Lifts the 4 GiB linear-memory ceiling on browsers supporting Memory64 (Chrome / Firefox via V8 / SpiderMonkey). Tier-3 Rust target, requires `-Z build-std`. Closed upstream pending crate-ecosystem support; retained here as the feature branch merged into the `local/release-merge-wasm64` integration on top of the wasm32 build. |
| [`pluggable-font-renderer`](../../tree/pluggable-font-renderer) | [#23492](https://github.com/ruffle-rs/ruffle/pull/23492) (open) | Indirection layer in the device-font rasterization path so embedders can plug in an external rasterizer module (the reference implementation drives GDI on Windows for pixel-perfect parity with native Flash Player). See [`docs/pluggable-font-renderer.html`](docs/pluggable-font-renderer.html). |


## Relationship with upstream

* **`master`** is a strict mirror of upstream. It is advanced via `git fetch upstream && git merge --ff-only`; no commit lands directly on it. Tags and releases produced by upstream are not republished here.
* **`ruffle-enterprise`** carries only fork-only meta assets (this README, `scripts/`, `docs/`) as a thin linear layer above `master`. When `master` advances, it is rebased on the new tip and force-pushed. It contains no functional code changes and does not feed into the integration branch.
* Each `fix-*`, `add-*` and single-feature branch is rebased on top of `master` and intended to be submittable as a self-contained PR. The upstream PR number, where one exists, is listed in the tables above and is the authoritative description of the patch.
* The `local/release-merge-wasm32` / `local/release-merge-wasm64` integration branches are **local, ephemeral build artefacts**: rebuilt fresh from `master + feature branches` on every run of the rebuild script, and intentionally never pushed to the remote. The base branch `ruffle-enterprise` is kept out of that chain so the integration stays a pure "master + features" tree. They are not intended to be submitted upstream.
* **Releases are published as tags**, not as branches. Tags follow the pattern `enterprise-vX.Y.Z-wasm32` / `-wasm64` and point at the validated tip of the corresponding integration branch. They are immutable and are the correct reference for anyone wanting to check out a specific release build.
* **Issues filed on this repository should concern this fork** — Flex enterprise workloads, the patches listed above, or release packaging. For upstream bugs unrelated to this scope, please file them on [ruffle-rs/ruffle/issues](https://github.com/ruffle-rs/ruffle/issues).


## Structure

The directory layout is unchanged from upstream, with two fork-specific additions (both present only on the `ruffle-enterprise` base branch):

- `core` — core emulator and common code
- `swf` — SWF and ActionScript parser
- `desktop` — desktop client (uses `wgpu-rs`)
- `web` — web client and browser extension (uses `wasm-bindgen`)
- `render` — various rendering backends for both desktop and web
- `video` — video decoding backends
- `flv` — Flash Video decoder
- `wstr` — a Flash-compatible implementation of strings
- `scanner` — a utility to bulk parse SWF files
- `exporter` — a utility to generate PNG screenshots of a SWF file
- `scripts` — **(fork-specific)** rebuild-integration and branch-rebase automation for producing release builds
- `docs` — **(fork-specific)** per-branch design references (one self-contained HTML per feature branch), collected here so the feature branches and their upstream PRs stay code-only


## License

This fork inherits the dual license from upstream Ruffle:

- Apache License, Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0)
- MIT License (http://opensource.org/licenses/MIT)

at your option.

Ruffle depends on third-party libraries under compatible licenses. See [LICENSE.md](LICENSE.md) for full information.


### Contributing

Contributions targeting the Flex enterprise scope are welcome on this repository. Generic patches — unrelated to Flex, MX/Spark components, heavy-XML workloads, wasm64, or the specific compatibility fixes above — should be submitted to [upstream Ruffle](https://github.com/ruffle-rs/ruffle) directly, where they reach the broader Ruffle audience and review process.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work shall be dual licensed as above, without any additional terms or conditions.

The entire Ruffle community, including the chat room and GitHub project, is expected to abide by the [Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct) that the Rust project itself follows.
