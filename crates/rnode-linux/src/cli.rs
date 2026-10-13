//! Support for the `rnode-linux` command line.

use std::error::Error;
use std::fmt;

/// An error and its sources, joined with `": "`, leaving out a source whose message already ends
/// the text: `open SPI device /dev/spidev9.9: spi io: No such file or directory (os error 2)`.
///
/// The daemon's error types include their source in their own message, so that a log line that
/// prints only `{e}` still names the cause, and also return it from [`Error::source`]. Printing
/// the whole chain, as anyhow's `{:#}` does, would then repeat each cause (#54).
#[derive(Debug, Clone, Copy)]
pub struct ErrorChain<'a>(pub &'a (dyn Error + 'static));

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut text = self.0.to_string();
        let mut source = self.0.source();
        while let Some(error) = source {
            let message = error.to_string();
            if !text.ends_with(&message) {
                text.push_str(": ");
                text.push_str(&message);
            }
            source = error.source();
        }
        f.write_str(&text)
    }
}
