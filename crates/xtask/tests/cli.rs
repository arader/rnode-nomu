//! `cargo xtask` as CI runs it, through the built binary: no task, or a task xtask does not know,
//! exits 1, so that a mistyped task in a workflow fails the job; `-h`, `--help` and a known task
//! exit 0. Only arguments that never build, fetch or start anything are used.

use std::process::{Command, Output};

/// Runs the xtask binary with `args`.
fn xtask(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args(args)
        .output()
        .expect("run xtask")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn no_task_or_an_unknown_task_exits_1_with_the_usage() {
    let none = xtask(&[]);
    assert_eq!(none.status.code(), Some(1), "{none:?}");
    assert!(
        text(&none.stderr).starts_with("xtask: usage: cargo xtask rns-e2e "),
        "{none:?}"
    );

    let unknown = xtask(&["rns_e2e", "--scenario", "sf7"]);
    assert_eq!(unknown.status.code(), Some(1), "{unknown:?}");
    assert!(
        text(&unknown.stderr).starts_with("xtask: unknown task \"rns_e2e\"\nusage: cargo xtask rns-e2e "),
        "{unknown:?}"
    );
    assert!(unknown.stdout.is_empty(), "{unknown:?}");
}

#[test]
fn help_and_rns_e2e_exit_0_and_a_bad_rns_e2e_argument_exits_1() {
    for args in [&["-h"][..], &["--help"], &["rns-e2e", "--help"]] {
        let out = xtask(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(
            text(&out.stdout).starts_with("usage: cargo xtask rns-e2e "),
            "{args:?}: {out:?}"
        );
        assert!(out.stderr.is_empty(), "{args:?}: {out:?}");
    }

    let bad = xtask(&["rns-e2e", "--scenario", "sf9"]);
    assert_eq!(bad.status.code(), Some(1), "{bad:?}");
    assert_eq!(
        text(&bad.stderr),
        "xtask: unknown scenario \"sf9\"; valid scenarios: sf7, sf11, rnodeconf\n"
    );
}
