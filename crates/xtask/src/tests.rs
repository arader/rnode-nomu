use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rnode_nomu::config::{Config, RadioDriver};

use super::{
    CLIENT_PY, Check, Children, KISS_BOUNDARY_PY, Options, RETICULUM_COMMIT, RETICULUM_REPO, SCENARIOS, SERVER_PY,
    Scenario, Task, create_private_dir, daemon_config, dispatch, exit_status, expect_output, fetch_reticulum, fnv1a64,
    parse_args, requirements_fingerprint, reticulum_pin_warning, scenario_outcome, usage, write_log_tails,
};

fn args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

/// The names of the scenarios `cargo xtask rns-e2e <args>` runs.
fn scenarios(args_: &[&str]) -> Vec<&'static str> {
    parse_args(&args(args_))
        .unwrap()
        .scenarios
        .iter()
        .map(|s| s.name)
        .collect()
}

#[test]
fn rns_e2e_runs_every_scenario_against_a_local_reticulum_by_default() {
    assert_eq!(
        parse_args(&[]).unwrap(),
        Options {
            reticulum: None,
            fetch_reticulum: false,
            keep_logs: false,
            scenarios: SCENARIOS.to_vec(),
            help: false,
        }
    );
    assert_eq!(scenarios(&[]), ["sf7", "sf11", "rnodeconf"]);
}

/// `--scenario` picks scenarios by name. They run once each, in the harness's order, however often
/// and in whatever order they were named.
#[test]
fn scenario_selects_the_named_scenarios() {
    assert_eq!(scenarios(&["--scenario", "sf7"]), ["sf7"]);
    assert_eq!(scenarios(&["--scenario", "sf11"]), ["sf11"]);
    assert_eq!(scenarios(&["--scenario", "sf11", "--scenario", "sf7"]), ["sf7", "sf11"]);
    assert_eq!(scenarios(&["--scenario", "sf7", "--scenario", "sf7"]), ["sf7"]);
    assert_eq!(scenarios(&["--scenario", "rnodeconf"]), ["rnodeconf"]);
    assert_eq!(
        scenarios(&["--scenario", "rnodeconf", "--scenario", "sf7"]),
        ["sf7", "rnodeconf"]
    );
    assert_eq!(
        scenarios(&["--keep-logs", "--scenario", "sf11", "--keep-logs"]),
        ["sf11"]
    );
}

#[test]
fn an_unknown_scenario_is_an_error_that_lists_the_valid_ones() {
    assert_eq!(
        parse_args(&args(&["--scenario", "sf9"])).unwrap_err(),
        "unknown scenario \"sf9\"; valid scenarios: sf7, sf11, rnodeconf"
    );
    assert_eq!(
        parse_args(&args(&["--scenario", "SF7"])).unwrap_err(),
        "unknown scenario \"SF7\"; valid scenarios: sf7, sf11, rnodeconf"
    );
    assert_eq!(
        parse_args(&args(&["--scenario", "sf7", "--scenario", ""])).unwrap_err(),
        "unknown scenario \"\"; valid scenarios: sf7, sf11, rnodeconf"
    );
    assert_eq!(
        parse_args(&args(&["--scenario"])).unwrap_err(),
        "--scenario requires a name"
    );
    assert!(usage().contains("(sf7, sf11, rnodeconf)"), "{}", usage());
}

#[test]
fn fetch_reticulum_replaces_the_local_checkout() {
    let options = parse_args(&args(&["--fetch-reticulum", "--scenario", "sf7"])).unwrap();
    assert!(options.fetch_reticulum);
    assert_eq!(options.reticulum, None);
    assert_eq!(scenarios(&["--fetch-reticulum", "--scenario", "sf7"]), ["sf7"]);

    let options = parse_args(&args(&["--reticulum", "../rns", "--keep-logs"])).unwrap();
    assert!(!options.fetch_reticulum && options.keep_logs);
    assert_eq!(options.reticulum.as_deref(), Some(Path::new("../rns")));

    for both in [
        ["--fetch-reticulum", "--reticulum", "../rns"],
        ["--reticulum", "../rns", "--fetch-reticulum"],
    ] {
        assert_eq!(
            parse_args(&args(&both)).unwrap_err(),
            "--fetch-reticulum and --reticulum exclude each other"
        );
    }
    assert_eq!(
        parse_args(&args(&["--reticulum"])).unwrap_err(),
        "--reticulum requires a path"
    );
}

/// Switches may be repeated; a second path is ambiguous, so it is refused.
#[test]
fn repeated_flags() {
    let options = parse_args(&args(&[
        "--fetch-reticulum",
        "--keep-logs",
        "--fetch-reticulum",
        "--keep-logs",
    ]))
    .unwrap();
    assert!(options.fetch_reticulum && options.keep_logs);
    assert_eq!(
        parse_args(&args(&["--reticulum", "a", "--reticulum", "a"])).unwrap_err(),
        "--reticulum given more than once"
    );
}

