//! GPIO chip lookup. Linux only, like the `gpio` module itself.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
#[test]
fn gpio_resolve_chip_handles_paths_and_names() {
    assert_eq!(
        rnode_nomu::gpio::resolve_chip("/dev/gpiochip15").unwrap(),
        PathBuf::from("/dev/gpiochip15")
    );
    assert_eq!(
        rnode_nomu::gpio::resolve_chip("gpiochip15").unwrap(),
        PathBuf::from("/dev/gpiochip15")
    );
    assert!(matches!(
        rnode_nomu::gpio::resolve_chip("definitely-not-a-gpio-label"),
        Err(rnode_nomu::gpio::GpioError::ChipNotFound { .. })
    ));
}

/// Issue #55: a chip whose label cannot be read is skipped instead of ending the scan, and named
/// in the error if no chip has the label.
#[cfg(target_os = "linux")]
#[test]
fn gpio_chip_scan_skips_chips_whose_label_cannot_be_read() {
    use rnode_nomu::gpio::{GpioError, UnreadableChip, find_chip_by_label};

    let chips = ["/dev/gpiochip0", "/dev/gpiochip1", "/dev/gpiochip2", "/dev/gpiochip3"].map(PathBuf::from);
    let read = std::cell::RefCell::new(Vec::new());
    let labels = |path: &Path| {
        read.borrow_mut().push(path.to_owned());
        match path.to_str().unwrap() {
            "/dev/gpiochip0" => Ok("gpio-brcmstb@107d508500".to_owned()),
            "/dev/gpiochip1" => Err("Permission denied (os error 13)"),
            "/dev/gpiochip2" => Ok("pinctrl-rp1".to_owned()),
            _ => Err("No such device (os error 19)"),
        }
    };
    assert_eq!(
        find_chip_by_label("pinctrl-rp1", chips.clone(), labels).unwrap(),
        PathBuf::from("/dev/gpiochip2")
    );
    assert_eq!(
        *read.borrow(),
        chips[..3],
        "the scan goes past gpiochip1 and stops at the match"
    );

    let err = find_chip_by_label("pinctrl-rp2", chips, labels).unwrap_err();
    match &err {
        GpioError::ChipNotFound { label, unreadable } => {
            assert_eq!(label, "pinctrl-rp2");
            assert_eq!(
                unreadable,
                &[
                    UnreadableChip {
                        path: PathBuf::from("/dev/gpiochip1"),
                        error: "Permission denied (os error 13)".to_owned(),
                    },
                    UnreadableChip {
                        path: PathBuf::from("/dev/gpiochip3"),
                        error: "No such device (os error 19)".to_owned(),
                    },
                ]
            );
        }
        other => panic!("unexpected error {other}"),
    }
    assert_eq!(
        err.to_string(),
        "gpio chip \"pinctrl-rp2\" not found; could not read the label of /dev/gpiochip1 \
         (Permission denied (os error 13)), /dev/gpiochip3 (No such device (os error 19))"
    );
    assert_eq!(
        find_chip_by_label("pinctrl-rp1", Vec::new(), labels)
            .unwrap_err()
            .to_string(),
        "gpio chip \"pinctrl-rp1\" not found"
    );
}
