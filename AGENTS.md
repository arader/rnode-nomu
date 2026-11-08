# AGENTS.md

Guidance for anyone (human or agent) changing this repository.

## What this is

rnode-nomu is a Linux daemon that turns a LoRa modem on a host's SPI bus into an RNode: it
exposes a PTY speaking the RNode KISS host protocol, so Reticulum's `RNodeInterface` (and other
RNode hosts) can use it exactly like an RNode_Firmware device. On the air it must interoperate
with real RNodes.

## Layout

- `crates/rnode-core` — hardware-independent RNode logic (KISS host protocol, TX queue, split
  framing, PHY/CSMA, the `Node` state machine) plus the `Radio` and `Storage` traits. No I/O;
  time is passed in as `now_ms` and randomness comes from a seed, so tests are deterministic.
- `crates/rnode-nomu` — the daemon on top of `rnode-core` (config, GPIO, SPI, PTY, saved
  settings, event loop).
- Modem drivers implement `rnode_core::Radio` in their own crates: `crates/rnode-sx126x` for the
  SX1262, `crates/rnode-sim` for the simulated radio. `docs/drivers.md` explains the contract.
- `crates/xtask` — repository tasks: `cargo xtask rns-e2e` runs Reticulum against two simulated
  daemons, and Reticulum's `rnodeconf` against simulated daemons, with the Reticulum commit, uv
  release and Python packages pinned (crates/xtask/README.md says where);
  `tests/e2e/kiss_probe.py` is its KISS probe.
- `tools/hwtest` — hardware bring-up, SDR analysis and an over-the-air LXMF echo test against a
  real RNode (`lxmf_echo_ota.py`) for the reference board, outside the workspace.
- `README.md` — the summary and the quick start; `docs/` — the guides it links, for users
  (installing, hardware, configuration, running, Reticulum, logging, troubleshooting) and for
  developers (how it works, development, drivers, releases).

Every crate's tests follow the Test layout section below.

## Reference implementations

Behaviour follows RNode_Firmware v1.86 (`github.com/markqvist/RNode_Firmware` at `56775c5`,
expected in a sibling checkout `../RNode_Firmware`) and must work with Reticulum's
`RNS/Interfaces/RNodeInterface.py` (`../Reticulum`). When behaviour is observable by the host or
on the air, match the firmware and cite the function you followed (e.g. `sx126x::dcd()`). Where
we deliberately differ, say so in a doc comment.

## Rules

- CI's `PR / test` runs `cargo fmt --all --check`, `cargo build --locked --all-targets`,
  `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, the Python unit
  tests of `tools/hwtest`, `python3 -m unittest discover -s tools/hwtest -p 'test_*.py' -v`, once
  with the standard library alone and once in a venv with the numpy that
  `tools/hwtest/requirements-test.txt` pins, the bring-up tool's unit tests,
  `cargo test --manifest-path tools/hwtest/sx126x-bringup/Cargo.toml --locked`, and the SF7
  scenario of the Reticulum end-to-end test,
  `cargo xtask rns-e2e --fetch-reticulum --scenario sf7`; all eight must pass (this is the CI
  gate). Run the four cargo commands locally before every push, the `tools/hwtest` unit tests
  when you change `tools/hwtest` (its README shows how; the bring-up tool's build only on Linux),
  and the end-to-end test (it needs `uv`) before pushing changes to rnode-core, rnode-nomu,
  rnode-sim or the harness (`crates/xtask`, `tests/e2e`). After each merge, CI on `main`
  (`.github/workflows/ci.yml`) runs the full suite: the same checks and every end-to-end
  scenario.
- Commit `Cargo.lock` whenever dependencies change. Prefer small dependencies.
- Every behaviour change comes with tests. Prefer golden values derived from the firmware over
  round-trip tests against our own code (a self-consistent but incompatible implementation is how
  the split-framing bug, #3, went unnoticed).
- New code is `unsafe`-free (workspace lint `unsafe_code = "forbid"`).
- Update the documentation with the behaviour it describes: config keys, CLI flags, log formats,
  the `Radio` contract (`docs/drivers.md`). README.md holds only the summary and the quick start;
  details go into a guide in `docs/`, and README.md links every guide.
  `crates/rnode-nomu/tests/docs_examples.rs` loads their config examples with the real parser and
  checks their links.
- Code translated from RNode_Firmware keeps its attribution: when you port from a file
  [NOTICE.md](NOTICE.md) does not list yet, add it there with its copyright and license.
- Never commit secrets: tokens, keys, private hostnames or addresses.

## Test layout

- Keep source files free of inline test bodies.
- Unit tests that need private access live in Rust's default test-module file: `src/tests.rs` for
  `lib.rs`, or `src/<module>/tests.rs` for `src/<module>.rs`; keep only `#[cfg(test)] mod tests;`
  in the source module.
