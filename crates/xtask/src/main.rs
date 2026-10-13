use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rnode_core::kiss::{MTU, SINGLE_MTU};
use rnode_core::phy::{self, Modulation, PhyParams};

const FREQUENCY_HZ: u32 = 914_875_000;
const BANDWIDTH_HZ: u32 = 125_000;
const TX_POWER_DBM: u8 = 2;
const CODING_RATE: u8 = 5;
const PY_REQUIREMENTS: &[&str] = &["pyserial", "cryptography"];

fn main() {
    if let Err(err) = real_main() {
        eprintln!("xtask: {err}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("rns-e2e") => rns_e2e(args.collect()),
        _ => {
            eprintln!("usage: cargo xtask rns-e2e [--reticulum PATH] [--keep-logs]");
            Ok(())
        }
    }
}

fn rns_e2e(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let root = workspace_root()?;
    let mut reticulum = env::var_os("RNODE_RETICULUM")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("..").join("Reticulum"));
    let mut keep_logs = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--reticulum" => {
                i += 1;
                reticulum = args.get(i).ok_or("--reticulum requires a path")?.into();
            }
            "--keep-logs" => keep_logs = true,
            other => return Err(format!("unknown rns-e2e argument {other:?}").into()),
        }
        i += 1;
    }
    // A relative --reticulum or $RNODE_RETICULUM is relative to where xtask was started, and
    // the Python processes need an absolute PYTHONPATH.
    let reticulum = env::current_dir()?.join(reticulum);
    env::set_current_dir(&root)?;
    if !reticulum.join("RNS").is_dir() {
        return Err(format!("Reticulum checkout not found at {}", reticulum.display()).into());
    }

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    run(
        &root,
        Command::new(cargo).args(["build", "--locked", "-p", "rnode-linux", "--bin", "rnode-linux"]),
    )?;
    let daemon = daemon_binary()?;

    let base_run_dir = PathBuf::from("target")
        .join("rns-e2e")
        .join("runs")
        .join(unique_run_name());
    fs::create_dir_all(&base_run_dir)?;
    let uv = find_uv()?;
    let venv = root.join("target").join("rns-e2e").join("venv");
    let requirements_marker = venv.join(".rnode-linux-e2e-requirements");
    let requirements_text = PY_REQUIREMENTS.join("\n");
    if !venv.join("pyvenv.cfg").exists() {
        run(
            &root,
            Command::new(&uv).args(["venv", venv.to_str().ok_or("bad venv path")?]),
        )?;
    }
    if fs::read_to_string(&requirements_marker).ok().as_deref() != Some(requirements_text.as_str()) {
        let python = venv.join("bin/python");
        let mut cmd = Command::new(&uv);
        cmd.args(["pip", "install", "--python", python.to_str().ok_or("bad python path")?])
            .args(PY_REQUIREMENTS);
        run(&root, &mut cmd)?;
        fs::write(requirements_marker, requirements_text)?;
    }

    let scenarios = [Scenario { name: "sf7", sf: 7 }, Scenario { name: "sf11", sf: 11 }];
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        for scenario in scenarios {
            run_scenario(&base_run_dir, &daemon, &venv, &reticulum, scenario)?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            if keep_logs {
                println!("rns-e2e passed; logs kept in {}", base_run_dir.display());
            } else {
                fs::remove_dir_all(&base_run_dir)?;
                println!("rns-e2e passed");
            }
            Ok(())
        }
        Err(err) => {
            eprintln!("rns-e2e failed; logs in {}", base_run_dir.display());
            dump_logs(&base_run_dir);
            Err(err)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Scenario {
    name: &'static str,
    sf: u8,
}

impl Scenario {
    /// How long the processes get to exit after SIGTERM. A daemon finishes the packet on the air
    /// before it exits (#51), so this covers the longest one, `MTU` bytes in two full
    /// `SINGLE_MTU`-byte LoRa frames, at the scenario's modulation, with half again as margin, plus
    /// the 3 s the processes need to exit. rnode-core's airtime is the firmware's estimate; the
    /// simulated air uses Semtech's time on air, which is a few percent longer.
    fn sigterm_wait(self) -> Duration {
        let modulation = Modulation {
            bandwidth_hz: BANDWIDTH_HZ,
            spreading_factor: self.sf,
            coding_rate: CODING_RATE,
        };
        let phy = PhyParams::new(modulation);
        let low_data_rate = phy::low_data_rate(self.sf, BANDWIDTH_HZ);
        let frame_ms = phy::airtime_ms(
            SINGLE_MTU,
            modulation,
            low_data_rate,
            phy.preamble_symbols,
            phy.symbol_time_ms,
        );
        // Each frame carries a header byte and `SINGLE_MTU - 1` bytes of the packet.
        let frames = MTU.div_ceil(SINGLE_MTU - 1) as f64;
        let packet = Duration::from_secs_f64(frames * f64::from(frame_ms) / 1000.0);
        packet + packet / 2 + Duration::from_secs(3)
    }
}

fn run_scenario(
    base_run_dir: &Path,
    daemon: &Path,
    venv: &Path,
    reticulum: &Path,
    scenario: Scenario,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("running RNS e2e scenario {} (SF{}/125kHz)", scenario.name, scenario.sf);
    let run_dir = base_run_dir.join(scenario.name);
    fs::create_dir_all(&run_dir)?;
    let air_dir = run_dir.join("air");
    create_private_dir(&air_dir)?;
    write_python_scripts(&run_dir)?;
    let node_a = NodePaths::new(&run_dir, "a");
    let node_b = NodePaths::new(&run_dir, "b");
    write_daemon_config(&node_a, &air_dir, -58, 9.0)?;
    write_daemon_config(&node_b, &air_dir, -64, 6.5)?;
    write_rns_config(&node_a, scenario.sf)?;
    write_rns_config(&node_b, scenario.sf)?;

    let mut children = Children::default();
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        children.spawn_logged(
            "daemon-a",
            Command::new(daemon).arg("--config").arg(&node_a.daemon_config),
            &node_a.log,
            Some(node_a.pty.clone()),
        )?;
        children.spawn_logged(
            "daemon-b",
            Command::new(daemon).arg("--config").arg(&node_b.daemon_config),
            &node_b.log,
            Some(node_b.pty.clone()),
        )?;
        wait_for_path(&node_a.pty, Duration::from_secs(10), "daemon A PTY")?;
        wait_for_path(&node_b.pty, Duration::from_secs(10), "daemon B PTY")?;

        let py = venv.join("bin/python");
        let mut kiss = Command::new(&py);
        kiss.arg(run_dir.join("kiss_boundary.py"))
            .arg(&node_a.pty)
            .arg(&node_b.pty)
            .arg(FREQUENCY_HZ.to_string())
            .arg(BANDWIDTH_HZ.to_string())
            .arg(TX_POWER_DBM.to_string())
            .arg(scenario.sf.to_string())
            .arg(CODING_RATE.to_string());
        let kiss_timeout = if scenario.sf >= 11 { 180 } else { 60 };
        let status = run_with_timeout(&mut kiss, Duration::from_secs(kiss_timeout), "KISS boundary probe")?;
        if !status.success() {
            return Err(format!("KISS boundary probe failed with {status}").into());
        }

        let mut server = Command::new(&py);
        server
            .arg("-u")
            .arg(run_dir.join("rns_server.py"))
            .arg(&node_b.rns_config_dir)
            .arg(&run_dir);
        server.env("PYTHONPATH", reticulum);
        children.spawn_logged("rns-server", &mut server, &run_dir.join("rns-server.log"), None)?;
        wait_for_path(
            &run_dir.join("server.hash"),
            Duration::from_secs(30),
            "server announce file",
        )?;

        let mut client = Command::new(&py);
        client
            .arg("-u")
            .arg(run_dir.join("rns_client.py"))
            .arg(&node_a.rns_config_dir)
            .arg(&run_dir)
            .arg(scenario.sf.to_string());
        client.env("PYTHONPATH", reticulum);
        let client_log_path = run_dir.join("rns-client.log");
        let client_log = fs::File::create(&client_log_path)?;
        client
            .stdout(Stdio::from(client_log.try_clone()?))
            .stderr(Stdio::from(client_log));
        let timeout = if scenario.sf >= 11 { 600 } else { 180 };
        let status = run_with_timeout(&mut client, Duration::from_secs(timeout), "RNS client")?;
        if !status.success() {
            return Err(format!("RNS client failed with {status}").into());
        }
        wait_for_path(
            &run_dir.join("server.done"),
            Duration::from_secs(30),
            "server completion",
        )?;
        let timings_path = run_dir.join("timings.json");
        if let Ok(timings) = fs::read_to_string(&timings_path) {
            println!("{} timings: {}", scenario.name, timings.trim());
        }
        Ok(())
    })();
    if result.is_err() {
        dump_log_tails(&run_dir, 40);
    }
    let shutdown_result = children.terminate_all(scenario.sigterm_wait());
    result.and(shutdown_result)
}

