# Installing

Releases carry rnode-nomu for 64-bit Linux: a Debian package for arm64, and tarballs for arm64 and
x86-64. The [quick start](../README.md#quick-start) installs the Debian package on the uConsole. On
other systems, or to run your own changes, [build it from source](#building-from-source).

## Installing a release

[Releases](https://github.com/arader/rnode-nomu/releases) carry the daemon built for 64-bit Linux:

| File | Contents |
| --- | --- |
| `rnode-nomu_arm64.deb` | Debian package for arm64: 64-bit Raspberry Pi OS, as on the uConsole CM5, or Debian 12 or newer. |
| `rnode-nomu-aarch64-unknown-linux-gnu.tar.gz` | The daemon for arm64 Linux, with the service unit, the example configs, the documentation and the licenses. |
| `rnode-nomu-x86_64-unknown-linux-gnu.tar.gz` | The same for x86-64 Linux. |

The binaries need glibc 2.35 or newer, as in Debian 12 and Ubuntu 22.04. The links below fetch the
latest release; [Releases and versions](releases.md) explains the version numbers.

### Debian package

```sh
curl -fL -o /tmp/rnode-nomu_arm64.deb \
  https://github.com/arader/rnode-nomu/releases/latest/download/rnode-nomu_arm64.deb
sudo apt install /tmp/rnode-nomu_arm64.deb
```

The package goes to `/tmp` because apt reads it as its `_apt` user, which cannot read most home
directories. From a home directory the install works too, but apt notes that it read the file
"unsandboxed as root".

The package installs:

- the daemon, as `/usr/bin/rnode-nomu`;
- its service, `/usr/lib/systemd/system/rnode-nomu.service`, which is
  [`contrib/rnode-nomu.service`](../contrib/rnode-nomu.service) starting `/usr/bin/rnode-nomu`. The
  package neither enables nor starts it, because the daemon needs `/etc/rnode-nomu.toml` first;
- the example configs, in `/usr/share/doc/rnode-nomu/examples/`;
- the `rnode` account, with `systemd-sysusers`. On its first install, the package adds the account
  to the groups `spi`, `gpio` and `dialout`, where they exist.

Install a config as `/etc/rnode-nomu.toml` before you enable the service: the
[quick start](../README.md#quick-start) installs the example for the AIO V2, and
[Setting up the LoRa hardware](hardware.md) explains the others and how to write your own.

To update, install the newer package the same way. If the daemon is running, the package restarts
it, and the restart first finishes the packet on the air
([How long a stop takes](running.md#how-long-a-stop-takes)). Drop-ins in
`/etc/systemd/system/rnode-nomu.service.d/` stay, as in
[Keeping local changes across updates](running.md#keeping-local-changes-across-updates).
`sudo apt remove rnode-nomu` stops the daemon and removes the package; `/etc/rnode-nomu.toml`, the
saved settings in `/var/lib/rnode` and the `rnode` account stay, even with `apt purge`.

A binary and unit installed from source, as in [Building from source](#building-from-source), take
precedence over the package's: the unit in `/etc/systemd/system` overrides the package's, and
`/usr/local/bin` comes first in `PATH`. To switch to the package, remove them first:

```sh
sudo systemctl disable --now rnode-nomu
sudo rm /usr/local/bin/rnode-nomu /etc/systemd/system/rnode-nomu.service
sudo apt install /tmp/rnode-nomu_arm64.deb
sudo systemctl enable --now rnode-nomu
```

### Tarballs

```sh
curl -fLO https://github.com/arader/rnode-nomu/releases/latest/download/rnode-nomu-aarch64-unknown-linux-gnu.tar.gz
tar -xzf rnode-nomu-aarch64-unknown-linux-gnu.tar.gz
cd rnode-nomu-aarch64-unknown-linux-gnu
sudo install -m 0755 rnode-nomu /usr/local/bin/rnode-nomu
```

The directory holds the config examples, `contrib/rnode-nomu.service` and the documentation at the
same paths as the repository, so the commands of [Building from source](#building-from-source) that
install the config and the unit and create the account work in it unchanged.
`THIRD-PARTY-LICENSES.md` lists the licenses of the Rust crates compiled into the binary.

## Building from source

You need Rust 1.85 or newer (the workspace uses edition 2024). Releases also come prebuilt: see
[Installing a release](#installing-a-release).

```sh
cargo build --release --locked -p rnode-nomu
sudo install -m 0755 target/release/rnode-nomu /usr/local/bin/rnode-nomu
```

The `sx1262` driver needs Linux: it uses spidev and the GPIO character device
(`/dev/gpiochipN`). On other systems the daemon builds and runs, but only with the `sim` driver. The
reference uConsole builds the daemon natively.

To set up the uConsole from source, replace step 2 of the [quick start](../README.md#quick-start)
with these commands. They install Rust from [rustup](https://rustup.rs) (skip the `curl` and `.`
lines if you already have Rust 1.85 or newer), build and install the binary, the config for the
AIO V2 and the service unit, and create the `rnode` account that the package would create:

```sh
sudo apt install -y git curl build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
git clone https://github.com/arader/rnode-nomu.git
cd rnode-nomu
cargo build --release --locked -p rnode-nomu
sudo install -m 0755 target/release/rnode-nomu /usr/local/bin/rnode-nomu
sudo install -m 0644 config.uconsole-aio-v2.toml /etc/rnode-nomu.toml
sudo install -m 0644 contrib/rnode-nomu.service /etc/systemd/system/rnode-nomu.service
sudo useradd --system --no-create-home --shell /usr/sbin/nologin rnode
sudo usermod -aG spi,gpio,dialout rnode
```

[Keeping local changes across updates](running.md#keeping-local-changes-across-updates) shows how
to update an installation from source.
