# Core <-> UI contract

`crates/steward-core` is the only backend. The Tauri shell (`src-tauri`, crate `disk-steward`) calls it and forwards
the serde types below unchanged (see "Tauri commands" at the end). Types live in `crates/steward-core/src/model.rs`. Every JSON key is camelCase; enum values are snake_case.
All byte counts are plain integers (`u64`, except `freedBytes: i64`). Times are RFC3339 UTC (`2026-11-24T15:18:00Z`),
days are `YYYY-MM-DD`. All functions return `Result<_, String>`; the string is an English message for logs.

## Types

### DiskStatus
`{ volume: string, totalBytes: u64, freeBytes: u64, usedBytes: u64, measuredAt: string }`
Data volume (`/System/Volumes/Data`) via statfs. `freeBytes` is what a normal user can use; `usedBytes = totalBytes - freeBytes`.
`totalBytes`/`freeBytes` are container-wide on APFS (shared with the other volumes of the container).

### Target
```
{ id: string, path: string,
  kind: "toolchain" | "package_cache" | "store" | "build" | "custom",
  enabled: bool, allowed: bool,
  logicalBytes: u64, onDiskBytes: u64, privateBytes: u64 | null,
  totalFiles: u64, compressedFiles: u64,
  risk: "none" | "clone_shared" | "rewritten_often" | "blocked_by_rule",
  riskDetail: string | null }
```
- `enabled`: the absolute path is in the engine's top-level `patterns`.
- `allowed`: may be enabled. False for `blocked_by_rule`, `rewritten_often` and `clone_shared` targets. The CLI/API must
  refuse to enable a target with `allowed: false` (and always refuses `clone_shared`), and `apply` refuses while one is enabled.
- `logicalBytes`: sum of file sizes. `onDiskBytes`: sum of `st_blocks * 512` (what `du` shows; counts clones in full).
  `compressedFiles`: files with `UF_COMPRESSED`. Hardlinks are counted once, symlinks are skipped, other filesystems are not entered.
- `privateBytes`: bytes not shared with any APFS clone (getattrlist `ATTR_CMNEXT_PRIVATESIZE`). `onDiskBytes - privateBytes`
  is what the clones share with files elsewhere. `null` when the filesystem refuses the attribute (also for `--no-measure`
  and for pseudo targets). Compressed files count their on-disk size as private (the kernel reports 0 for them).
- Sizes are all `0` and `privateBytes` is `null` when measurement is skipped (`measure = false`), the path does not exist,
  or the path is a glob (`rust-target`).
- `id` is stable: `rustup`, `cargo-registry`, `npm`, `mise`, `uv-cache`, `bun-store`, `pnpm-store`, `rust-target`, or
  `custom:<path with ~>` for engine patterns the catalog does not know.

Built-in catalog (order of the `targets` array, custom ones last):

| id | path | kind | rule |
|---|---|---|---|
| `rustup` | `~/.rustup` | toolchain | allowed |
| `cargo-registry` | `~/.cargo/registry` | package_cache | allowed |
| `npm` | `~/.npm` | package_cache | allowed |
| `mise` | `~/.local/share/mise` | toolchain | allowed |
| `uv-cache` | `~/.cache/uv` | package_cache | allowed unless `clone_shared` (see below) |
| `bun-store` | `~/.bun/install` | store | blocked, `blocked_by_rule` (bun installs by clonefile; compressing un-shares clones) |
| `pnpm-store` | `~/Library/pnpm` | store | allowed (global virtual store uses symlinks; hardlinked files are skipped by the engine) |
| `rust-target` | `~/**/target` (glob, pseudo target, never measured) | build | blocked, `rewritten_often` |

Any custom path named `target` is also blocked as `rewritten_often`; other custom paths are allowed with risk `none`.

Risk rules: `clone_shared` for `uv-cache` when any Python venv (dir with `pyvenv.cfg`, found under `~/dev` and
`~/.local/share/uv/tools`, depth <= 8, skipping `node_modules`, `.git`, `target`, `Library`) has module files
(`.py`/`.so`/`.pyi` outside `*.dist-info`) that are regular files instead of symlinks. `riskDetail` is a one-line English
string for the UI tooltip (not for translation).