#[derive(Debug)]
struct NodePaths {
    dir: PathBuf,
    daemon_config: PathBuf,
    rns_config_dir: PathBuf,
    pty: PathBuf,
    log: PathBuf,
}

impl NodePaths {
    fn new(run_dir: &Path, name: &str) -> Self {
        let dir = run_dir.join(name);
        Self {
            daemon_config: dir.join("daemon.toml"),
            rns_config_dir: dir.join("rns"),
            pty: dir.join("pty"),
            log: dir.join("daemon.log"),
            dir,
        }
    }
}

fn write_daemon_config(node: &NodePaths, air_dir: &Path, rssi: i16, snr: f32) -> io::Result<()> {
    fs::create_dir_all(&node.dir)?;
    let name = node.dir.file_name().and_then(|s| s.to_str()).unwrap_or("node");
    let settings = node.dir.join("radio-settings.toml");
    fs::write(
        &node.daemon_config,
        daemon_config(air_dir, name, &node.pty, &settings, rssi, snr),
    )
}

/// An rnode-linux config for the simulated radio (see `crates/rnode-linux/src/config.rs`).
fn daemon_config(air_dir: &Path, node_name: &str, pty: &Path, settings: &Path, rssi: i16, snr: f32) -> String {
    format!(
        r#"[radio]
driver = "sim"

[radio.sim]
air_dir = {air_dir}
node_name = {node_name}
rssi_dbm = {rssi}
snr_db = {snr:?}
loss = 0.0

[pty]
symlink = {pty}

[storage]
radio_settings = {settings}

[logging]
log_level = "debug"
"#,
        air_dir = toml_string(&air_dir.to_string_lossy()),
        node_name = toml_string(node_name),
        pty = toml_string(&pty.to_string_lossy()),
        settings = toml_string(&settings.to_string_lossy()),
    )
}