/// `cargo xtask` runs `rns-e2e`, and prints its usage for `-h` or `--help`; both exit 0. No task,
/// or one xtask does not know, is an error that shows the usage and exits 1: a mistyped task in a
/// workflow must fail the job, not pass without running anything as it used to.
#[test]
fn tasks_dispatch_and_a_missing_or_unknown_task_exits_1() {
    assert_eq!(dispatch(Some("rns-e2e")), Ok(Task::RnsE2e));
    assert_eq!(dispatch(Some("-h")), Ok(Task::Help));
    assert_eq!(dispatch(Some("--help")), Ok(Task::Help));
    assert_eq!(dispatch(None), Err(usage()));
    assert_eq!(
        dispatch(Some("rns_e2e")),
        Err(format!("unknown task \"rns_e2e\"\n{}", usage()))
    );
    assert!(dispatch(Some("")).is_err());
    assert!(dispatch(Some("RNS-E2E")).is_err());

    for (task, status) in [
        (Some("rns-e2e"), 0),
        (Some("-h"), 0),
        (Some("--help"), 0),
        (None, 1),
        (Some("rns_e2e"), 1),
        (Some("bogus"), 1),
    ] {
        assert_eq!(exit_status(&dispatch(task)), status, "{task:?}");
    }
}

/// A scenario fails when its checks fail, when its processes do not shut down cleanly, or both,
/// and then the error names both failures.
#[test]
fn a_scenario_fails_when_its_checks_or_its_shutdown_fail() {
    let failed = |message: &str| -> Result<(), Box<dyn std::error::Error>> { Err(message.into()) };
    let outcome = |checks, shutdown| scenario_outcome(checks, shutdown).map_err(|e| e.to_string());
    assert_eq!(outcome(Ok(()), Ok(())), Ok(()));
    assert_eq!(
        outcome(failed("RNS client failed with exit status: 1"), Ok(())),
        Err("RNS client failed with exit status: 1".to_owned())
    );
    assert_eq!(
        outcome(Ok(()), failed("daemon-a exited with exit status: 1")),
        Err("daemon-a exited with exit status: 1".to_owned())
    );
    assert_eq!(
        outcome(
            failed("KISS boundary probe timed out after 60s"),
            failed("daemon-b did not exit after SIGTERM; killed with signal: 9 (SIGKILL)")
        ),
        Err("KISS boundary probe timed out after 60s; then, on shutdown: \
             daemon-b did not exit after SIGTERM; killed with signal: 9 (SIGKILL)"
            .to_owned())
    );
}

#[test]
fn unknown_arguments_are_errors_and_help_is_not() {
    for unknown in [&["--scenarios", "sf7"][..], &["sf7"], &["--fetch-reticulum=yes"]] {
        let error = parse_args(&args(unknown)).unwrap_err();
        assert!(
            error.starts_with(&format!("unknown rns-e2e argument {:?}\nusage: ", unknown[0])),
            "{error}"
        );
    }
    assert!(parse_args(&args(&["--help"])).unwrap().help);
    assert!(parse_args(&args(&["-h"])).unwrap().help);
}

/// The pin `--fetch-reticulum` checks HEAD against must be a full commit hash: `git rev-parse HEAD`
/// prints 40 lowercase hex digits, and a shorter pin would never match it.
#[test]
fn the_reticulum_pin_is_a_full_commit_hash() {
    assert_eq!(RETICULUM_COMMIT.len(), 40, "{RETICULUM_COMMIT}");
    assert!(
        RETICULUM_COMMIT
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{RETICULUM_COMMIT}"
    );
    assert!(RETICULUM_REPO.starts_with("https://"), "{RETICULUM_REPO}");
}

/// One requirement of a pip requirements file: `name==version`, any environment marker, and the
/// `--hash` options of its continuation lines.
#[derive(Debug)]
struct Requirement {
    line: String,
    pin: Option<(String, String)>,
    hashes: Vec<String>,
}

fn parse_requirements(text: &str) -> Vec<Requirement> {
    let mut logical = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        // pip's comments start at a `#` at the start of a line or after whitespace.
        let line = if line.trim_start().starts_with('#') {
            ""
        } else {
            line.split(" #").next().unwrap_or_default()
        };
        if let Some(start) = line.trim_end().strip_suffix('\\') {
            current.push_str(start);
            current.push(' ');
            continue;
        }
        current.push_str(line);
        let words: Vec<&str> = current.split_whitespace().collect();
        if !words.is_empty() {
            logical.push(words.join(" "));
        }
        current.clear();
    }
    logical
        .into_iter()
        .map(|line| {
            let requirement = line.split([';', ' ']).next().unwrap_or_default();
            let pin = requirement
                .split_once("==")
                .map(|(name, version)| (name.to_owned(), version.to_owned()));
            let hashes = line
                .split_whitespace()
                .filter_map(|word| word.strip_prefix("--hash="))
                .map(str::to_owned)
                .collect();
            Requirement { line, pin, hashes }
        })
        .collect()
}

