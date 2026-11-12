#[cfg(test)]
#[path = "../tests/acceptance.rs"]
mod acceptance;
mod boost;
mod boostlearn;
mod br_watchdog;
#[cfg(test)]
mod br_watchdog_tests;
mod capture;
mod config;
mod devices;
mod experiment;
#[cfg(test)]
mod experiment_tests;
mod explore;
mod failsafe;
mod context;
mod dynamics;
#[cfg(test)]
mod context_tests;
mod http;
mod intent;
mod infer;
mod inventory;
mod lake;
#[cfg(test)]
mod lake_tests;
mod migrate;
mod node_health;
#[cfg(test)]
mod node_health_tests;
mod report;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod dynamics_tests;
mod manual;
#[cfg(test)]
mod manual_tests;
mod restore;
mod rules;
mod status;
mod storage;
mod watchmatch;
#[cfg(test)]
mod watchmatch_tests;
mod window;
#[cfg(test)]
mod supply_tests;
#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod window_tests;
mod window_model;
#[cfg(test)]
mod window_model_tests;

use anyhow::{anyhow, bail, Context, Result};
use config::{Config, ControllerKind};
use matter_controller::{ChipToolController, DeviceFeed, MatterController, MockController, StorageLock, SwitchOn};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: heatd [serve] | commission <node-id> <pairing-code> | read <node> | set <node> <celsius> | inventory <node> | migrate | export --room <room> --from <date> --to <date> [--out <path>] | export --view step_responses|room_delta|room_delta_daily|radiator_summary|radiator_pairs [--room <room>] [--from <date>] [--to <date>] [--out <path>] | report [date]| thread form <serial-port> | thread dataset <serial-port>";
const OT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const OT_LEADER_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq)]
pub enum ConsoleLine {
    Skip,
    Done,
    Error(String),
    Data(String),
}

pub fn classify_line(raw: &str, command: &str) -> ConsoleLine {
    let clean = matter_controller::strip_ansi(raw);
    let clean = clean.trim();
    let mut text = clean;
    while let Some((_, rest)) = text.split_once("> ").filter(|(p, _)| !p.contains(' ')) {
        text = rest.trim_start();
    }
    let text = if text.ends_with('>') && !text.contains(' ') { "" } else { text.trim() };
    let is_log = matches!(text.as_bytes(), [b'I' | b'W' | b'E' | b'D' | b'V', b' ', b'(', ..] | [b'I' | b'W' | b'E' | b'D' | b'V', b'(', ..]);
    if text.is_empty() || text == command || is_log {
        ConsoleLine::Skip
    } else if text == "Done" {
        ConsoleLine::Done
    } else if text.starts_with("Error") {
        ConsoleLine::Error(text.to_string())
    } else {
        ConsoleLine::Data(text.to_string())
    }
}

pub struct OtCli<P: Read + Write> {
    port: P,
    prefix: String,
    pending: Vec<u8>,
}

impl<P: Read + Write> OtCli<P> {
    pub fn new(port: P, prefix: &str) -> Self {
        Self { port, prefix: prefix.to_string(), pending: Vec::new() }
    }

