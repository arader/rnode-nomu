//! Keeps README.md and DRIVERS.md in step with the code. Every daemon config they show must load
//! with the real config loader, every relative link must point at a file or heading that exists,
//! and the README's Reticulum example must use the option names Reticulum reads.
//!
//! A ```` ```toml ```` block is a daemon config, either complete or a fragment such as a single
//! section. Fragments are checked merged over minimal `[spi]` and `[gpio]` sections. TOML that is not
//! a daemon config, such as a Cargo manifest or the saved radio settings file, is fenced as
//! ```` ```toml ignore ```` and only has to be valid TOML.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use nix::unistd::{Gid, Group};
use rnode_linux::config::Config;

const DOCS: &[&str] = &["README.md", "DRIVERS.md"];

/// The options Reticulum (e40191b3) reads for an `RNodeInterface`: `RNS/Reticulum.py` for `type`
/// and `enabled`/`interface_enabled`, `RNS/Interfaces/RNodeInterface.py` for the rest. Reticulum
/// silently ignores any other key, and wrongly spelled radio keys leave the radio unconfigured.
const RNODE_INTERFACE_KEYS: &[&str] = &[
    "type",
    "enabled",
    "interface_enabled",
    "port",
    "frequency",
    "bandwidth",
    "txpower",
    "spreadingfactor",
    "codingrate",
    "flow_control",
    "id_interval",
    "id_callsign",
    "airtime_limit_short",
    "airtime_limit_long",
];

/// What config fragments are merged over: the sections the default `sx1262` driver requires.
const FRAGMENT_BASE: &str = r#"
[spi]
device = "/dev/spidev0.0"

[gpio]
reset = 1
busy = 2
dio1 = 3
"#;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_doc(name: &str) -> String {
    fs::read_to_string(repo_root().join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
}

#[derive(Debug)]
struct CodeBlock {
    /// 1-based line of the opening fence.
    line: usize,
    /// The info string after the opening backticks, e.g. `toml ignore`.
    info: String,
    body: String,
}

/// Fenced code blocks (backticks only), with the fence's indentation removed from their lines.
fn code_blocks(doc: &str, text: &str) -> Vec<CodeBlock> {
    let mut blocks = Vec::new();
    let mut open: Option<(CodeBlock, usize, usize)> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let ticks = trimmed.bytes().take_while(|&b| b == b'`').count();
        let rest = &trimmed[ticks..];
        match open.take() {
            None => {
                if ticks >= 3 && !rest.contains('`') {
                    let block = CodeBlock {
                        line: index + 1,
                        info: rest.trim().to_owned(),
                        body: String::new(),
                    };
                    open = Some((block, indent, ticks));
                }
            }
            Some((block, _, fence)) if ticks >= fence && rest.trim().is_empty() => blocks.push(block),
            Some((mut block, fence_indent, fence)) => {
                block.body.push_str(&line[indent.min(fence_indent)..]);
                block.body.push('\n');
                open = Some((block, fence_indent, fence));
            }
        }
    }
    if let Some((block, _, _)) = open {
        panic!("{doc}:{}: unterminated code block", block.line);
    }
    blocks
}

