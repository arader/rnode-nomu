use super::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailAfterFirstWrite {
    writes: usize,
}

impl Write for FailAfterFirstWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.writes == 0 {
            self.writes += 1;
            Ok(buf.len())
        } else {
            Err(io::Error::other("forced trace failure"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("rnode-linux-spi-tests")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn hex_encoding_is_lowercase_and_dense() {
    assert_eq!(hex(&[0x00, 0x0a, 0xab, 0xff]), "000aabff");
}

#[test]
fn recorder_writes_header_and_lowercase_csv_rows() {
    let writer = SharedWriter::default();
    let output = Arc::clone(&writer.0);
    let mut recorder = SpiRecorder::from_writer(writer).unwrap();
    recorder.record("TXRX", &[0x0a, 0xbc], &[0x00, 0xff]).unwrap();

    let csv = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some("timestamp_us,direction,tx,rx"));
    let row = lines.next().unwrap();
    assert!(lines.next().is_none());
    let fields: Vec<_> = row.split(',').collect();
    assert_eq!(fields.len(), 4);
    assert!(fields[0].parse::<u128>().is_ok());
    assert_eq!(fields[1], "TXRX");
    assert_eq!(fields[2], "0abc");
    assert_eq!(fields[3], "00ff");
}

#[test]
fn recorder_warns_once_on_trace_write_failure() {
    let mut recorder = SpiRecorder::from_writer(FailAfterFirstWrite { writes: 0 }).unwrap();
    recorder.record("TXRX", &[1], &[2]).unwrap();
    recorder.record("TXRX", &[3], &[4]).unwrap();
    assert!(recorder.write_failed);
    assert_eq!(recorder.warned_failures_for_tests(), 1);
}

#[test]
fn recorder_create_on_unwritable_path_returns_error() {
    let dir = test_dir("directory-is-not-a-file");
    assert!(SpiRecorder::create(&dir).is_err());
}