### Measure
`{ logicalBytes, onDiskBytes, privateBytes: u64 | null, totalFiles, compressedFiles }`: the size part of a Target
(`steward measure <path> --json`).

### Savings
`{ files: u64, originalBytes: u64, onDiskBytes: u64, savedBytes: u64, ratio: f64, quarantined: u64, lastRunDay: string | null, unattributedBytes: u64 }`
Parsed from `auto-compress-apfs stat`. The engine prints sizes with one decimal (`4.9 GiB`), so the byte counts are
approximate (about +-50 MiB at GiB scale). `lastRunDay` is null when the engine has never run.

`unattributedBytes` = engine `savedBytes` minus the sum of `logicalBytes - onDiskBytes` over the **enabled** targets
(signed sum), never negative; `0` when sizes were not measured. `savedBytes` stays the engine total. Why they differ
(measured 2026-11-24: engine 3.5 GiB, targets 2.9 GiB, so about 0.6 GiB unattributed):
1. About 0.49 GiB: block-rounding slack. `logical - onDisk` of a target also counts every file the engine did not
   compress (small files, excluded types, files under the minimum size). Those sit in whole 4 KiB blocks, so their
   `onDisk` is larger than their `logical` and they subtract from the target's "saved". The engine only counts the files it
   compressed. Summing only compressed files (`UF_COMPRESSED`) gives 3.39 GiB, which matches the engine.
2. About 0.15 GiB: stale engine records. The engine's `state.tsv` keeps one row per file it ever compressed and never
   revisits it; 103 of those files were deleted since (cargo registry and toolchain churn) and 34 were rewritten
   uncompressed, so they still count in `stat` but no longer exist in any target.
No file outside the enabled patterns contributed (every state row was under `~/.rustup`, `~/.cargo/registry`, `~/.npm`).
The UI should show `savedBytes` as the headline and `unattributedBytes` as "not attributed to a folder".

### Measurement
`{ state: "fresh" | "stale" | "measuring" | "never", measuredAt: string | null }`
How trustworthy the sizes of an `Overview` are. Core produces `fresh` (just measured, `measuredAt` set) or `never`
(`with_sizes = false`); the app adds `stale` (measured more than 6 h ago) and `measuring` (a measurement is running).

### Preview
`{ files: u64, bytes: u64, skipped: u64, roots: [{ path: string, bytes: u64 }] }`
What `compress` (no `--apply`) would compress now. `roots` are the engine's top roots by size, `~` expanded. Approximate bytes (one decimal).

### RunResult
`{ startedAt: string, durationMs: u64, freedBytes: i64, preview: Preview, errors: string[] }`
Result of `apply`. `preview` is what was about to be compressed (taken right before the run). `freedBytes` is the
data-volume free-space delta (after - before; can be negative or noisy while other processes write). `errors` are the
engine's `ERROR`/`WARN` stderr lines, or one line when it exited non-zero.

### ScheduleAgent
`{ label: string, loaded: bool, lastExitStatus: i32 | null, logPath: string | null }`
Agents listed, in order: `local.auto-compress-apfs` (the engine's daily agent, installed with `auto-compress-apfs install-launchd`;
default log `~/Library/Logs/auto-compress-apfs/launchd.log`) and `com.lucasbrito.docker-gc` (default log `~/Library/Logs/docker-gc.log`).
`loaded` = `launchctl list <label>` succeeds; `lastExitStatus` and `logPath` come from its output (`logPath` falls back to the default when not loaded).

### DockerUsage
`{ available: bool, imagesBytes: u64, buildCacheBytes: u64, volumesBytes: u64, reclaimableBytes: u64 }`
From `docker system df --format json` (decimal units). `reclaimableBytes` sums images, containers, volumes and build cache.
When Docker is not running or not installed: `available: false` and every number `0`.