/// A TOML basic string holding `text`.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_rns_config(node: &NodePaths, sf: u8) -> io::Result<()> {
    fs::create_dir_all(&node.rns_config_dir)?;
    fs::write(
        node.rns_config_dir.join("config"),
        format!(
            r#"[reticulum]
enable_transport = yes
share_instance = No
panic_on_interface_error = Yes

[logging]
loglevel = 6

[interfaces]
  [[rnode-linux-sim]]
    type = RNodeInterface
    enabled = Yes
    port = {}
    frequency = {FREQUENCY_HZ}
    bandwidth = {BANDWIDTH_HZ}
    txpower = {TX_POWER_DBM}
    spreadingfactor = {sf}
    codingrate = {CODING_RATE}
"#,
            node.pty.display()
        ),
    )
}

fn write_python_scripts(run_dir: &Path) -> io::Result<()> {
    fs::write(run_dir.join("kiss_boundary.py"), KISS_BOUNDARY_PY)?;
    fs::write(run_dir.join("rns_server.py"), SERVER_PY)?;
    fs::write(run_dir.join("rns_client.py"), CLIENT_PY)?;
    Ok(())
}

const KISS_BOUNDARY_PY: &str = r#"
import struct, sys, time
import serial
FEND=0xC0; FESC=0xDB; TFEND=0xDC; TFESC=0xDD
CMD_DATA=0x00; CMD_FREQUENCY=0x01; CMD_BANDWIDTH=0x02; CMD_TXPOWER=0x03; CMD_SF=0x04; CMD_CR=0x05; CMD_RADIO_STATE=0x06; CMD_DETECT=0x08; CMD_STAT_RSSI=0x23; CMD_STAT_SNR=0x24; CMD_STAT_CHTM=0x25; CMD_STAT_PHYPRM=0x26; CMD_STAT_CSMA=0x28; CMD_ERROR=0x90; DETECT_REQ=0x73; DETECT_RESP=0x46