    fn read_line(&mut self, deadline: Instant) -> Result<String> {
        let mut byte = [0u8; 1];
        loop {
            if let Some(i) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=i).collect();
                return Ok(String::from_utf8_lossy(&line).into_owned());
            }
            if Instant::now() > deadline {
                bail!("console timeout");
            }
            match self.port.read(&mut byte) {
                Ok(0) => bail!("console closed"),
                Ok(_) => self.pending.push(byte[0]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn exec(&mut self, command: &str, timeout: Duration) -> Result<Vec<String>> {
        self.pending.clear();
        self.port.write_all(format!("{}{}\r\n", self.prefix, command).as_bytes())?;
        self.port.flush()?;
        let deadline = Instant::now() + timeout;
        let mut data = Vec::new();
        loop {
            match classify_line(&self.read_line(deadline)?, &format!("{}{}", self.prefix, command)) {
                ConsoleLine::Skip => {}
                ConsoleLine::Done => return Ok(data),
                ConsoleLine::Error(e) => bail!("{command}: {e}"),
                ConsoleLine::Data(d) => data.push(d),
            }
        }
    }

    pub fn active_dataset(&mut self) -> Result<String> {
        self.exec("dataset active -x", OT_COMMAND_TIMEOUT)?
            .into_iter()
            .find(|l| l.len() >= 4 && l.len() % 2 == 0 && l.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| anyhow!("no dataset hex in console output"))
    }

    pub fn form_network(&mut self, leader_timeout: Duration, poll: Duration) -> Result<String> {
        for command in ["dataset init new", "dataset commit active", "ifconfig up", "thread start"] {
            self.exec(command, OT_COMMAND_TIMEOUT)?;
        }
        let deadline = Instant::now() + leader_timeout;
        while self.exec("state", OT_COMMAND_TIMEOUT)?.last().map(String::as_str) != Some("leader") {
            if Instant::now() > deadline {
                bail!("thread did not become leader");
            }
            std::thread::sleep(poll);
        }
        self.active_dataset()
    }
}

fn open_cli(port: &str, config: &Config) -> Result<OtCli<Box<dyn serialport::SerialPort>>> {
    let port = serialport::new(port, 115_200).timeout(Duration::from_millis(200)).dtr_on_open(false).open().with_context(|| format!("open {port}"))?;
    Ok(OtCli::new(port, &config.thread_cli_prefix))
}

fn save_dataset(config: &Config, hex: &str) -> Result<()> {
    if let Some(parent) = Path::new(&config.dataset_path).parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&config.dataset_path, format!("{hex}\n"))?;
    println!("dataset written to {}", config.dataset_path);
    Ok(())
}

fn lock_storage(config: &Config) -> Result<Option<StorageLock>> {
    match config.controller {
        ControllerKind::ChipTool => Ok(Some(config.chip_tool.to_controller_config().lock_storage()?)),
        ControllerKind::Mock => Ok(None),
    }
}

struct Built {
    controller: Arc<dyn MatterController>,
    mock: Option<Arc<MockController>>,
    feed: Arc<dyn DeviceFeed>,
    switch: Arc<dyn SwitchOn>,
}

fn build_controller(config: &Config, serving: bool) -> Built {
    match config.controller {
        ControllerKind::ChipTool => {
            let chip_tool = config.chip_tool.to_controller_config();
            let controller = Arc::new(if serving { ChipToolController::serving(chip_tool) } else { ChipToolController::new(chip_tool) });
            Built { controller: controller.clone(), mock: None, feed: controller.clone(), switch: controller }
        }
        ControllerKind::Mock => {
            let mock = Arc::new(MockController::new(config.chip_tool.endpoint));
            config.all_nodes().into_iter().for_each(|n| mock.add_node(n, 18.0, 18.0));
            config.devices.iter().for_each(|d| mock.add_device(d.node, ""));
            Built { controller: mock.clone(), feed: mock.clone(), switch: mock.clone(), mock: Some(mock) }
        }
    }
}

async fn serve(config: Config) -> Result<()> {
    let _lock = lock_storage(&config)?;
    let Built { controller, mock, feed, switch } = build_controller(&config, true);
    if let Some(mock) = mock {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                mock.tick();
            }
        });
    }
    let model = if Path::new(&config.model_path).exists() {
        Some(Arc::new(infer::RoomModel::load(Path::new(&config.model_path))?))
    } else {
        None
    };
    let sink = lake::Sink::open(lake::LakeConfig::new(&config.data_dir))?;
    sink.append(lake::Event::now(lake::kind::STARTUP, "heatd").with_detail(serde_json::json!({"commit": lake::COMMIT, "schema_version": lake::SCHEMA_VERSION})))?;
    let mut windows = window::Windows::default();
    if let Some(bundle) = window_model::try_load(&config.window.model_dir) {
        windows.set_model(bundle);
    }
    let replayed = windows.rebuild(&config, Path::new(&config.data_dir), lake::now_us() / 1000);
    println!("window history: {replayed} readings");
    lake::spawn_background(sink.clone());
    let status = status::Status::new(lake::now_us());
    status.set_stale_after(config.stale.after_s);
    status.register_nodes(config.all_nodes());
    let boost = boost::Boost::new(&config);
    let devices = devices::Devices::new(&config, feed, switch);
    let config = Arc::new(config);
    context::spawn_background(sink.clone(), status.clone(), config.clone(), context::Io::live(&config));
    let watchmatch = watchmatch::WatchMatch::new(&config.watchmatch, sink.clone(), status.clone());
    watchmatch.spawn_background();
    let windows = Arc::new(std::sync::Mutex::new(windows));
    window_model::spawn_reload(windows.clone(), config.window.model_dir.clone(), Duration::from_secs_f64(config.window.reload_min.max(0.1) * 60.0));
    let state = http::AppState {
        watchmatch,
        boost: boost.clone(),
        failsafe: failsafe::Failsafe::new(&config.failsafe, Some(Path::new(&config.data_dir).join("failsafe"))),
        devices,
        sink: sink.clone(),
        latest: Default::default(),
        status,
        storage: Arc::new(storage::Storage::open(&config.db_path)?),
        model,
        controller,
        windows,
        commanded: Default::default(),
        explorer: Arc::new(std::sync::Mutex::new(explore::Explorer::new(config.exploration_seed.unwrap_or_else(|| lake::now_us() as u64)))),
        tracker: Default::default(),
        batch_seq: Arc::new(std::sync::atomic::AtomicU64::new(lake::now_us() as u64)),
        config,
    };
    boost::spawn_background(boost);
    failsafe::spawn_background(state.clone());
    match restore::restore(&state, lake::now_us() / 1000) {
        Ok(summary) => println!("presence restore: {summary}"),
        Err(e) => eprintln!("presence restore: {e:#}"),
    }
    match restore::restore_windows(&state, lake::now_us() / 1000) {
        Ok(n) => println!("window restore: {n} events"),
        Err(e) => eprintln!("window restore: {e:#}"),
    }
    match restore::restore_holds(&state, lake::now_us() / 1000) {
        Ok(n) => println!("manual hold restore: {n} holds"),
        Err(e) => eprintln!("manual hold restore: {e:#}"),
    }
    match experiment::restore(&state, lake::now_us() / 1000) {
        Ok(s) => println!("messfahrt restore: {s}"),
        Err(e) => eprintln!("messfahrt restore: {e:#}"),
    }
    rules::spawn_background(state.clone());
    if state.config.br_watchdog.enabled {
        br_watchdog::spawn(state.clone());
    }
    node_health::spawn(state.clone());
    capture::spawn(state.clone(), capture::Timing::default());
    devices::spawn(state.clone(), devices::LoopTiming::default());
    let listener = tokio::net::TcpListener::bind(&state.config.listen).await.with_context(|| format!("bind {}", state.config.listen))?;
    println!("heatd listening on {}", state.config.listen);
    let stopper = state.clone();
    axum::serve(listener, http::router(state)).with_graceful_shutdown(async move {
        shutdown_signal().await;
        failsafe::shutdown(&stopper).await;
    }).await?;
    sink.append(lake::Event::now(lake::kind::SHUTDOWN, "heatd"))?;
    sink.close()?;
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() {
    let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn controller_command(config: Config, args: &[String]) -> Result<()> {
    let _lock = lock_storage(&config)?;
    let controller = build_controller(&config, false).controller;
    match args {
        [cmd, node, code] if cmd == "commission" => {
            let dataset = std::fs::read_to_string(&config.dataset_path).with_context(|| format!("read {}", config.dataset_path))?;
            controller.commission(node.parse()?, code, dataset.trim()).await?;
            println!("commissioned node {node}");
        }
        [cmd, node] if cmd == "read" => {
            let state = controller.read_thermostat(node.parse()?).await?;
            println!("{state:#?}");
        }
        [cmd, node] if cmd == "inventory" => {
            if config.controller == ControllerKind::Mock {
                bail!("inventory needs controller = \"chip-tool\", the config says mock");
            }
            let (path, inventory) = inventory::capture(controller.as_ref(), Path::new(&config.inventory_dir), node.parse()?).await?;
            println!("{} attributes, {} events, {} warnings", inventory.attributes.len(), inventory.events.len(), inventory.errors.len());
            println!("{}", path.display());
        }
        [cmd, node, celsius] if cmd == "set" => {
            controller.write_heating_setpoint(node.parse()?, celsius.parse()?).await?;
            println!("setpoint {celsius} written to node {node}");
        }
        _ => bail!(USAGE),
    }
    Ok(())
}

fn flag<'a>(args: &[&'a str], name: &str) -> Option<&'a str> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1])
}