### Overview
`{ disk: DiskStatus, savings: Savings, targets: Target[], schedule: ScheduleAgent[], docker: DockerUsage, measurement: Measurement }`
With `measurement.state = "never"` (no sizes) `docker` is left at its default (`available: false`): `docker system df`
takes seconds, so it is only queried by measured overviews.

## Rust API (`steward_core`)

| Function | Returns |
|---|---|
| `disk::status()` | `io::Result<DiskStatus>` |
| `measure::measure(&Path)` | `io::Result<Measure>` |
| `overview::overview(&Engine, with_sizes: bool)` | `Result<Overview, String>` |
| `overview::targets(&Engine, with_sizes: bool)` | `Result<Vec<Target>, String>` (one thread per target, 4 workers each) |
| `overview::resolve(&Engine, "<id or path>")` | `Result<catalog::Entry, String>` |
| `Engine::{new, preview, apply, stat, patterns, set_enabled, check_launchd}` | see `engine.rs`; `Engine { bin, config, home }` fields are public |
| `Engine::apply_with(preview, on_start(pid), on_line(&str))` | `Result<(RunResult, bool /* exited ok */), String>`; streams the engine log (`compress --apply --no-progress -v`, stderr) |
| `Engine::{install_launchd, uninstall_launchd}` | `Result<(), String>`; run `install-launchd` / `uninstall-launchd --config <engine.config>` (they write `~/Library/LaunchAgents`) |
| `overview::unattributed_bytes(saved, &[Target])` | `u64` |
| `schedule::list(&Path /* home */)` | `Vec<ScheduleAgent>` |
| `docker::usage()` | `DockerUsage` |

`Engine::set_enabled(path, bool)` edits only the top-level `patterns` array with `toml_edit` (comments and the
`[trash]`/`[purge]`/`[dedup]`/`[tm_exclude]` sections stay byte-identical); it adds `~/...` when the path is under home.
The shell is responsible for the policy: refuse `set_enabled(.., true)` when `Target.allowed` is false
(`steward enable` does this), and call `apply` only after explicit user confirmation.

## CLI `steward`

`steward <command> [--json] [--no-measure] [--yes] [--config <path>] [--engine <path>]`

| Command | JSON output (`--json`) |
|---|---|
| `overview` | `Overview` |
| `targets` | `Target[]` |
| `measure <path>` | `Measure` |
| `preview` | `Preview` |
| `enable <id\|path>` / `disable <id\|path>` | text only; exit code 1 with the reason when refused (`allowed: false` or `clone_shared`) |
| `apply --yes` | `RunResult`; refuses without `--yes` and while an enabled target is not allowed |
| `schedule` | `ScheduleAgent[]` |

Errors go to stderr with exit code 1. `--no-measure` skips the file walk (instant; sizes `0`, `privateBytes: null`).
Measuring is kernel-bound (`~/.cache/uv` and `~/.bun/install` hold 150k-300k files each), so a full `overview` on a
dev machine takes seconds even in a release build; the UI should fetch with `measure = false` first and fill the sizes
afterwards. Directory walks use `getattrlistbulk` (type, logical/on-disk sizes, `UF_COMPRESSED` and
`ATTR_CMNEXT_PRIVATESIZE` per entry in one syscall) instead of per-file `lstat` + `getattrlist`. Timings before that
change (debug build, 2026-11-24, machine under load average 40+ from other builds, so pessimistic):
`overview --no-measure` 2.7 s (the `uv-cache` venv scan under `~/dev`; `docker system df` is skipped),
`measure ~/.bun/install` (304k files) 9 s, full `overview` 42-44 s (the 4 workers per target and one thread per target
were kernel-bound: `sys` time is 5-10x `user`). The app still serves the cached overview and measures in the
background.

## Tauri commands (`src-tauri`)

Invoke with `invoke("<name>", { ...args })` (argument keys camelCase). Payloads are the types above. Every command
returns `Result<T, AppError>`; a rejected promise carries `AppError = { code: string, message: string }` (`message` is an
English log string, the UI translates by `code`).