def esc(data):
    out=bytearray()
    for b in data:
        if b==FEND: out += bytes([FESC,TFEND])
        elif b==FESC: out += bytes([FESC,TFESC])
        else: out.append(b)
    return bytes(out)

def frame(cmd, payload=b''):
    return bytes([FEND,cmd])+esc(payload)+bytes([FEND])

class KissPort:
    def __init__(self, path):
        self.port=serial.Serial(path, 115200, timeout=0)
        self.in_frame=False; self.cmd=None; self.data=bytearray(); self.escp=False
    def close(self): self.port.close()
    def write(self, data): self.port.write(data); self.port.flush()
    def read_frames(self, deadline):
        frames=[]
        while time.time()<deadline:
            chunk=self.port.read(4096)
            if not chunk:
                time.sleep(0.01); continue
            for b in chunk:
                if b==FEND:
                    if self.in_frame and self.cmd is not None:
                        frames.append((self.cmd,bytes(self.data)))
                        self.in_frame=False; self.cmd=None; self.data.clear(); self.escp=False
                    else:
                        self.in_frame=True; self.cmd=None; self.data.clear(); self.escp=False
                    continue
                if not self.in_frame: continue
                if self.cmd is None:
                    self.cmd=b; continue
                if b==FESC:
                    self.escp=True; continue
                if self.escp:
                    if b==TFEND: b=FEND
                    elif b==TFESC: b=FESC
                    self.escp=False
                if self.cmd==CMD_DATA:
                    self.data.append(b)
                else:
                    self.data.append(b)
        return frames

def configure(port, freq, bw, txp, sf, cr):
    port.write(b''.join([
        frame(CMD_DETECT, bytes([DETECT_REQ])),
        frame(CMD_FREQUENCY, struct.pack('>I', freq)),
        frame(CMD_BANDWIDTH, struct.pack('>I', bw)),
        frame(CMD_TXPOWER, bytes([txp])),
        frame(CMD_SF, bytes([sf])),
        frame(CMD_CR, bytes([cr])),
        frame(CMD_RADIO_STATE, bytes([1])),
    ]))
    frames=port.read_frames(time.time()+3)
    if (CMD_DETECT, bytes([DETECT_RESP])) not in frames:
        raise RuntimeError(f'detect response missing: {frames!r}')
    if not any(cmd == CMD_STAT_PHYPRM and len(data) == 12 for cmd,data in frames):
        raise RuntimeError(f'PHYPRM push missing after configuration/radio-on: {frames!r}')
    for cmd,data in frames:
        if cmd == CMD_STAT_CHTM and len(data) != 11:
            raise RuntimeError(f'bad CHTM length {len(data)}: {data.hex()}')
        if cmd == CMD_STAT_PHYPRM and len(data) != 12:
            raise RuntimeError(f'bad PHYPRM length {len(data)}: {data.hex()}')
        if cmd == CMD_STAT_CSMA and len(data) != 3:
            raise RuntimeError(f'bad CSMA length {len(data)}: {data.hex()}')
    return frames

def drain(port): port.read_frames(time.time()+0.25)
def send(port, payload): port.write(frame(CMD_DATA, payload))
def describe(frames):
    # Data frames by length only: they can be 508 bytes long.
    return [(cmd, len(data)) if cmd == CMD_DATA else (cmd, data) for cmd,data in frames]