/// PEP 503's normalised project name.
fn normalized(name: &str) -> String {
    name.to_ascii_lowercase().replace(['_', '.'], "-")
}

/// `uv pip sync --require-hashes` installs exactly what requirements.txt pins, so every package in
/// it must be pinned to one version and carry the sha256 hashes of its files. Every package
/// requirements.in asks for must be in it: requirements.txt is regenerated from it.
#[test]
fn requirements_pin_every_package_with_hashes() {
    let pinned = parse_requirements(include_str!("../../../tests/e2e/requirements.txt"));
    assert!(pinned.len() >= 2, "{pinned:?}");
    for requirement in &pinned {
        let Some((name, version)) = &requirement.pin else {
            panic!("not pinned with ==: {}", requirement.line);
        };
        assert!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
            "{}",
            requirement.line
        );
        assert!(
            !version.is_empty() && !version.contains(['*', ',', '<', '>', '=', '!', '~']),
            "not one exact version: {}",
            requirement.line
        );
        assert!(!requirement.hashes.is_empty(), "no --hash: {}", requirement.line);
        for hash in &requirement.hashes {
            let digest = hash.strip_prefix("sha256:").unwrap_or_default();
            assert!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "not a sha256 hash: {hash}"
            );
        }
    }

    let names: Vec<String> = pinned
        .iter()
        .filter_map(|r| r.pin.as_ref())
        .map(|(name, _)| normalized(name))
        .collect();
    let wanted = include_str!("../../../tests/e2e/requirements.in")
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter(|line| !line.is_empty());
    for package in wanted {
        let name = package
            .split(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c)))
            .next()
            .unwrap_or_default();
        assert!(
            names.contains(&normalized(name)),
            "requirements.in asks for {package}, but requirements.txt does not pin it; regenerate it"
        );
    }
    for package in ["cryptography", "pyserial"] {
        assert!(names.iter().any(|name| name == package), "{names:?}");
    }
}

#[test]
fn requirements_parser_joins_continuations_and_skips_comments() {
    let parsed = parse_requirements(
        "# header\n\
         cffi==2.1.1 ; platform_python_implementation != 'PyPy' \\\n    \
             --hash=sha256:aa \\\n    \
             --hash=sha256:bb\n    \
             # via cryptography\n\
         pyserial \\\n    \
             --hash=sha256:cc\n",
    );
    assert_eq!(parsed.len(), 2, "{parsed:?}");
    assert_eq!(parsed[0].pin, Some(("cffi".into(), "2.1.1".into())));
    assert_eq!(parsed[0].hashes, ["sha256:aa", "sha256:bb"]);
    assert_eq!(parsed[1].pin, None);
    assert_eq!(parsed[1].hashes, ["sha256:cc"]);
}

/// The venv marker holds a fingerprint of requirements.txt, so the venv is synced again exactly when
/// the file changes. FNV-1a's published test vectors pin the function down.
#[test]
fn the_venv_marker_changes_with_the_requirements() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);

    assert_eq!(
        requirements_fingerprint(b"foobar"),
        "tests/e2e/requirements.txt fnv1a64:85944171f73967e8\n"
    );
    let pinned = b"pyserial==3.5 --hash=sha256:aa\n";
    assert_eq!(requirements_fingerprint(pinned), requirements_fingerprint(pinned));
    assert_ne!(
        requirements_fingerprint(pinned),
        requirements_fingerprint(b"pyserial==3.5 --hash=sha256:ab\n")
    );
}

const PR_WORKFLOW: &str = include_str!("../../../.github/workflows/pr.yml");
const CI_WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yml");

/// The lines of the step called `name` in `workflow`, from its `- name:` line to the next step,
/// the comment above that, or the next job.
fn workflow_step<'a>(workflow: &'a str, name: &str) -> Vec<&'a str> {
    let header = format!("- name: {name}");
    let mut lines = workflow.lines().skip_while(|line| line.trim() != header);
    let first = lines.next().unwrap_or_else(|| panic!("no step {name:?}"));
    let indent = |line: &str| line.len() - line.trim_start().len();
    std::iter::once(first)
        .chain(lines.take_while(|line| line.trim().is_empty() || indent(line) > indent(first)))
        .collect()
}

/// The names of the steps of the `test` job in `workflow`.
fn test_job_step_names(workflow: &str) -> Vec<&str> {
    // A job's lines are blank or indented further than the job keys, `  test:` and the next one.
    let is_job_key = |line: &str| line.starts_with("  ") && !line.starts_with("   ") && line.ends_with(':');
    workflow
        .lines()
        .skip_while(|line| *line != "  test:")
        .skip(1)
        .take_while(|line| !is_job_key(line))
        .filter_map(|line| line.trim().strip_prefix("- name: "))
        .collect()
}

