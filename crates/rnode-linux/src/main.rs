use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use clap::Parser;
use log::{info, warn};
use rnode_linux::config::{Config, RadioDriver};

#[derive(Parser, Debug)]
#[command(name = "rnode-linux", about = "RNode Linux daemon")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Record SPI transactions to a CSV file.
    #[arg(long)]
    spilog: Option<PathBuf>,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fatal: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config).with_context(|| format!("load {}", cli.config.display()))?;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&config.logging.log_level)).init();
    info!("loaded config from {}", cli.config.display());
    for warning in config.warnings() {
        warn!("{warning}");
    }
    let seed = seed_from_urandom();
    run_daemon(config, cli.spilog, seed)
}

fn seed_from_urandom() -> u64 {
    let mut bytes = [0_u8; 8];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok()
    {
        return u64::from_le_bytes(bytes);
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn run_daemon(config: Config, _spilog: Option<PathBuf>, seed: u64) -> anyhow::Result<()> {
    let shutdown = install_signal_flag()?;
    match config.radio.driver {
        RadioDriver::Sim => run_sim(config, seed, &shutdown),
        RadioDriver::Sx1262 => Err(anyhow::anyhow!("sx1262 hardware daemon runs only on Linux")),
    }
}

#[cfg(target_os = "linux")]
fn run_daemon(config: Config, spilog: Option<PathBuf>, seed: u64) -> anyhow::Result<()> {
    let shutdown = install_signal_flag()?;
    match config.radio.driver {
        RadioDriver::Sx1262 => run_sx1262(config, spilog, seed, &shutdown),
        RadioDriver::Sim => run_sim(config, seed, &shutdown),
    }
}

fn run_sim(config: Config, seed: u64, shutdown: &std::sync::atomic::AtomicBool) -> anyhow::Result<()> {
    use rnode_core::Node;
    use rnode_linux::event_loop::{FdIrqSource, run};
    use rnode_linux::storage::TomlStorage;
    use rnode_sim::{SimConfig, SimRadio};
    use std::os::fd::BorrowedFd;

    let sim_config = SimConfig {
        air_dir: config.radio.sim.air_dir.clone(),
        node_name: config.radio.sim.node_name.clone(),
        rssi_dbm: config.radio.sim.rssi_dbm,
        snr_db: config.radio.sim.snr_db,
        loss: config.radio.sim.loss,
        seed,
    };
    let radio = SimRadio::new(sim_config).context("open simulated radio")?;
    let irq_fd = BorrowedFd::try_clone_to_owned(&radio.irq_fd()).context("clone simulated IRQ fd")?;
    let irq = FdIrqSource::new(irq_fd);
    let pty = open_pty(&config)?;
    let storage = TomlStorage::new(config.storage.radio_settings.clone());
    let node = Node::new(radio, storage, config.node_config(), seed);
    run(node, irq, pty, shutdown).map_err(Into::into)
}

#[cfg(target_os = "linux")]
fn run_sx1262(
    config: Config,
    spilog: Option<PathBuf>,
    seed: u64,
    shutdown: &std::sync::atomic::AtomicBool,
) -> anyhow::Result<()> {
    use rnode_core::Node;
    use rnode_linux::delay::StdDelay;
    use rnode_linux::event_loop::run;
    use rnode_linux::gpio::RadioGpio;
    use rnode_linux::spi::{LinuxSpiDevice, SpiRecorder};
    use rnode_linux::storage::TomlStorage;
    use rnode_sx126x::Sx126x;

    let spi_config = config
        .spi
        .as_ref()
        .context("[spi] is required when radio.driver = \"sx1262\"")?;
    let gpio_config = config
        .gpio
        .as_ref()
        .context("[gpio] is required when radio.driver = \"sx1262\"")?;
    let radio_config = config.radio.sx1262.driver_config().context("radio.sx1262")?;

    let recorder = spilog.map(SpiRecorder::create).transpose().context("open SPI log")?;
    let spi = LinuxSpiDevice::open(&spi_config.device, spi_config.speed_hz, recorder)
        .with_context(|| format!("open SPI device {}", spi_config.device.display()))?;
    let gpio = RadioGpio::open(gpio_config).context("open GPIO lines")?;
    let pty = open_pty(&config)?;

    let _power = gpio.power;
    let radio = Sx126x::new(spi, gpio.busy, gpio.reset, gpio.rxen, StdDelay, radio_config);
    let storage = TomlStorage::new(config.storage.radio_settings.clone());
    let node = Node::new(radio, storage, config.node_config(), seed);
    run(node, gpio.dio1, pty, shutdown).map_err(Into::into)
}

fn open_pty(config: &Config) -> anyhow::Result<rnode_linux::pty::PtyPair> {
    let permissions = config.pty.slave_permissions()?;
    let pty = rnode_linux::pty::PtyPair::open_with_permissions(config.pty.symlink.as_deref(), permissions)
        .context("open PTY")?;
    info!("PTY slave: {}", pty.slave_path().display());
    Ok(pty)
}

fn install_signal_flag() -> anyhow::Result<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, std::sync::Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, std::sync::Arc::clone(&shutdown))?;
    Ok(shutdown)
}