Codes: `engine_missing` (auto-compress-apfs could not be started), `target_blocked`, `clone_shared`, `config_io`
(engine config unreadable/unwritable), `run_in_progress`, `run_failed`, `schedule_failed`, `not_found`, `internal`.

| Command | Args | Returns | Notes |
|---|---|---|---|
| `get_overview` | - | `Overview` | Cached overview (`<app data>/overview.json`) with current free space and `measurement` recomputed (`fresh` <= 6 h, `stale`, `measuring`, `never`). No cache yet: a sizeless overview (see `never`), not cached; takes about 3 s because of the venv risk scan. |
| `refresh_overview` | - | `()` | Starts one background measurement (a call while one runs is a no-op). Events below. |
| `set_target_enabled` | `{ id, enabled }` | `Target` | Edits only top-level `patterns`. `enabled: true` is refused with `clone_shared` (risk `clone_shared`) or `target_blocked` (any other `allowed: false`); disabling is always allowed. `not_found` for an unknown id, `run_in_progress` during a run. The cached overview is updated too. |
| `preview` | - | `Preview` | Runs the engine without `--apply`; takes seconds. |
| `apply` | - | `RunResult` | Resolves when the run ends (so the promise also carries the result), emitting events meanwhile. Refuses with `run_in_progress`, or `target_blocked`/`clone_shared` while an enabled target is not allowed. Cancelled: rejects with `run_failed` / `"cancelled"`. Every `RunResult` (including cancelled ones, with `"cancelled"` in `errors`) is appended to `<app data>/runs.json` (last 50). |
| `cancel_run` | - | `()` | SIGINT to the engine (its swaps are atomic). `not_found` when no run is active. |
| `list_runs` | - | `RunResult[]` | Newest first. |
| `get_schedule` | - | `ScheduleAgent[]` | |
| `install_schedule` / `uninstall_schedule` | - | `ScheduleAgent[]` | `auto-compress-apfs install-launchd` / `uninstall-launchd --config <engine config>`; failure is `schedule_failed`. |
| `reveal_in_finder` | `{ path }` | `()` | opener plugin; `not_found` when the path does not exist. |

### Events (`listen("<name>", cb)`)

| Event | Payload |
|---|---|
| `overview://measuring` | `{}` when a measurement starts |
| `overview://updated` | `Overview` when it finishes (already cached) |
| `overview://failed` | `{ code, message }` when it fails (the `measuring` state is over) |
| `run://progress` | `{ phase: "starting" \| "compressing" \| "verifying" \| "done" \| "failed", files: number \| null, totalFiles: number \| null, bytes: number \| null, currentPath: string \| null }` |
| `run://finished` | `RunResult`, emitted before the `apply` promise settles |
| `tray://compress-now` | `{}` when the tray menu item "Comprimir agora" is clicked; the main window is shown, nothing is compressed: the UI must ask for confirmation, then call `apply` |

`run://progress` is parsed from the engine log: `starting` before the preview and on the engine's `start` line,
`compressing` per `compressed <path> <orig> -> <disk>` line (`files`/`bytes` count originals processed so far,
`totalFiles` is the preview's file count, `currentPath` the last file; throttled to 10/s), `verifying` only when the
engine logs a line mentioning verification (best effort, may never appear), `done`/`failed` at exit (`failed` also for a
non-zero exit or a cancel). `files`/`bytes` are `null` until the first file.

### Windows and tray

- `main` (1180x760, min 960x640, overlay title bar). Closing it hides it (the app keeps running in the menu bar and leaves
  the Dock); reopen from the tray or the Dock.
- `tray` (360x480, borderless, always on top, hidden on blur) is created on the first left click on the menu bar icon,
  positioned under it, and loads `index.html#/tray`. The UI must render the compact view for that route.
- Menu bar item: template glyph drawn at runtime, title = free space of the data volume in GiB labelled `GB` (`75 GB`),
  refreshed every 60 s and after `overview://updated` and `run://finished`. Right click: "Abrir Disk Steward",
  "Comprimir agora" (emits `tray://compress-now`), "Sair" (English fallback when the system language is not Portuguese).