fn export_command(config: &Config, args: &[&str]) -> Result<()> {
    let parse = |name: &str| -> Result<Option<chrono::NaiveDate>> {
        flag(args, name).map(|text| chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| anyhow!("bad {name} {text}, expected YYYY-MM-DD"))).transpose()
    };
    let (root, out, room) = (Path::new(&config.data_dir), flag(args, "--out").map(Into::into), flag(args, "--room"));
    let (path, rows) = match flag(args, "--view") {
        Some(view) if view != "room_minute" => {
            let to = parse("--to")?.unwrap_or_else(|| chrono::Utc::now().date_naive() - chrono::Duration::days(1));
            dynamics::export_view(root, view, room, parse("--from")?.unwrap_or(to - chrono::Duration::days(dynamics::SUMMARY_DAYS - 1)), to, out)?
        }
        _ => {
            let required = |name: &str| parse(name)?.ok_or_else(|| anyhow!(USAGE));
            report::export(root, room.ok_or_else(|| anyhow!(USAGE))?, required("--from")?, required("--to")?, out)?
        }
    };
    println!("{rows} rows written to {}", path.display());
    Ok(())
}

fn run(args: Vec<String>) -> Result<()> {
    let config = Config::load(Path::new(&std::env::var("HEATD_CONFIG").unwrap_or_else(|_| "heatd.toml".into())))?;
    let runtime = || tokio::runtime::Builder::new_multi_thread().enable_all().build();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["serve"] => runtime()?.block_on(serve(config)),
        ["thread", "form", port] => {
            let dataset = open_cli(port, &config)?.form_network(OT_LEADER_TIMEOUT, Duration::from_secs(2))?;
            save_dataset(&config, &dataset)
        }
        ["thread", "dataset", port] => {
            let dataset = open_cli(port, &config)?.active_dataset()?;
            save_dataset(&config, &dataset)
        }
        ["migrate"] => migrate::run(&config),
        ["export", rest @ ..] => export_command(&config, rest),
        ["report"] => report::report_command(Path::new(&config.data_dir), None).map(|p| println!("{}", p.display())),
        ["report", date] => report::report_command(Path::new(&config.data_dir), Some(date)).map(|p| println!("{}", p.display())),
        ["commission" | "read" | "set" | "inventory", ..] => runtime()?.block_on(controller_command(config, &args)),
        _ => bail!(USAGE),
    }
}

