# AGENTS.md

Guidance for anyone (human or agent) changing this repository.

## What this is

rnode-linux is a Linux daemon that turns a LoRa modem on a host's SPI bus into an RNode: it
exposes a PTY speaking the RNode KISS host protocol, so Reticulum's `RNodeInterface` (and other
RNode hosts) can use it exactly like an RNode_Firmware device. On the air it must interoperate
with real RNodes.

## Layout

- `crates/rnode-core` — hardware-independent RNode logic (KISS host protocol, TX queue, split
  framing, PHY/CSMA, the `Node` state machine) plus the `Radio` and `Storage` traits. No I/O;
  time is passed in as `now_ms` and randomness comes from a seed, so tests are deterministic.
- `crates/rnode-linux` — the daemon on top of `rnode-core` (config, GPIO, SPI, PTY, saved
  settings, event loop).
- Modem drivers implement `rnode_core::Radio` in their own crates: `crates/rnode-sx126x` for the
  SX1262, `crates/rnode-sim` for the simulated radio. DRIVERS.md explains the contract.
- `crates/xtask` — repository tasks: `cargo xtask rns-e2e` runs Reticulum against two simulated
  daemons. It needs Reticulum and `uv`, so it runs by hand, not in CI; `tests/e2e/kiss_probe.py`
  is its KISS probe.
- `tools/hwtest` — hardware bring-up and SDR analysis for the reference board, outside the
  workspace.

Every crate's tests follow the Test layout section below.

## Reference implementations

Behaviour follows RNode_Firmware v1.86 (`github.com/markqvist/RNode_Firmware` at `56775c5`,
expected in a sibling checkout `../RNode_Firmware`) and must work with Reticulum's
`RNS/Interfaces/RNodeInterface.py` (`../Reticulum`). When behaviour is observable by the host or
on the air, match the firmware and cite the function you followed (e.g. `sx126x::dcd()`). Where
we deliberately differ, say so in a doc comment.

## Rules

- CI's `PR / test` runs `cargo fmt --all --check`, `cargo build --locked --all-targets`,
  `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked`; all four must
  pass (this is the CI gate), so run the same commands locally before pushing.
- Commit `Cargo.lock` whenever dependencies change. The CI runner has little memory: prefer small
  dependencies.
- Every behaviour change comes with tests. Prefer golden values derived from the firmware over
  round-trip tests against our own code (a self-consistent but incompatible implementation is how
  the split-framing bug, #3, went unnoticed).
- New code is `unsafe`-free (workspace lint `unsafe_code = "forbid"`).
- Update README.md and DRIVERS.md with behaviour they describe (config keys, CLI flags, log
  formats, the `Radio` contract). `crates/rnode-linux/tests/docs_examples.rs` loads their config
  examples with the real parser and checks their links.
- Never commit `.forgejo/token` or other secrets.

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

- `main` only changes through pull requests on Forgejo (`infra/rnode-linux`), squash-merged.
- CI runs `PR / test`, then the `agent-actions` reviewers (pr-agent, security-agent, qa-agent);
  findings at **high** or above block. Reply to every review comment with what you are doing
  about it, then push the fix.
- Reference issues in PRs (`Fixes #n`); open an issue for every bug you find that is out of
  scope for your change.

## Hardware

The reference board is the uConsole (CM5) with the HackerGadgets AIO V2: SX1262 on
`/dev/spidev1.0`, GPIO character device `gpiochip15` (pinctrl-rp1) lines 25 = NRESET,
24 = BUSY, 26 = DIO1 (IRQ); 1.8 V TCXO driven by DIO3; DIO2 controls the RF switch; line 16
enables the LoRa power rail, which the reference unit's `config.txt` turns off at boot.