def expect(port, payload, timeout):
    end=time.time()+timeout; seen=[]
    while time.time()<end:
        for cmd,data in port.read_frames(min(end, time.time()+0.25)):
            if cmd == CMD_DATA and data == payload:
                if [c for c,_ in seen[-2:]] != [CMD_STAT_RSSI, CMD_STAT_SNR]:
                    raise RuntimeError(f'missing RSSI/SNR immediately before data len {len(payload)}: {describe(seen[-4:])}')
                return
            seen.append((cmd, data))
    raise RuntimeError(f'missing payload len {len(payload)}, saw {describe(seen)}')

def expect_absent(port, payload, timeout):
    end=time.time()+timeout; seen=[]
    while time.time()<end:
        seen += port.read_frames(min(end, time.time()+0.25))
    if any(cmd == CMD_DATA for cmd,_ in seen):
        raise RuntimeError(f'over-MTU payload produced CMD_DATA frames: {describe(seen)}')

def expect_no_query_reply(port, cmd):
    drain(port)
    port.write(frame(cmd, b'\xff'))
    frames=port.read_frames(time.time()+0.5)
    if any(c == cmd for c,_ in frames):
        raise RuntimeError(f'host query 0x{cmd:02x} unexpectedly replied: {frames!r}')

def main():
    a_path,b_path,freq,bw,txp,sf,cr=sys.argv[1],sys.argv[2],int(sys.argv[3]),int(sys.argv[4]),int(sys.argv[5]),int(sys.argv[6]),int(sys.argv[7])
    with_a=KissPort(a_path); with_b=KissPort(b_path)
    try:
        a=with_a; b=with_b
        configure(a, freq, bw, txp, sf, cr); frames=configure(b, freq, bw, txp, sf, cr)
        frames += b.read_frames(time.time()+3)
        if not any(cmd == CMD_STAT_CHTM and len(data) == 11 for cmd,data in frames):
            raise RuntimeError(f'CHTM push missing while online: {frames!r}')
        for cmd,data in frames:
            if cmd == CMD_STAT_CSMA and len(data) != 3:
                raise RuntimeError(f'bad CSMA push length {len(data)}: {data.hex()}')
        for p in (a,b):
            p.write(frame(CMD_RADIO_STATE, bytes([0])))
        drain(a); drain(b)
        for cmd in (CMD_STAT_CHTM, CMD_STAT_PHYPRM, CMD_STAT_CSMA):
            expect_no_query_reply(a, cmd)
        for p in (a,b):
            p.write(frame(CMD_RADIO_STATE, bytes([1])))
        drain(a); drain(b)
        timeout=20 if sf >= 11 else 8
        for size in (1, 254, 255, 508):
            payload=bytes([x%251 for x in range(size)])
            send(a, payload); expect(b, payload, timeout)
            send(b, payload[::-1]); expect(a, payload[::-1], timeout)
        # Firmware queues over-MTU host CMD_DATA, but flush_queue()/pop_queue() silently drop
        # lengths greater than MTU (508) without CMD_ERROR.
        over=bytes([x%251 for x in range(509)])
        send(a, over)
        expect_absent(b, over, timeout)
        seen_a=a.read_frames(time.time()+0.5)
        if any(cmd == 0x90 for cmd,_ in seen_a):
            raise RuntimeError(f'over-MTU payload emitted CMD_ERROR: {seen_a!r}')
        follow=b'after-over-mtu'
        send(a, follow); expect(b, follow, timeout)
        a.write(frame(CMD_RADIO_STATE, bytes([0]))); b.write(frame(CMD_RADIO_STATE, bytes([0])))
    finally:
        with_a.close(); with_b.close()
    print('KISS boundary probe passed')

if __name__ == '__main__':
    main()
"#;

const SERVER_PY: &str = r#"
import hashlib, pathlib, sys, time, traceback
import RNS
APP='rnode_linux_e2e'
latest=None
packets=[]
resources=[]
errors=[]
run=pathlib.Path(sys.argv[2])

def record_error(message):
    errors.append(message)
    (run/'server.error').write_text(message)

def connected(link):
    global latest
    latest=link
    link.set_packet_callback(packet_received)
    link.set_resource_strategy(RNS.Link.ACCEPT_ALL)
    link.set_resource_concluded_callback(resource_received)