/// main runs every check a pull request runs: each step of pr.yml's test job is in ci.yml's test
/// job, word for word, except the SF7 end-to-end step, whose scenarios ci.yml runs all of.
#[test]
fn main_runs_every_check_pull_requests_run() {
    let pr_steps = test_job_step_names(PR_WORKFLOW);
    let ci_steps = test_job_step_names(CI_WORKFLOW);
    assert!(
        pr_steps.contains(&"Tests") && pr_steps.contains(&"Install uv"),
        "{pr_steps:?}"
    );
    for &name in pr_steps.iter().filter(|&&name| name != "RNS end-to-end (SF7)") {
        assert!(ci_steps.contains(&name), "ci.yml has no step {name:?}: {ci_steps:?}");
        assert_eq!(
            workflow_step(PR_WORKFLOW, name),
            workflow_step(CI_WORKFLOW, name),
            "{name}"
        );
    }
    assert!(ci_steps.contains(&"RNS end-to-end (all scenarios)"), "{ci_steps:?}");
}

/// pr.yml and ci.yml install uv with the same step, so that the pinned release and its hashes
/// cannot drift apart.
#[test]
fn pr_and_ci_workflows_install_the_same_uv() {
    let step = workflow_step(PR_WORKFLOW, "Install uv");
    assert_eq!(step, workflow_step(CI_WORKFLOW, "Install uv"));
    let script = step.join("\n");
    for expected in [
        "set -euo pipefail",
        "x86_64)",
        "aarch64)",
        "curl --fail",
        "--retry 3",
        "sha256sum --check --strict",
        "\"$GITHUB_PATH\"",
    ] {
        assert!(script.contains(expected), "{expected:?} missing from\n{script}");
    }
}

/// Pull requests run the SF7 scenario and main runs every scenario, both against the pinned
/// Reticulum.
#[test]
fn workflows_run_the_e2e_test_against_the_pinned_reticulum() {
    for (workflow, step, expected) in [
        (PR_WORKFLOW, "RNS end-to-end (SF7)", &["sf7"][..]),
        (
            CI_WORKFLOW,
            "RNS end-to-end (all scenarios)",
            &["sf7", "sf11", "rnodeconf"],
        ),
    ] {
        let step = workflow_step(workflow, step);
        let command = step
            .iter()
            .find_map(|line| line.trim().strip_prefix("run: cargo xtask rns-e2e"))
            .unwrap_or_else(|| panic!("no `cargo xtask rns-e2e` in {step:?}"));
        let options = parse_args(&command.split_whitespace().map(str::to_owned).collect::<Vec<_>>()).unwrap();
        assert!(options.fetch_reticulum, "{command}");
        assert_eq!(options.scenarios.iter().map(|s| s.name).collect::<Vec<_>>(), expected);
    }
}

/// Runs `git` in `dir` with a fixed identity and no signing, whatever the user's configuration, or
/// returns `None` when there is no `git` to run.
fn git_in(dir: &Path, args: &[&str]) -> Option<String> {
    let output = match Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=xtask tests",
            "-c",
            "user.email=xtask@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
    {
        Ok(output) => output,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => panic!("start git: {e}"),
    };
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A stand-in for the Reticulum repository at `dir` with two commits; returns their hashes, oldest
/// first, or `None` when there is no `git`.
fn upstream_reticulum(dir: &Path) -> Option<[String; 2]> {
    fs::create_dir_all(dir.join("RNS")).unwrap();
    fs::write(dir.join("RNS/__init__.py"), "__version__ = '1'\n").unwrap();
    git_in(dir, &["init", "--quiet"])?;
    git_in(dir, &["add", "."])?;
    git_in(dir, &["commit", "--quiet", "-m", "one"])?;
    let first = git_in(dir, &["rev-parse", "HEAD"])?;
    fs::write(dir.join("RNS/__init__.py"), "__version__ = '2'\n").unwrap();
    git_in(dir, &["commit", "--quiet", "-am", "two"])?;
    let second = git_in(dir, &["rev-parse", "HEAD"])?;
    Some([first, second])
}

/// `--fetch-reticulum` fetches the pinned commit, also one behind the branch tip as a pin usually
/// is, checks it out, and reuses that checkout on later runs without going to the network. A
/// checkout with local changes is refused. Skipped without `git`; the CI builder image has it.
#[test]
fn fetch_reticulum_fetches_the_pinned_commit_and_reuses_it() {
    let dir = test_dir("fetch-reticulum");
    let upstream = dir.join("upstream");
    let Some([pinned, _tip]) = upstream_reticulum(&upstream) else {
        eprintln!("skipping the Reticulum fetch check: git is not on PATH");
        return;
    };
    let repo = format!("file://{}", upstream.display());
    let cache = dir.join("rns-e2e");
    fs::create_dir_all(&cache).unwrap();

    let checkout = fetch_reticulum(&cache, &repo, &pinned, &[]).unwrap();
    assert_eq!(checkout, cache.join(format!("reticulum-{}", &pinned[..12])));
    assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).unwrap(), pinned);
    assert_eq!(
        fs::read_to_string(checkout.join("RNS/__init__.py")).unwrap(),
        "__version__ = '1'\n"
    );
    assert!(!cache.join(format!("reticulum-{}.partial", &pinned[..12])).exists());

    // Reused: the upstream is gone, and Python's bytecode caches are not local changes.
    fs::remove_dir_all(&upstream).unwrap();
    fs::create_dir_all(checkout.join("RNS/__pycache__")).unwrap();
    fs::write(checkout.join("RNS/__pycache__/__init__.cpython-313.pyc"), "").unwrap();
    assert_eq!(fetch_reticulum(&cache, &repo, &pinned, &[]).unwrap(), checkout);

    fs::write(checkout.join("RNS/__init__.py"), "__version__ = 'patched'\n").unwrap();
    let error = fetch_reticulum(&cache, &repo, &pinned, &[]).unwrap_err().to_string();
    assert!(error.contains("has local changes"), "{error}");
    assert!(error.contains("delete"), "{error}");
}