/// Merge `overlay` into `base`, recursing into tables that both have.
fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base_table)), toml::Value::Table(overlay_table)) => {
                merge(base_table, overlay_table)
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// `pty.group` must name a group that exists where the config is loaded. Documentation examples
/// use groups such as `dialout`, so check them with a group every system has.
fn use_existing_group(config: &mut toml::Table) {
    if let Some(toml::Value::Table(pty)) = config.get_mut("pty") {
        if pty.contains_key("group") {
            let root_group = Group::from_gid(Gid::from_raw(0)).unwrap().unwrap().name;
            pty.insert("group".to_owned(), toml::Value::String(root_group));
        }
    }
}

/// Checks every TOML block of `doc` and returns how many daemon configs it contains.
fn check_toml_blocks(doc: &str, text: &str) -> usize {
    let mut configs = 0;
    for block in code_blocks(doc, text) {
        let mut words = block.info.split_whitespace();
        if words.next() != Some("toml") {
            continue;
        }
        let ignore = words.any(|word| word == "ignore");
        let table: toml::Table = block
            .body
            .parse()
            .unwrap_or_else(|e| panic!("{doc}:{}: invalid TOML: {e}", block.line));
        if ignore {
            continue;
        }
        let mut config: toml::Table = FRAGMENT_BASE.parse().unwrap();
        merge(&mut config, table);
        use_existing_group(&mut config);
        let merged = toml::to_string(&config).unwrap();
        if let Err(e) = Config::from_toml(&merged) {
            panic!(
                "{doc}:{}: not a valid rnode-linux config: {e}\n\
                 (fence TOML that is not a daemon config as ```toml ignore)",
                block.line
            );
        }
        configs += 1;
    }
    configs
}

/// The lines of `text` outside fenced code blocks; lines inside them are blank, so indices still
/// match line numbers.
fn prose(doc: &str, text: &str) -> Vec<String> {
    let mut in_block = HashSet::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut fence: Option<usize> = None;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let ticks = trimmed.bytes().take_while(|&b| b == b'`').count();
        let rest = &trimmed[ticks..];
        match fence {
            None if ticks >= 3 && !rest.contains('`') => {
                fence = Some(ticks);
                in_block.insert(index);
            }
            Some(open) => {
                in_block.insert(index);
                if ticks >= open && rest.trim().is_empty() {
                    fence = None;
                }
            }
            None => {}
        }
    }
    assert!(fence.is_none(), "{doc}: unterminated code block");
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            if in_block.contains(&index) {
                String::new()
            } else {
                (*line).to_owned()
            }
        })
        .collect()
}

/// Remove inline code spans: a run of backticks up to the next run of the same length.
fn strip_code_spans(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            let next = line[i..].find('`').map_or(line.len(), |offset| i + offset);
            out.push_str(&line[i..next]);
            i = next;
            continue;
        }
        let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        let closing = "`".repeat(run);
        let mut search = i + run;
        let mut end = None;
        while let Some(offset) = line[search..].find(&closing) {
            let at = search + offset;
            let after = bytes[at + run..].iter().take_while(|&&b| b == b'`').count();
            if after == 0 {
                end = Some(at + run);
                break;
            }
            search = at + run + after;
        }
        match end {
            Some(end) => i = end,
            None => {
                out.push_str(&line[i..i + run]);
                i += run;
            }
        }
    }
    out
}

/// Heading anchors as GitHub and Forgejo generate them: lowercase letters, digits, `_` and `-`
/// are kept, whitespace becomes `-`, everything else is dropped; repeats get `-1`, `-2`, ….
fn anchors(doc: &str, text: &str) -> HashSet<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut anchors = HashSet::new();
    for line in prose(doc, text) {
        let level = line.bytes().take_while(|&b| b == b'#').count();
        if !(1..=6).contains(&level) || !line[level..].starts_with(' ') {
            continue;
        }
        // The heading's text: code spans keep their content but lose the backticks.
        let mut slug = String::new();
        for c in line[level..].trim().chars().filter(|&c| c != '`') {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                slug.extend(c.to_lowercase());
            } else if c.is_whitespace() {
                slug.push('-');
            }
        }
        let count = seen.entry(slug.clone()).or_insert(0);
        let anchor = if *count == 0 { slug } else { format!("{slug}-{count}") };
        *count += 1;
        anchors.insert(anchor);
    }
    anchors
}

/// Inline link targets, `[text](target)`, with their 1-based line numbers.
fn link_targets(doc: &str, text: &str) -> Vec<(usize, String)> {
    let mut targets = Vec::new();
    for (index, line) in prose(doc, text).iter().enumerate() {
        let line = strip_code_spans(line);
        let mut rest = line.as_str();
        while let Some(start) = rest.find("](") {
            let after = &rest[start + 2..];
            let end = after
                .find(')')
                .unwrap_or_else(|| panic!("{doc}:{}: unterminated link", index + 1));
            targets.push((index + 1, after[..end].to_owned()));
            rest = &after[end..];
        }
    }
    targets
}

#[test]
fn readme_and_drivers_daemon_configs_load() {
    let readme = read_doc("README.md");
    assert!(
        check_toml_blocks("README.md", &readme) >= 2,
        "README.md should show the sim quick start and the uConsole config"
    );
    check_toml_blocks("DRIVERS.md", &read_doc("DRIVERS.md"));
}