def packet_received(data, packet):
    try:
        packets.append(data)
        if packet.rssi is None or packet.snr is None:
            raise RuntimeError('missing packet RSSI/SNR')
        # Packets received by daemon B report daemon B's configured sim metadata.
        if packet.rssi != -64 or packet.snr != 6.5:
            raise RuntimeError(f'unexpected server-side RSSI/SNR {packet.rssi}/{packet.snr}, expected -64/6.5')
        RNS.Packet(latest, data).send()
    except Exception as e:
        record_error('packet callback failed: '+str(e)+'\n'+traceback.format_exc())

def resource_received(resource):
    try:
        if resource.status != RNS.Resource.COMPLETE:
            raise RuntimeError('resource receive failed')
        data = resource.data.read()
        resources.append(hashlib.sha256(data).hexdigest())
        (run/'server.resource.sha256').write_text(resources[-1])
    except Exception as e:
        record_error('resource callback failed: '+str(e)+'\n'+traceback.format_exc())

reticulum=RNS.Reticulum(configdir=sys.argv[1], loglevel=RNS.LOG_DEBUG)
identity=RNS.Identity()
dest=RNS.Destination(identity, RNS.Destination.IN, RNS.Destination.SINGLE, APP, 'server')
dest.set_link_established_callback(connected)
(run/'server.hash').write_text(dest.hash.hex())
start=time.time()
while time.time()-start < 590:
    if errors:
        raise SystemExit(errors[0])
    if (run/'client.done').exists() and len(packets) >= 4 and resources:
        (run/'server.done').write_text('ok')
        sys.exit(0)
    time.sleep(0.1)
raise SystemExit('server timed out')
"#;

const CLIENT_PY: &str = r#"
import hashlib, json, pathlib, sys, time, traceback
import RNS
APP='rnode_linux_e2e'
run=pathlib.Path(sys.argv[2])
received=[]
resource_done=False
errors=[]

def record_error(message):
    errors.append(message)
    (run/'client.error').write_text(message)

def wait(cond, timeout, what):
    start=time.time()
    while time.time()-start < timeout:
        if errors:
            raise RuntimeError(errors[0])
        for path in (run/'client.error', run/'server.error'):
            if path.exists():
                raise RuntimeError(path.read_text())
        if cond(): return
        time.sleep(0.1)
    raise RuntimeError('timeout waiting for '+what)

def packet_received(data, packet):
    try:
        if packet.rssi is None or packet.snr is None:
            raise RuntimeError('missing packet RSSI/SNR')
        # Echoes received by daemon A report daemon A's configured sim metadata.
        if packet.rssi != -58 or packet.snr != 9.0:
            raise RuntimeError(f'unexpected client-side RSSI/SNR {packet.rssi}/{packet.snr}, expected -58/9.0')
        received.append(data)
    except Exception as e:
        record_error('packet callback failed: '+str(e)+'\n'+traceback.format_exc())

def resource_sent(resource):
    global resource_done
    try:
        if resource.status != RNS.Resource.COMPLETE:
            raise RuntimeError('resource send failed')
        resource_done=True
    except Exception as e:
        record_error('resource callback failed: '+str(e)+'\n'+traceback.format_exc())

reticulum=RNS.Reticulum(configdir=sys.argv[1], loglevel=RNS.LOG_DEBUG)
sf=int(sys.argv[3])
hash_path=run/'server.hash'
wait(hash_path.exists, 20, 'server hash')
dest_hash=bytes.fromhex(hash_path.read_text().strip())
if not RNS.Transport.has_path(dest_hash):
    RNS.Transport.request_path(dest_hash)
path_start=time.monotonic(); last_request=path_start
while not RNS.Transport.has_path(dest_hash):
    if errors:
        raise RuntimeError(errors[0])
    for path in (run/'client.error', run/'server.error'):
        if path.exists():
            raise RuntimeError(path.read_text())
    time.sleep(0.1)
    now=time.monotonic()
    if now-path_start > 180:
        raise RuntimeError('timeout waiting for server announce/path')
    if now-last_request >= 15:
        RNS.Transport.request_path(dest_hash)
        last_request=now
