//! Switching the modem off when the daemon exits (`[gpio] power_off_on_exit`).

use embedded_hal::digital::OutputPin;

/// The modem's power-enable line (`[gpio] power`), held while the daemon runs.
///
/// With `power_off_on_exit`, dropping the guard switches the modem off in a fixed order: it drives
/// the lines added with [`PowerGuard::lower_on_power_off`] low, in the order they were added, and
/// only then the power line. The daemon adds NRESET, then RXEN, then TXEN. A line left high would
/// hold an unpowered modem's input above the SX1261/2's absolute maximum rating (VDD + 0.3 V on
/// digital pins), and could feed the modem through its input protection (#86). Raspberry Pi kernels
/// keep a released output at its level, so the level the guard leaves is the level the line keeps
/// after the daemon exits.
///
/// The guard owns the lines it lowers until it drops; the daemon gives it clones of the lines the
/// radio driver drives (`gpio::RadioGpio::open()`). So the sequence is the same whatever is dropped
/// first: after a normal stop, once the node has put the modem to sleep and been dropped; after an
/// error; and on an early return before the radio exists. Unwinding from a panic drops the guard
/// too, but nothing runs when the process is killed with SIGKILL.
///
/// Without `power_off_on_exit`, dropping the guard changes nothing: the rail stays on and the lines
/// stay as they are, so a restart does not power-cycle the modem.
#[derive(Debug)]
pub struct PowerGuard<P: OutputPin> {
    power: P,
    power_off_on_exit: bool,
    /// The lines to drive low before the power line, each with its `[gpio]` key for warnings.
    lines: Vec<(&'static str, P)>,
}

impl<P: OutputPin> PowerGuard<P> {
    /// Guard `power`, the power-enable line, which the caller has driven high. `power_off_on_exit`
    /// is the `[gpio]` setting of that name.
    pub fn new(power: P, power_off_on_exit: bool) -> Self {
        Self {
            power,
            power_off_on_exit,
            lines: Vec::new(),
        }
    }

    /// Keep `line`, a line into the modem, until the guard drops, and drive it low before the power
    /// line when the guard switches the modem off. `name` is its `[gpio]` key, for warnings.
    pub fn lower_on_power_off(&mut self, name: &'static str, line: P) {
        self.lines.push((name, line));
    }
}

impl<P: OutputPin> Drop for PowerGuard<P> {
    /// Each step is best effort: a line that cannot be driven low is logged, and the rail is still
    /// switched off as configured.
    fn drop(&mut self) {
        if !self.power_off_on_exit {
            return;
        }
        for (name, line) in &mut self.lines {
            if let Err(e) = line.set_low() {
                log::warn!("power_off_on_exit: failed to drive [gpio] {name} low: {e:?}");
            }
        }
        if let Err(e) = self.power.set_low() {
            log::warn!("power_off_on_exit: failed to drive [gpio] power low: {e:?}");
        }
    }
}