/// A `.partial` directory that an interrupted fetch left behind, for example one of a cancelled CI
/// job, is deleted and the fetch starts over, so nothing of it reaches the checkout. Skipped
/// without `git`.
#[test]
fn fetch_reticulum_recovers_from_a_stale_partial_checkout() {
    let dir = test_dir("fetch-reticulum-partial");
    let upstream = dir.join("upstream");
    let Some([pinned, _tip]) = upstream_reticulum(&upstream) else {
        eprintln!("skipping the Reticulum fetch check: git is not on PATH");
        return;
    };
    let repo = format!("file://{}", upstream.display());
    let cache = dir.join("rns-e2e");
    let partial = cache.join(format!("reticulum-{}.partial", &pinned[..12]));

    // What a `git fetch --depth 1` killed part-way leaves: the lock it held, a half-written pack
    // (git writes packs read-only), and stray files, one where the checkout puts a tracked file.
    fs::create_dir_all(partial.join("RNS")).unwrap();
    git_in(&partial, &["init", "--quiet"]).unwrap();
    fs::write(partial.join(".git/shallow.lock"), "").unwrap();
    let pack = partial.join(".git/objects/pack/tmp_pack_stale");
    fs::write(&pack, "half a pack").unwrap();
    fs::set_permissions(&pack, fs::Permissions::from_mode(0o444)).unwrap();
    fs::write(partial.join("RNS/__init__.py"), "stale\n").unwrap();
    fs::write(partial.join("stale.txt"), "stale\n").unwrap();

    let checkout = fetch_reticulum(&cache, &repo, &pinned, &[]).unwrap();
    assert_eq!(checkout, cache.join(format!("reticulum-{}", &pinned[..12])));
    assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]).unwrap(), pinned);
    assert_eq!(
        fs::read_to_string(checkout.join("RNS/__init__.py")).unwrap(),
        "__version__ = '1'\n"
    );
    assert!(!checkout.join("stale.txt").exists());
    assert!(!checkout.join(".git/shallow.lock").exists());
    assert!(!checkout.join(".git/objects/pack/tmp_pack_stale").exists());
    assert!(fs::symlink_metadata(&partial).is_err(), "{} is left", partial.display());
}

/// A checkout that is not at the pin is refused rather than used, and a fetch that keeps failing is
/// retried once per delay and then reported, leaving no checkout behind.
#[test]
fn fetch_reticulum_refuses_another_commit_and_reports_failed_fetches() {
    let dir = test_dir("fetch-reticulum-errors");
    let upstream = dir.join("upstream");
    let Some([first, second]) = upstream_reticulum(&upstream) else {
        eprintln!("skipping the Reticulum fetch check: git is not on PATH");
        return;
    };
    let repo = format!("file://{}", upstream.display());
    let cache = dir.join("rns-e2e");
    fs::create_dir_all(&cache).unwrap();

    let checkout = fetch_reticulum(&cache, &repo, &first, &[]).unwrap();
    git_in(&checkout, &["fetch", "--quiet", "--depth", "1", &repo, &second]).unwrap();
    git_in(&checkout, &["checkout", "--quiet", "--detach", &second]).unwrap();
    let error = fetch_reticulum(&cache, &repo, &first, &[]).unwrap_err().to_string();
    assert!(
        error.contains(&format!("is at {second}, not the pinned {first}")),
        "{error}"
    );

    let missing = "0123456789abcdef0123456789abcdef01234567";
    let start = Instant::now();
    let error = fetch_reticulum(&cache, &repo, missing, &[Duration::ZERO, Duration::from_millis(1)])
        .unwrap_err()
        .to_string();
    assert!(error.contains("in 3 attempts"), "{error}");
    assert!(!cache.join("reticulum-0123456789ab").exists());
    assert!(start.elapsed() < Duration::from_secs(60));

    let error = fetch_reticulum(&cache, &repo, "e40191b", &[]).unwrap_err().to_string();
    assert!(error.contains("not a full commit hash"), "{error}");
}