- Large unit-test files (about 600+ lines) may be split as `src/tests/mod.rs` plus topic files.
- Tests that use only public API belong in `crates/<crate>/tests/<topic>.rs`.
- Put shared unit-test helpers in one `#[cfg(test)]` helper module per crate; integration helpers
  belong in `tests/common/mod.rs`.

## Workflow

- `main` only changes through pull requests on GitHub (`arader/rnode-nomu`), squash-merged.
- CI runs `PR / test`; it must pass before a merge. Reply to every review comment with what you
  are doing about it, then push the fix.
- Reference issues in PRs (`Fixes #n`); open an issue for every bug you find that is out of
  scope for your change.

## Versions and releases

- rnode-nomu and its libraries each have their own version in their `Cargo.toml` (xtask has none,
  and release-plz skips it); the root `Cargo.toml`'s `[workspace.dependencies]` holds the
  requirements of the crates that others depend on. No crate is on crates.io (`publish = false`).
  `docs/releases.md` is the user's view.
- release-plz (`release-plz.toml`, `.github/workflows/release.yml`) keeps a release PR open with the
  next versions and changelog entries. Merging it tags each released crate `<crate>-v<version>` and
  publishes rnode-nomu's GitHub release with the binaries and the Debian package. Merging any PR
  from a branch whose name starts with `release-plz-` releases, so use that prefix for nothing else.
- The squash-merged title of a PR is its changelog entry: write it for the people who run the
  daemon, in the imperative. Its first word picks the section: Add, Support or Implement for
  Added, Deprecate for Deprecated, Remove or Drop for Removed, Fix for Fixed, any other for Changed.
- release-plz raises the patch version of every crate changed since its tag. A PR with a breaking
  change (as `docs/releases.md` defines it) raises the minor version itself, of every crate it
  breaks, and the requirement in `[workspace.dependencies]` and `Cargo.lock` with it. A breaking
  change to rnode-core's API also breaks rnode-sx126x and rnode-sim, which implement its `Radio`
  trait. release-plz keeps a version that is already ahead of the crate's tag. A new version of
  rnode-core or rnode-sx126x also goes into the bring-up tool's lock file:
  `cargo update --manifest-path tools/hwtest/sx126x-bringup/Cargo.toml -p rnode-core -p rnode-sx126x`
  (the release workflow does this in the release PR).
- release-plz counts a commit for a crate when it changes a file of the crate's package
  (`cargo package --list -p <crate>`; for rnode-nomu also README.md), and for rnode-nomu every
  commit of the libraries it is built from. Changes outside `crates/`, such as the example configs,
  `contrib/` and `docs/`, release nothing by themselves but ship with rnode-nomu's next release; to
  release one sooner, raise rnode-nomu's patch version in the same PR. `crates/rnode-nomu/debian`
  has the Debian package's copy of `contrib/rnode-nomu.service`;
  `crates/rnode-nomu/tests/packaging.rs` keeps the two in step.
- Changelog entries become GitHub release notes, where line breaks show: when you edit a
  changelog, keep each paragraph or list item on one line.
- The release PR is opened with the `RELEASE_PLZ_TOKEN` secret, so that `PR / test` runs on it.
  `Release / binaries` builds the tarballs and the Debian package on PRs that change the packaging
  or rnode-nomu's changelog, as every release PR does (the `paths` in release.yml); it is not part
  of the CI gate, but a release needs it to pass.
- If a release fails halfway, re-run the failed jobs of its `Release` run; the GitHub release stays
  a draft until `publish` succeeds. If the `release` job failed after tagging rnode-nomu, a re-run
  skips that version: delete the tag (`git push origin --delete rnode-nomu-v<version>`) and its
  draft release, if any, first.

## Hardware

The reference board is the uConsole (CM5) with the HackerGadgets AIO V2: SX1262 on
`/dev/spidev1.0`, GPIO character device `gpiochip15` (pinctrl-rp1) lines 25 = NRESET,
24 = BUSY, 26 = DIO1 (IRQ); 1.8 V TCXO driven by DIO3; DIO2 controls the RF switch; line 16
enables the LoRa power rail, which the reference unit's `config.txt` turns off at boot.

The AIO V1 has the same radio without the power rail (`config.uconsole-aio-v1.toml`). It has not
been tested on hardware, so keep it in step with the V2 config; `tests/config.rs` checks that the
two differ only in the GPIO chip's name and the power rail.