identity=RNS.Identity.recall(dest_hash)
dest=RNS.Destination(identity, RNS.Destination.OUT, RNS.Destination.SINGLE, APP, 'server')
timings={'sf': sf, 'echoes': []}
def write_timings():
    (run/'timings.json').write_text(json.dumps(timings, indent=2, sort_keys=True))

timings['path_s']=time.monotonic()-path_start
write_timings()
link_start=time.monotonic()
link=RNS.Link(dest)
link.set_packet_callback(packet_received)
wait(lambda: link.status == RNS.Link.ACTIVE or link.status == RNS.Link.CLOSED, 180, 'link active')
if link.status == RNS.Link.CLOSED:
    raise RuntimeError('link closed before ACTIVE (RNS establishment timeout)')
timings['link_establishment_s']=time.monotonic()-link_start
write_timings()
for payload in [b'a', bytes(range(64)), bytes([x%251 for x in range(240)]), b'bidirectional-check'*8]:
    sent=time.monotonic()
    RNS.Packet(link, payload).send()
    wait(lambda p=payload: p in received, 180, 'echo '+str(len(payload)))
    timings['echoes'].append({'len': len(payload), 'rtt_s': time.monotonic()-sent})
    write_timings()
data=bytes([x%251 for x in range(4096)])
sha=hashlib.sha256(data).hexdigest()
resource_start=time.monotonic()
RNS.Resource(data, link, metadata={'name':'e2e'}, callback=resource_sent, auto_compress=False)
wait(lambda: resource_done, 300, 'resource send')
wait(lambda: (run/'server.resource.sha256').exists(), 30, 'server resource hash')
timings['resource_s']=time.monotonic()-resource_start
write_timings()
if (run/'server.resource.sha256').read_text().strip() != sha:
    raise RuntimeError('resource hash mismatch')
iface=next((i for i in RNS.Transport.interfaces if i.__class__.__name__ == 'RNodeInterface'), None)
if iface is None:
    raise RuntimeError('RNodeInterface not found')
missing=[name for name in ('r_current_rssi','r_noise_floor','r_symbol_time_ms','r_preamble_symbols','r_csma_slot_time_ms') if getattr(iface, name, None) is None]
if missing:
    raise RuntimeError('RNodeInterface did not receive CHTM/PHYPRM fields: '+','.join(missing))
expected_symbol_time_ms=(2**sf)/125000*1000
if abs(iface.r_symbol_time_ms-expected_symbol_time_ms) > 0.001:
    raise RuntimeError(f'unexpected symbol time {iface.r_symbol_time_ms}, expected {expected_symbol_time_ms}')
(run/'client.done').write_text('ok')
"#;

#[derive(Default)]
struct Children(Vec<ChildProcess>);

struct ChildProcess {
    name: String,
    child: Child,
    pty_symlink: Option<PathBuf>,
}

impl Children {
    fn spawn_logged(
        &mut self,
        name: &str,
        cmd: &mut Command,
        log: &Path,
        pty_symlink: Option<PathBuf>,
    ) -> io::Result<()> {
        if let Some(parent) = log.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::File::create(log)?;
        let child = cmd
            .stdout(Stdio::from(file.try_clone()?))
            .stderr(Stdio::from(file))
            .spawn()?;
        self.0.push(ChildProcess {
            name: name.to_owned(),
            child,
            pty_symlink,
        });
        Ok(())
    }

    /// SIGTERM every process that is still running, wait up to `sigterm_wait` for all of them
    /// to exit, then kill the rest. Fails if a daemon had to be killed, exited with an error or
    /// left its PTY symlink behind.
    fn terminate_all(&mut self, sigterm_wait: Duration) -> Result<(), Box<dyn std::error::Error>> {
        let mut errors = Vec::new();
        for proc in &mut self.0 {
            if proc.child.try_wait()?.is_none() {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(proc.child.id().to_string())
                    .status();
            }
        }
        let deadline = Instant::now() + sigterm_wait;
        for proc in &mut self.0 {
            let status = loop {
                if let Some(status) = proc.child.try_wait()? {
                    break status;
                }
                if Instant::now() >= deadline {
                    let _ = proc.child.kill();
                    let status = proc.child.wait()?;
                    errors.push(format!(
                        "{} did not exit after SIGTERM; killed with {status}",
                        proc.name
                    ));
                    break status;
                }
                thread::sleep(Duration::from_millis(50));
            };
            if proc.pty_symlink.is_some() && !status.success() {
                errors.push(format!("{} exited with {status}", proc.name));
            }
            if let Some(path) = &proc.pty_symlink {
                // The link itself, not its target: once the daemon has exited, the PTY slave the
                // link points at is gone, so a leftover link dangles and `exists()` misses it.
                if fs::symlink_metadata(path).is_ok() {
                    errors.push(format!("{} did not remove PTY symlink {}", proc.name, path.display()));
                }
            }
        }
        self.0.clear();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; ").into())
        }
    }
}