/// A local Reticulum checkout that is not at the pin, or whose commit cannot be told, gets a
/// warning, not an error. Skipped without `git`.
#[test]
fn a_local_reticulum_off_the_pin_gets_a_warning() {
    let dir = test_dir("reticulum-pin-warning");
    let upstream = dir.join("upstream");
    let Some([first, second]) = upstream_reticulum(&upstream) else {
        eprintln!("skipping the Reticulum pin check: git is not on PATH");
        return;
    };
    assert_eq!(reticulum_pin_warning(&upstream, &second), None);
    let warning = reticulum_pin_warning(&upstream, &first).unwrap();
    assert!(
        warning.contains(&format!("is at {second}, not the pinned {first} that CI tests")),
        "{warning}"
    );
    assert!(warning.contains("--fetch-reticulum"), "{warning}");

    let unpacked = dir.join("unpacked");
    fs::create_dir_all(unpacked.join("RNS")).unwrap();
    let warning = reticulum_pin_warning(&unpacked, &first).unwrap();
    assert!(warning.contains("is not a git checkout"), "{warning}");
}

/// When a scenario fails, the harness prints the end of each of its logs, in path order, so that a
/// CI log shows why.
#[test]
fn a_failed_scenario_prints_the_tail_of_each_log() {
    let dir = test_dir("log-tails");
    fs::create_dir_all(dir.join("a").join("rns")).unwrap();
    let daemon_log: String = (1..=150).map(|i| format!("daemon line {i}\n")).collect();
    fs::write(dir.join("a").join("daemon.log"), daemon_log).unwrap();
    fs::write(dir.join("rns-client.log"), "path found\nlink timed out\n").unwrap();
    fs::write(dir.join("client.error"), "timeout waiting for link active").unwrap();
    fs::write(dir.join("timings.json"), "{\"sf\": 7}").unwrap();
    fs::write(dir.join("a").join("rns").join("config"), "[reticulum]").unwrap();

    let mut out = Vec::new();
    write_log_tails(&dir, 100, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    let headers: Vec<&str> = out.lines().filter(|line| line.starts_with("=====")).collect();
    assert_eq!(
        headers,
        [
            format!(
                "===== {} (last 100 of 150 lines) =====",
                dir.join("a").join("daemon.log").display()
            ),
            format!("===== {} (1 line) =====", dir.join("client.error").display()),
            format!("===== {} (2 lines) =====", dir.join("rns-client.log").display()),
        ]
    );
    assert!(out.contains("\ndaemon line 51\n"), "{out}");
    assert!(out.contains("\ndaemon line 150\n"), "{out}");
    assert!(!out.contains("\ndaemon line 50\n"), "{out}");
    assert!(out.contains("\ntimeout waiting for link active\n"), "{out}");
    assert!(out.ends_with("\npath found\nlink timed out\n"), "{out}");
    assert!(!out.contains("\"sf\"") && !out.contains("[reticulum]"), "{out}");
}

/// The daemon rejects unknown keys (`deny_unknown_fields`), so the config the harness writes
/// must stay loadable by the rnode-nomu it drives.
#[test]
fn daemon_config_loads_with_the_rnode_nomu_parser() {
    let air = Path::new("target/rns-e2e/runs/r1-2/sf7/air");
    let pty = Path::new("target/rns-e2e/runs/r1-2/sf7/b/pty");
    let settings = Path::new("target/rns-e2e/runs/r1-2/sf7/b/radio-settings.toml");
    let config = Config::from_toml(&daemon_config(air, "b", pty, settings, None, -64, 6.5)).unwrap();

    assert_eq!(config.radio.driver, RadioDriver::Sim);
    assert!(!config.radio.avoid_interference);
    assert_eq!(config.radio.sim.air_dir, air);
    assert_eq!(config.radio.sim.node_name, "b");
    assert_eq!(config.radio.sim.rssi_dbm, -64);
    assert_eq!(config.radio.sim.snr_db, 6.5);
    assert_eq!(config.radio.sim.loss, 0.0);
    assert_eq!(config.pty.symlink.as_deref(), Some(pty));
    assert_eq!(config.storage.radio_settings.as_deref(), Some(settings));
    assert_eq!(config.storage.eeprom, None);
    assert_eq!(config.logging.log_level, "debug");
    assert_eq!(config.warnings(), Vec::<String>::new());

    // The rnodeconf scenario's second daemon keeps an EEPROM info block.
    let eeprom = Path::new("target/rns-e2e/runs/r1-2/rnodeconf/signed/eeprom.bin");
    let config = Config::from_toml(&daemon_config(air, "signed", pty, settings, Some(eeprom), -64, 6.5)).unwrap();
    assert_eq!(config.storage.radio_settings.as_deref(), Some(settings));
    assert_eq!(config.storage.eeprom.as_deref(), Some(eeprom));
    assert_eq!(config.warnings(), Vec::<String>::new());
}

#[test]
fn daemon_config_quotes_awkward_paths_and_whole_snr_values() {
    let awkward = Path::new("runs/\"quoted\" \\ back\tslash\u{1}/air");
    let config = Config::from_toml(&daemon_config(awkward, "a", awkward, awkward, Some(awkward), -58, 9.0)).unwrap();

    assert_eq!(config.radio.sim.air_dir, awkward);
    assert_eq!(config.pty.symlink.as_deref(), Some(awkward));
    assert_eq!(config.storage.radio_settings.as_deref(), Some(awkward));
    assert_eq!(config.storage.eeprom.as_deref(), Some(awkward));
    assert_eq!(config.radio.sim.snr_db, 9.0);
}

/// The rnodeconf scenario checks rnodeconf's output for each text it expects, and a failure names
/// the first one missing and shows the output.
#[test]
fn expect_output_names_the_missing_text() {
    let output = "[12:00:00] EEPROM checksum correct\n[12:00:00] \tDevice mode        : TNC\n";
    assert_eq!(
        expect_output(output, &["EEPROM checksum correct", "Device mode        : TNC"]),
        Ok(())
    );
    assert_eq!(
        expect_output(output, &["EEPROM checksum correct", "Validated - Local signature"]),
        Err(format!(
            "rnodeconf did not print \"Validated - Local signature\"; it printed:\n{output}"
        ))
    );
}

/// Once a daemon has exited, the PTY its symlink pointed at is gone, so a symlink it left behind
/// dangles. `terminate_all()` must still report it.
#[test]
fn terminate_all_reports_a_dangling_pty_symlink() {
    let error = terminate_exited_daemon("dangling-pty-symlink", |link| {
        symlink(link.with_file_name("closed-pty"), link).unwrap();
    })
    .unwrap_err();
    assert!(error.contains("did not remove PTY symlink"), "{error}");
}

#[test]
fn terminate_all_accepts_a_removed_pty_symlink() {
    terminate_exited_daemon("removed-pty-symlink", |_| {}).unwrap();
}

/// Runs `terminate_all()` on a stand-in daemon that has already exited cleanly, after
/// `leave_link` has had the chance to leave something at its PTY symlink path.
fn terminate_exited_daemon(name: &str, leave_link: impl FnOnce(&Path)) -> Result<(), String> {
    let dir = test_dir(name);
    let link = dir.join("pty");
    leave_link(&link);
    let mut children = Children::default();
    children
        .spawn_logged("daemon", &mut Command::new("true"), &dir.join("daemon.log"), Some(link))
        .unwrap();
    // Let it exit by itself, so that only the symlink can be at fault.
    let deadline = Instant::now() + Duration::from_secs(10);
    while children.0[0].child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the stand-in daemon did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    children
        .terminate_all(Duration::from_secs(3))
        .map_err(|e| e.to_string())
}

/// A daemon finishes the packet on the air before it exits after SIGTERM (#51), so the harness
/// must wait at least as long as the longest packet takes on the simulated air, plus the 3 s the
/// processes need to exit. The simulated air times a frame with Semtech's formula: (preamble +
/// 4.25 + 8 + ceil((8 x 255 - 4 SF + 28 + 16) / (4 SF)) x 5) symbols of 2^SF / 125 kHz, with a
/// 24-symbol preamble at SF7 and 18 symbols at SF11 (firmware `LORA_PREAMBLE_SYMBOLS_MIN`). The
/// longest packet, 508 bytes, takes two such 255-byte frames.
#[test]
fn sigterm_wait_outlasts_the_longest_packet_on_the_simulated_air() {
    for (sf, frame_ms) in [(7, 416.0), (11, 4_345.856)] {
        let scenario = Scenario {
            name: "test",
            sf,
            check: Check::Reticulum,
        };
        let wait_ms = scenario.sigterm_wait().as_secs_f64() * 1000.0;
        assert!(wait_ms >= 2.0 * frame_ms + 3000.0, "SF{sf}: {wait_ms} ms");
        assert!(
            wait_ms <= 2.0 * (2.0 * frame_ms) + 3000.0,
            "SF{sf}: {wait_ms} ms is excessive"
        );
    }
}

/// A fresh, empty directory under this crate's `target/`.
fn test_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("xtask-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The harness's Python only runs by hand (`cargo xtask rns-e2e`), so check here that it at least
/// compiles. `compile()` parses and compiles without running or importing anything. Skipped when
/// `python3` is not on PATH; the CI builder image has it.
#[test]
fn harness_python_compiles() {
    let scripts = [
        ("kiss_boundary.py", KISS_BOUNDARY_PY),
        ("rns_server.py", SERVER_PY),
        ("rns_client.py", CLIENT_PY),
        (
            "tests/e2e/kiss_probe.py",
            include_str!("../../../tests/e2e/kiss_probe.py"),
        ),
    ];
    for (name, source) in scripts {
        let Some(output) = python_compile(name, source) else {
            eprintln!("skipping the Python syntax check: python3 is not on PATH");
            return;
        };
        assert!(
            output.status.success(),
            "{name} does not compile:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Issue #60: the probe's `expect()` reports a payload that arrives without RSSI and SNR right
/// before it as a `RuntimeError`, also when another data frame came just before, where it used to
/// fail with a `TypeError`; `expect_absent()` reports any data frame. The probe runs with fake
/// ports and a stub `serial` module, so no pyserial is needed. Skipped without `python3`.
#[test]
fn kiss_boundary_probe_reports_missing_metadata_and_stray_data() {
    let driver = r#"
import sys, time, types
sys.modules['serial'] = types.ModuleType('serial')
probe = {'__name__': 'kiss_boundary'}
exec(compile(sys.stdin.read(), 'kiss_boundary.py', 'exec'), probe)
RSSI, SNR, DATA = probe['CMD_STAT_RSSI'], probe['CMD_STAT_SNR'], probe['CMD_DATA']

class Port:
    def __init__(self, frames): self.frames = frames
    def read_frames(self, deadline):
        frames, self.frames = self.frames, []
        if not frames: time.sleep(0.01)
        return frames

def outcome(check, frames):
    try:
        check(Port(frames), b'payload', 0.1)
        return 'ok'
    except RuntimeError as e:
        return f'RuntimeError: {e}'

expect, absent = probe['expect'], probe['expect_absent']
print(outcome(expect, [(RSSI, b'\x60'), (SNR, b'\x1c'), (DATA, b'payload')]))
print(outcome(expect, [(RSSI, b'\x60'), (DATA, b'stray'), (DATA, b'payload')]))
print(outcome(expect, [(DATA, b'payload')]))
print(outcome(expect, [(DATA, b'stray')]))
print(outcome(absent, [(RSSI, b'\x60')]))
print(outcome(absent, [(RSSI, b'\x60'), (DATA, b'stray')]))
"#;
    let Some(output) = python("probe-check", driver, KISS_BOUNDARY_PY) else {
        eprintln!("skipping the KISS probe check: python3 is not on PATH");
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        [
            "ok",
            "RuntimeError: missing RSSI/SNR immediately before data len 7: [(35, b'`'), (0, 5)]",
            "RuntimeError: missing RSSI/SNR immediately before data len 7: []",
            "RuntimeError: missing payload len 7, saw [(0, 5)]",
            "ok",
            "RuntimeError: over-MTU payload produced CMD_DATA frames: [(35, b'`'), (0, 5)]",
        ]
    );
}

/// Issue #60: the harness creates the air directory private to its owner in one step, with any
/// missing parents, and refuses an existing directory that others can open rather than change it.
#[test]
fn create_private_dir_creates_it_private_and_refuses_an_open_one() {
    let dir = test_dir("private-dir");
    let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;

    let air = dir.join("run").join("air");
    create_private_dir(&air).unwrap();
    assert_eq!(mode(&air), 0o700);
    create_private_dir(&air).unwrap();

    let open = dir.join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    let error = create_private_dir(&open).unwrap_err();
    assert!(error.to_string().contains("mode 0755"), "{error}");
    assert_eq!(mode(&open), 0o755, "left as it was");

    let link = dir.join("link");
    symlink(&air, &link).unwrap();
    assert!(create_private_dir(&link).is_err(), "a symlink to a private directory");
}

/// Compiles `source` with `python3`, or returns `None` when there is no `python3` to run.
fn python_compile(name: &str, source: &str) -> Option<Output> {
    python(
        name,
        "import sys; compile(sys.stdin.buffer.read(), sys.argv[1], 'exec')",
        source,
    )
}

/// Runs `code` with `python3 -I`, `name` as its argument and `stdin` as its input, or returns `None`
/// when there is no `python3` to run.
fn python(name: &str, code: &str, stdin: &str) -> Option<Output> {
    let mut python = match Command::new("python3")
        .args(["-I", "-c", code, name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(python) => python,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => panic!("start python3: {e}"),
    };
    let mut input = python.stdin.take().expect("python3 stdin is piped");
    input.write_all(stdin.as_bytes()).expect("write the input to python3");
    drop(input);
    Some(python.wait_with_output().expect("wait for python3"))
}
