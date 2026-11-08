# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.1](https://github.com/arader/rnode-nomu/compare/rnode-nomu-v0.3.0...rnode-nomu-v0.3.1) - 2026-11-08

### Changed

- Split the README into guides in docs/; the quick start installs the release ([#7](https://github.com/arader/rnode-nomu/pull/7))

## [0.3.0](https://github.com/arader/rnode-nomu/releases/tag/rnode-nomu-v0.3.0)

The first release. Until November 2026 the project was developed as rnode-linux 0.2.0 on a private Forgejo instance; this version is that code, renamed and moved to GitHub.

### Added

- Releases, with the daemon built for arm64 and x86-64 Linux and a Debian package for arm64 ([#6](https://github.com/arader/rnode-nomu/pull/6)).
- A config and setup notes for the HackerGadgets AIO V1 ([#4](https://github.com/arader/rnode-nomu/pull/4)).

### Changed

- Renamed from rnode-linux: the binary, the systemd unit and `/etc/rnode-nomu.toml` are called rnode-nomu now, while the PTY symlink, `/run/rnode` and `/var/lib/rnode` keep their paths. [#1](https://github.com/arader/rnode-nomu/pull/1) lists the steps to migrate an installation.
