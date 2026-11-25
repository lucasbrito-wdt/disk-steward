# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Measurement walks directories with `getattrlistbulk` (one syscall for names, sizes and
  `ATTR_CMNEXT_PRIVATESIZE`) instead of per-file `lstat` + `getattrlist`.

## [0.1.0] - 2026-11-24

First public release.

### Added

- Menu-bar app and `steward` CLI that control APFS transparent compression through the `auto-compress-apfs` engine.
- Three sizes per folder: logical, on disk and private (`ATTR_CMNEXT_PRIVATESIZE`).
- Clone safety: targets are refused when more than `max(64 MiB, 10%)` of their on-disk bytes are shared with clones.
- Built-in catalog: `~/.rustup`, `~/.cargo/registry`, `~/.npm`, mise, the uv cache (excluding ephemeral `uvx`
  environments) and the pnpm store, plus custom paths. The bun store and Rust `target` directories are blocked by rule.
- Preview before apply, live progress, cancel, and run history.
- Daily schedule through the engine's LaunchAgent.
- Docker usage view. Docker data is never compressed.
- Interface in Portuguese (Brazil) and English, light and dark theme.
- `scripts/install.sh` and `scripts/uninstall.sh`. There are no prebuilt releases yet.

[Unreleased]: https://github.com/lucasbrito-wdt/disk-steward/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/lucasbrito-wdt/disk-steward/releases/tag/v0.1.0
