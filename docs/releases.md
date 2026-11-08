# Releases and versions

rnode-nomu and its libraries each have their own version and changelog. The version that matters for
running rnode-nomu is the daemon's, which `rnode-nomu --version` prints:
[GitHub releases](https://github.com/arader/rnode-nomu/releases) are releases of the daemon, tagged
`rnode-nomu-v<version>`, with the notes of [its changelog](../crates/rnode-nomu/CHANGELOG.md) and
the files of [Installing a release](installing.md#installing-a-release). The libraries,
`rnode-core`, `rnode-sx126x` and `rnode-sim`, get tags of their own, `<crate>-v<version>`, and a
changelog in their directories, but no GitHub release.

Versions follow [semantic versioning](https://semver.org) the way Cargo applies it before 1.0: a
breaking change raises the minor version, from 0.3.1 to 0.4.0, and any other change the patch
version, from 0.3.0 to 0.3.1. A change to the daemon is breaking when a setup that worked needs
changing after the update: a config key, a command line flag or a default that changes meaning or
goes away, saved settings or an EEPROM file that the new version reads differently, or a path or
unit setting that moves. A change to a library is breaking when it can break code that uses it.

The crates are not on crates.io. To use a library in another project, depend on its tag:

```toml ignore
[dependencies]
rnode-core = { git = "https://github.com/arader/rnode-nomu", tag = "rnode-core-v0.2.0" }
```

[release-plz](https://release-plz.dev) makes the releases
([`release-plz.toml`](../release-plz.toml)). When a merge to `main` changes a crate, it opens or
updates a pull request with the next versions and changelog entries, and merging that pull request
tags the crates and publishes the daemon's release, with binaries and the Debian package built by
[`release.yml`](../.github/workflows/release.yml). Changes outside the crates, such as to the
example configs, ship with the daemon's next release. [AGENTS.md](../AGENTS.md) has the rules for
pull requests.
