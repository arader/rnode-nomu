#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("sx126x-bringup requires Linux spidev and GPIO chardev; build/run it on the uConsole target.");
    std::process::exit(1);
}