/// A safety net for a scenario that ends without `terminate_all()`, which checks how the
/// processes exit; this only makes sure none is left running.
impl Drop for Children {
    fn drop(&mut self) {
        for proc in &mut self.0 {
            if matches!(proc.child.try_wait(), Ok(None)) {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(proc.child.id().to_string())
                    .status();
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        for proc in &mut self.0 {
            loop {
                match proc.child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                    _ => {
                        let _ = proc.child.kill();
                        let _ = proc.child.wait();
                        break;
                    }
                }
            }
        }
        self.0.clear();
    }
}

fn workspace_root() -> io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "workspace root not found"))
}

/// The daemon `cargo build` just built. `cargo xtask` runs this executable from the same profile
/// directory, so unlike `<root>/target/debug` this follows `CARGO_TARGET_DIR` and
/// `build.target-dir`.
fn daemon_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let xtask = env::current_exe()?;
    let daemon = xtask
        .parent()
        .ok_or("the xtask executable has no parent directory")?
        .join("rnode-linux");
    if daemon.is_file() {
        Ok(daemon)
    } else {
        Err(format!(
            "rnode-linux was not built next to xtask: {} is missing",
            daemon.display()
        )
        .into())
    }
}

fn find_uv() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = env::var_os("UV").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("UV points to missing executable: {}", path.display()).into());
    }
    let Some(path_var) = env::var_os("PATH") else {
        return Err("uv is required for the Reticulum e2e harness; set UV or add uv to PATH".into());
    };
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join("uv");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err("uv is required for the Reticulum e2e harness; set UV or add uv to PATH".into())
}

fn unique_run_name() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("r{}-{}", millis % 1_000_000_000, std::process::id())
}

fn run(cwd: &Path, cmd: &mut Command) -> Result<(), Box<dyn std::error::Error>> {
    let status = cmd.current_dir(cwd).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("command failed with {status}").into())
    }
}

fn run_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    what: &str,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let mut child = cmd.spawn()?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{what} timed out after {timeout:?}").into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_path(path: &Path, timeout: Duration, what: &str) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("timed out waiting for {what}: {}", path.display()).into())
}

/// Create `path`, and any missing parents, private to its owner (mode 0700) as rnode-sim requires
/// of its air directory. `mkdir(2)` applies the mode, so the directory is never open to others,
/// as it was between `create_dir_all()` and a later `chmod` (#60). An existing directory that is
/// not private is refused, not changed.
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    let mode = metadata.permissions().mode() & 0o777;
    if !metadata.is_dir() || mode != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a directory private to its owner (mode {mode:04o})",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn dump_logs(run_dir: &Path) {
    let mut stack = vec![run_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("log") {
                let _ = writeln!(io::stderr(), "\n===== {} =====", path.display());
                if let Ok(text) = fs::read_to_string(&path) {
                    eprintln!("{text}");
                }
            }
        }
    }
}

fn dump_log_tails(run_dir: &Path, lines: usize) {
    let mut stack = vec![run_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if matches!(path.extension().and_then(|s| s.to_str()), Some("log" | "error")) {
                let _ = writeln!(io::stderr(), "\n===== tail {} =====", path.display());
                if let Ok(text) = fs::read_to_string(&path) {
                    let all: Vec<&str> = text.lines().collect();
                    for line in all.iter().skip(all.len().saturating_sub(lines)) {
                        eprintln!("{line}");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