#[test]
fn readme_quotes_the_uconsole_config_verbatim() {
    let config = fs::read_to_string(repo_root().join("config.uconsole-aio-v2.toml")).unwrap();
    let readme = read_doc("README.md");
    assert!(
        code_blocks("README.md", &readme)
            .iter()
            .any(|block| block.info == "toml" && block.body == config),
        "README.md must quote config.uconsole-aio-v2.toml verbatim"
    );
}

/// The hardware validation found an older README using `spreading_factor`/`coding_rate` without
/// `enabled = yes`, which makes `rnsd` skip the interface or exit.
#[test]
fn readme_reticulum_example_uses_rnodeinterface_keys() {
    let readme = read_doc("README.md");
    let blocks: Vec<CodeBlock> = code_blocks("README.md", &readme)
        .into_iter()
        .filter(|block| block.body.contains("type = RNodeInterface"))
        .collect();
    assert!(!blocks.is_empty(), "README.md should show an RNodeInterface example");
    for block in blocks {
        let keys: Vec<&str> = block
            .body
            .lines()
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim()))
            .collect();
        for key in &keys {
            assert!(
                RNODE_INTERFACE_KEYS.contains(key),
                "README.md:{}: Reticulum does not read `{key}` for an RNodeInterface",
                block.line
            );
        }
        let required = [
            "type",
            "port",
            "frequency",
            "bandwidth",
            "txpower",
            "spreadingfactor",
            "codingrate",
        ];
        for key in required {
            assert!(
                keys.contains(&key),
                "README.md:{}: the example lacks `{key}`",
                block.line
            );
        }
        assert!(
            keys.contains(&"enabled") || keys.contains(&"interface_enabled"),
            "README.md:{}: without `enabled = yes` Reticulum skips the interface",
            block.line
        );
    }
}

#[test]
fn readme_and_drivers_relative_links_resolve() {
    let root = repo_root();
    let texts: HashMap<&str, String> = DOCS.iter().map(|&doc| (doc, read_doc(doc))).collect();
    let doc_anchors: HashMap<&str, HashSet<String>> =
        texts.iter().map(|(&doc, text)| (doc, anchors(doc, text))).collect();
    let mut checked = 0;
    for (&doc, text) in &texts {
        for (line, target) in link_targets(doc, text) {
            if target.starts_with("http://") || target.starts_with("https://") || target.starts_with("mailto:") {
                continue;
            }
            let (path, anchor) = target.split_once('#').unwrap_or((target.as_str(), ""));
            let anchor_doc = if path.is_empty() {
                doc
            } else {
                assert!(root.join(path).exists(), "{doc}:{line}: link to missing {path}");
                path
            };
            if !anchor.is_empty() {
                let known = doc_anchors
                    .get(anchor_doc)
                    .unwrap_or_else(|| panic!("{doc}:{line}: anchors are only checked in {DOCS:?}: {target}"));
                assert!(
                    known.contains(anchor),
                    "{doc}:{line}: no heading for #{anchor} in {anchor_doc}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 20, "only {checked} links found; is the link parser broken?");
}

#[test]
fn doc_parsers_handle_fences_spans_and_anchors() {
    let text = "# Title: `[radio]` and RNode_Firmware\n\
                \n\
                Use ```` ```toml ignore ```` and [a link](#title-radio-and-rnode_firmware).\n\
                \n   ```toml\n   [radio]\n   driver = \"sim\"\n   ```\n\
                ```toml ignore\n[package]\n```\n\
                ## Title: `[radio]` and RNode_Firmware\n";
    let blocks = code_blocks("test", text);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].info, "toml");
    assert_eq!(blocks[0].body, "[radio]\ndriver = \"sim\"\n");
    assert_eq!(blocks[1].info, "toml ignore");
    assert_eq!(check_toml_blocks("test", text), 1);
    assert_eq!(
        link_targets("test", text),
        [(3, "#title-radio-and-rnode_firmware".to_owned())]
    );
    let anchors = anchors("test", text);
    assert!(anchors.contains("title-radio-and-rnode_firmware"), "{anchors:?}");
    assert!(anchors.contains("title-radio-and-rnode_firmware-1"), "{anchors:?}");
}