fn main() {
    if let Err(e) = run(std::env::args().skip(1).collect()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct FakePort {
        input: Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for FakePort {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn cli(console: &str) -> OtCli<FakePort> {
        OtCli::new(FakePort { input: Cursor::new(console.as_bytes().to_vec()), written: Vec::new() }, "")
    }

    #[test]
    fn classifies_lines() {
        let cases = [
            ("I (12345) OPENTHREAD: radio tx", ConsoleLine::Skip),
            ("\x1b[0;32mI (12345) wifi: connected\x1b[0m", ConsoleLine::Skip),
            ("W (99) esp_netif: something", ConsoleLine::Skip),
            ("I(44143) OPENTHREAD:[N] Mle-----------: Role detached -> leader", ConsoleLine::Skip),
            ("E(1396) OPENTHREAD:[C] P-SpinelDrive-: Software reset co-processor successfully", ConsoleLine::Skip),
            ("esp32s3> I (11423) example_connect: Got IPv6 event", ConsoleLine::Skip),
            ("Error 35: InvalidCommand", ConsoleLine::Error("Error 35: InvalidCommand".into())),
            ("esp32s3>", ConsoleLine::Skip),
            ("esp32s3> ", ConsoleLine::Skip),
            ("> > Done", ConsoleLine::Done),
            ("", ConsoleLine::Skip),
            ("> ", ConsoleLine::Skip),
            ("state", ConsoleLine::Skip),
            ("esp32s3> state", ConsoleLine::Skip),
            ("Done", ConsoleLine::Done),
            ("> Done", ConsoleLine::Done),
            ("Error 7: InvalidState", ConsoleLine::Error("Error 7: InvalidState".into())),
            ("leader", ConsoleLine::Data("leader".into())),
            ("0e080000000000010000", ConsoleLine::Data("0e080000000000010000".into())),
        ];
        for (raw, expected) in cases {
            assert_eq!(classify_line(raw, "state"), expected, "{raw}");
        }
    }

    #[test]
    fn exec_collects_data_and_skips_logs() {
        let mut cli = cli("state\r\nI (100) OT: noise\r\nleader\r\nW (101) OT: more\r\nDone\r\n");
        assert_eq!(cli.exec("state", Duration::from_secs(1)).unwrap(), vec!["leader"]);
        assert_eq!(cli.port.written, b"state\r\n");
    }

    #[test]
    fn reads_dataset_from_real_console_output() {
        let console = "dataset active -x\r\nesp32s3>\r\nI(44143) OPENTHREAD:[N] Mle-----------: Role detached -> leader\r\nesp32s3> I (5) wifi: x\r\n0e08000000000001\r\nDone\r\n";
        assert_eq!(cli(console).active_dataset().unwrap(), "0e08000000000001");
    }

    #[test]
    fn exec_reports_errors_and_timeouts() {
        let err = cli("dataset commit active\r\nError 13: InvalidArgs\r\n").exec("dataset commit active", Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("InvalidArgs"));
        assert!(cli("I (1) x: only logs\r\n").exec("state", Duration::from_secs(1)).is_err());
    }

    #[test]
    fn forms_network_and_reads_dataset() {
        let console = "Done\r\nDone\r\nDone\r\nDone\r\ndetached\r\nDone\r\nleader\r\nDone\r\nI (5) OT: x\r\n0e0800000000\r\nDone\r\n";
        let mut cli = cli(console);
        let dataset = cli.form_network(Duration::from_secs(5), Duration::from_millis(1)).unwrap();
        assert_eq!(dataset, "0e0800000000");
        let written = String::from_utf8(cli.port.written.clone()).unwrap();
        assert_eq!(written, "dataset init new\r\ndataset commit active\r\nifconfig up\r\nthread start\r\nstate\r\nstate\r\ndataset active -x\r\n");
    }
}
