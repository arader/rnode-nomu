//! GPIO chip lookup. Linux only, like the `gpio` module itself.

#![cfg(target_os = "linux")]

use std::path::PathBuf;

#[cfg(target_os = "linux")]
#[test]
fn gpio_resolve_chip_handles_paths_and_names() {
    assert_eq!(
        rnode_linux::gpio::resolve_chip("/dev/gpiochip15").unwrap(),
        PathBuf::from("/dev/gpiochip15")
    );
    assert_eq!(
        rnode_linux::gpio::resolve_chip("gpiochip15").unwrap(),
        PathBuf::from("/dev/gpiochip15")
    );
    assert!(matches!(
        rnode_linux::gpio::resolve_chip("definitely-not-a-gpio-label"),
        Err(rnode_linux::gpio::GpioError::ChipNotFound(_))
    ));
}
