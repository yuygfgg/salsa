use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use salsa_audiod::{
    CarrierSource, Codec, FileConfig, OutputConfig, RenderMode, ServiceConfig,
    choose_output_device_with_default, compare, list_output_devices, render, resolve_selector,
};

fn prompt(label: &str, default: &str) -> Result<String> {
    let mut stderr = std::io::stderr();
    write!(stderr, "{label} [{default}]: ")?;
    stderr.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    })
}

static STOP: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_signal(_signal: libc::c_int) {
    if let Some(stop) = STOP.get() {
        stop.store(true, Ordering::Relaxed);
    }
}

fn install_signal_handler(stop: &Arc<AtomicBool>) {
    let _ = STOP.set(stop.clone());
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CodecArg {
    Eac3,
    Truehd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RenderArg {
    Speaker,
    Binaural,
}

/// One of the two independently installable virtual-device services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AgentArg {
    Eac3,
    Truehd,
}

impl AgentArg {
    fn name(self) -> &'static str {
        match self {
            Self::Eac3 => "eac3",
            Self::Truehd => "truehd",
        }
    }

    fn label(self) -> &'static str {
        #[cfg(target_os = "macos")]
        {
            match self {
                Self::Eac3 => "dev.salsa.audio.audiod.eac3",
                Self::Truehd => "dev.salsa.audio.audiod.truehd",
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            match self {
                Self::Eac3 => "salsa-audiod-eac3.service",
                Self::Truehd => "salsa-audiod-truehd.service",
            }
        }
    }
}

impl From<CodecArg> for Codec {
    fn from(value: CodecArg) -> Self {
        match value {
            CodecArg::Eac3 => Self::Eac3,
            CodecArg::Truehd => Self::TrueHd,
        }
    }
}

#[derive(Debug, Clone, Args)]
struct RenderArgs {
    /// Speaker layout used by `salsaoar`.
    #[arg(long, default_value = "7.1.4")]
    layout: String,
    /// Rosella `.personalized_headphone` model; enables binaural rendering.
    #[arg(long, env = "SALSA_ROSELLA_MODEL")]
    binaural_model: Option<PathBuf>,
    /// HRTF filterbank: 1 = public, 2 = captured.
    #[arg(long, default_value_t = 1)]
    filterbank: u32,
    /// Rosella profile: near, mid or far.
    #[arg(long, default_value = "mid")]
    profile: String,
    /// Fixed scheduling allowance added to the presentation delay.
    #[arg(long, default_value_t = 50.0)]
    scheduling_allowance_ms: f64,
    /// Measured physical output latency (for example a CoreAudio or ALSA device).
    #[arg(long, default_value_t = 0.0)]
    physical_latency_ms: f64,
    /// Offline pipeline stall timeout.
    #[arg(long, default_value_t = 60.0)]
    idle_timeout_s: f64,
    /// Live output stall deadline before a deadline failure is counted.
    #[arg(long, default_value_t = 2000.0)]
    deadline_timeout_ms: f64,
    /// Live output stall warning threshold; the output sink recovers instead
    /// of stopping the session.
    #[arg(long, default_value_t = 10000.0)]
    hard_stall_timeout_ms: f64,
}

impl RenderArgs {
    fn mode(&self) -> RenderMode {
        match &self.binaural_model {
            Some(model) => RenderMode::Binaural {
                model: model.clone(),
                filterbank: self.filterbank,
                profile: self.profile.clone(),
            },
            None => RenderMode::speaker(self.layout.clone()),
        }
    }

    fn apply(&self, config: &mut ServiceConfig) {
        config.render = self.mode();
        config.scheduling_allowance_ns =
            (self.scheduling_allowance_ms.max(0.0) * 1_000_000.0) as u64;
        config.physical_output_latency_ns =
            (self.physical_latency_ms.max(0.0) * 1_000_000.0) as u64;
        config.idle_timeout = Duration::from_secs_f64(self.idle_timeout_s.max(0.1));
        config.deadline_timeout =
            Duration::from_secs_f64((self.deadline_timeout_ms.max(1.0)) / 1000.0);
        config.hard_stall_timeout =
            Duration::from_secs_f64((self.hard_stall_timeout_ms.max(1.0)) / 1000.0);
    }
}

#[derive(Debug, Clone, Default, Args)]
struct ConfigOverrides {
    /// Carrier shared-memory name.
    #[arg(long)]
    shm_name: Option<String>,
    /// Physical output: list number, device name, unique ID, or `default`.
    #[arg(long)]
    device: Option<String>,
    /// Require one codec instead of auto-detecting it.
    #[arg(long, value_enum)]
    codec: Option<CodecArg>,
    #[arg(long)]
    sample_rate: Option<u32>,
    /// Rendering mode: speaker or binaural.
    #[arg(long, value_enum)]
    render: Option<RenderArg>,
    /// Use binaural rendering (same as `--render binaural`).
    #[arg(long)]
    binaural: bool,
    /// Speaker layout used by `salsaoar` (switches to speaker rendering).
    #[arg(long)]
    layout: Option<String>,
    /// Rosella `.personalized_headphone` model (switches to binaural rendering).
    #[arg(long)]
    binaural_model: Option<PathBuf>,
    #[arg(long)]
    filterbank: Option<u32>,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    scheduling_allowance_ms: Option<f64>,
    #[arg(long)]
    physical_latency_ms: Option<f64>,
    #[arg(long)]
    deadline_timeout_ms: Option<f64>,
    #[arg(long)]
    hard_stall_timeout_ms: Option<f64>,
    /// Publish the fixed-delay contract to the carrier producer.
    #[arg(long)]
    declare_latency: bool,
    /// Do not publish the fixed-delay contract to the carrier producer.
    #[arg(long)]
    no_declare_latency: bool,
}

impl ConfigOverrides {
    fn apply(&self, config: &mut FileConfig) -> Result<()> {
        if self.declare_latency && self.no_declare_latency {
            bail!("--declare-latency and --no-declare-latency are mutually exclusive");
        }
        if let Some(value) = &self.shm_name {
            config.shm_name = value.clone();
        }
        if let Some(value) = &self.device {
            config.device = Some(value.clone());
        }
        if let Some(value) = self.codec {
            config.codec = Some(
                match value {
                    CodecArg::Eac3 => "eac3",
                    CodecArg::Truehd => "truehd",
                }
                .to_string(),
            );
        }
        if let Some(value) = self.sample_rate {
            config.sample_rate = value;
        }
        if self.binaural && self.render == Some(RenderArg::Speaker) {
            bail!("--binaural and --render speaker are mutually exclusive");
        }
        if let Some(value) = &self.layout {
            config.layout = value.clone();
            config.render = Some("speaker".to_string());
            config.binaural_model = None;
        }
        if let Some(value) = self.render {
            config.render = Some(
                match value {
                    RenderArg::Speaker => "speaker",
                    RenderArg::Binaural => "binaural",
                }
                .to_string(),
            );
        }
        if self.binaural {
            config.render = Some("binaural".to_string());
        }
        if let Some(value) = &self.binaural_model {
            config.binaural_model = Some(value.clone());
            config.render = Some("binaural".to_string());
        }
        if let Some(value) = self.filterbank {
            config.filterbank = value;
        }
        if let Some(value) = &self.profile {
            config.profile = value.clone();
        }
        if let Some(value) = self.scheduling_allowance_ms {
            config.scheduling_allowance_ms = value;
        }
        if let Some(value) = self.physical_latency_ms {
            config.physical_latency_ms = value;
        }
        if let Some(value) = self.deadline_timeout_ms {
            config.deadline_timeout_ms = value;
        }
        if let Some(value) = self.hard_stall_timeout_ms {
            config.hard_stall_timeout_ms = value;
        }
        if self.declare_latency {
            config.declare_latency = true;
        }
        if self.no_declare_latency {
            config.declare_latency = false;
        }
        Ok(())
    }

    fn render_requested(&self) -> bool {
        self.render.is_some()
            || self.binaural
            || self.layout.is_some()
            || self.binaural_model.is_some()
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "salsa-audiod",
    version,
    about = "Salsa virtual IEC 61937 audio service"
)]
struct Cli {
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Render an offline IEC 61937 capture through the Salsa path.
    Render {
        /// Captured carrier bytes (`.spdif`).
        #[arg(
            long,
            conflicts_with = "carrier_shm",
            required_unless_present = "carrier_shm"
        )]
        carrier: Option<PathBuf>,
        /// Render live carrier bytes from a shared-memory ring.
        #[arg(long, value_name = "NAME")]
        carrier_shm: Option<String>,
        /// Require a codec instead of auto-detecting it.
        #[arg(long, value_enum)]
        codec: Option<CodecArg>,
        #[arg(long, default_value_t = 48_000)]
        sample_rate: u32,
        /// Write an IEEE-float WAV. Omit for a null output.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Write the JSON render report.
        #[arg(long)]
        json: Option<PathBuf>,
        #[command(flatten)]
        render: RenderArgs,
    },
    /// Run the live service from the carrier shared-memory ring.
    Run {
        /// Configuration file for one codec agent.
        #[arg(long, env = "SALSA_AUDIOD_CONFIG")]
        config: PathBuf,
        /// Write the JSON render report at exit.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Offline-style stall timeout for a capture-backed run.
        #[arg(long)]
        idle_timeout_s: Option<f64>,
        #[command(flatten)]
        overrides: ConfigOverrides,
    },
    /// Choose the physical output and render settings, then save them.
    Configure {
        /// Configuration file to write.
        #[arg(long, env = "SALSA_AUDIOD_CONFIG")]
        config: Option<PathBuf>,
        /// Per-codec service instance. Selects `audiod-<agent>.json` and the
        /// matching service.
        #[arg(long, value_enum)]
        agent: AgentArg,
        /// Print the saved configuration instead of changing it.
        #[arg(long)]
        show: bool,
        /// Restart the installed service after saving.
        #[arg(long)]
        restart: bool,
        #[command(flatten)]
        overrides: ConfigOverrides,
    },
    /// List or choose a physical output device.
    Devices {
        /// Include Salsa's virtual carrier device in the list.
        #[arg(long)]
        all: bool,
        /// Prompt for a device and print its unique ID.
        #[arg(long)]
        choose: bool,
        /// Resolve a list number, name, or unique ID to the value for --device.
        #[arg(long, value_name = "DEVICE")]
        resolve: Option<String>,
        /// Print the list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Compare carrier rendering with direct GStreamer rendering.
    Compare {
        #[arg(long)]
        carrier: PathBuf,
        /// Raw codec file used by the direct baseline (`.ec3` or `.mlp`).
        #[arg(long)]
        direct: PathBuf,
        #[arg(long, value_enum)]
        codec: Option<CodecArg>,
        #[arg(long, default_value_t = 48_000)]
        sample_rate: u32,
        /// Maximum allowed sample difference.
        #[arg(long, default_value_t = 1e-5)]
        tolerance: f32,
        /// Write the JSON reports.
        #[arg(long)]
        json: Option<PathBuf>,
        #[command(flatten)]
        render: RenderArgs,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.verbose {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    } else {
        let _ = env_logger::try_init();
    }

    match cli.command {
        Command::Devices {
            all,
            choose,
            resolve,
            json,
        } => {
            if let Some(query) = resolve {
                println!("{}", resolve_selector(&query)?);
                return Ok(());
            }
            if choose {
                println!("{}", choose_output_device_with_default(None)?);
                return Ok(());
            }
            let devices = list_output_devices(all)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&devices)?);
            } else if devices.is_empty() {
                println!("no output devices found");
            } else {
                for device in devices {
                    let selector = device.selector().unwrap_or("(no unique id)");
                    println!(
                        "{:>2}. {}\n    {selector}",
                        device.index,
                        device.description()
                    );
                }
            }
        }
        Command::Render {
            carrier,
            carrier_shm,
            codec,
            sample_rate,
            output,
            json,
            render: render_args,
        } => {
            let source = match (carrier, carrier_shm) {
                (Some(path), None) => CarrierSource::File(path),
                (None, Some(name)) => CarrierSource::Shared(name),
                _ => bail!("exactly one of --carrier or --carrier-shm is required"),
            };
            let mut config = ServiceConfig::new(source);
            config.codec = codec.map(Into::into);
            config.sample_rate = sample_rate;
            config.output = match output {
                Some(path) => OutputConfig::Wav(path),
                None => OutputConfig::Null,
            };
            render_args.apply(&mut config);
            let stop = Arc::new(AtomicBool::new(false));
            install_signal_handler(&stop);
            config.stop = stop;
            let outcome = render(config)?;
            print_report(&outcome.report, json.as_ref())?;
        }
        Command::Configure {
            config,
            agent,
            show,
            restart,
            overrides,
        } => {
            let path = match config {
                Some(path) => path,
                None => FileConfig::profile_path(agent.name())?,
            };
            let mut file = if path.exists() {
                FileConfig::load(&path)?
            } else {
                FileConfig::profile_default(agent.name())
            };
            overrides.apply(&mut file)?;
            if show {
                println!("{}", serde_json::to_string_pretty(&file)?);
                return Ok(());
            }
            if std::io::stdin().is_terminal() && overrides.device.is_none() {
                file.device = Some(choose_output_device_with_default(file.device.as_deref())?);
            }
            if file.device.is_none() {
                if std::io::stdin().is_terminal() {
                    file.device = Some(choose_output_device_with_default(None)?);
                } else {
                    bail!(
                        "no output device configured; run `salsa-audiod devices` and pass --device"
                    );
                }
            }
            if file.device.is_none() {
                file.device = Some("system".to_string());
            }
            #[cfg(target_os = "macos")]
            if let Some(device) = &file.device
                && !device.is_empty()
                && device != "system"
            {
                file.device = Some(resolve_selector(device)?);
            }
            #[cfg(target_os = "linux")]
            if let Some(device) = &file.device
                && !device.is_empty()
                && device != "system"
                && !device.contains(':')
            {
                file.device = Some(resolve_selector(device)?);
            }
            if let Some(model) = &file.binaural_model
                && !model.exists()
            {
                bail!("binaural model does not exist: {}", model.display());
            }
            if std::io::stdin().is_terminal() {
                if !overrides.render_requested() {
                    let current = if file.binaural_model.is_some() {
                        "binaural"
                    } else {
                        "speaker"
                    };
                    let mode = prompt("Render mode [speaker/binaural]", current)?;
                    match mode.to_lowercase().as_str() {
                        "speaker" => {
                            file.render = Some("speaker".to_string());
                            file.binaural_model = None;
                        }
                        "binaural" => file.render = Some("binaural".to_string()),
                        other => bail!("unknown render mode '{other}'"),
                    }
                }
                let binaural = file.render.as_deref() == Some("binaural")
                    || (file.render.is_none() && file.binaural_model.is_some());
                if binaural {
                    let current = file
                        .binaural_model
                        .as_ref()
                        .map(|path| path.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let model = prompt("Binaural model path", &current)?;
                    if model.is_empty() {
                        bail!("binaural rendering requires a model path");
                    }
                    let model = PathBuf::from(model);
                    if !model.exists() {
                        bail!("binaural model does not exist: {}", model.display());
                    }
                    file.binaural_model = Some(model);
                    file.render = Some("binaural".to_string());
                    let filterbank = prompt(
                        "Filterbank (1=public, 2=captured)",
                        &file.filterbank.to_string(),
                    )?;
                    file.filterbank = filterbank.parse().context("filterbank must be 1 or 2")?;
                    file.profile = prompt("Profile (near/mid/far)", &file.profile)?;
                } else if overrides.layout.is_none() {
                    file.layout = prompt("Speaker layout", &file.layout)?;
                }
            } else {
                file.render_mode()?;
            }
            file.save(&path)?;
            println!("saved {}", path.display());
            if let Some(device) = &file.device {
                println!("output: {device}");
            }
            #[cfg(target_os = "macos")]
            {
                let uid = unsafe { libc::getuid() };
                let label_name = agent.label();
                let label = format!("gui/{uid}/{label_name}");
                let plist = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(format!("Library/LaunchAgents/{label_name}.plist"));
                let loaded = std::process::Command::new("launchctl")
                    .args(["print", &label])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if restart {
                    if !plist.exists() {
                        println!("service is not installed; run the install script first");
                        return Ok(());
                    }
                    // Reload the plist so KeepAlive/RunAtLoad and argument
                    // changes take effect, then start it.
                    let _ = std::process::Command::new("launchctl")
                        .args(["bootout", &label])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    std::thread::sleep(Duration::from_millis(300));
                    let bootstrap = || {
                        std::process::Command::new("launchctl")
                            .args(["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])
                            .status()
                            .map(|status| status.success())
                            .unwrap_or(false)
                    };
                    if !bootstrap() {
                        std::thread::sleep(Duration::from_millis(500));
                        let _ = bootstrap();
                    }
                    std::thread::sleep(Duration::from_millis(300));
                    let kickstart = || {
                        std::process::Command::new("launchctl")
                            .args(["kickstart", "-k", &label])
                            .status()
                            .map(|status| status.success())
                            .unwrap_or(false)
                    };
                    let started = kickstart() || {
                        std::thread::sleep(Duration::from_millis(500));
                        kickstart()
                    };
                    if started {
                        println!("service started");
                    } else {
                        println!("could not start the service; check: launchctl print {label}");
                    }
                } else if loaded {
                    println!("restart the service to apply the new settings:");
                    println!("  launchctl kickstart -k {label}");
                } else if plist.exists() {
                    println!("start the service to apply the new settings:");
                    println!("  launchctl bootstrap gui/{uid} \"{}\"", plist.display());
                } else {
                    println!("service is not installed; run the install script first");
                }
            }
            #[cfg(target_os = "linux")]
            {
                let unit = agent.label();
                let loaded = std::process::Command::new("systemctl")
                    .args(["--user", "is-active", "--quiet", unit])
                    .status()
                    .map(|status| status.success())
                    .unwrap_or(false);
                if restart {
                    let status = std::process::Command::new("systemctl")
                        .args(["--user", "restart", unit])
                        .status()
                        .map(|status| status.success())
                        .unwrap_or(false);
                    if status {
                        println!("service restarted");
                    } else {
                        println!("could not restart {unit}; run the Linux install script first");
                    }
                } else if loaded {
                    println!("restart the service to apply the new settings:");
                    println!("  systemctl --user restart {unit}");
                } else {
                    println!("start the service to apply the new settings:");
                    println!("  systemctl --user enable --now {unit}");
                }
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = restart;
                println!("service restart is only supported on macOS and Linux");
            }
        }
        Command::Run {
            config,
            json,
            idle_timeout_s,
            overrides,
        } => {
            let path = config;
            let mut file = FileConfig::load(&path)?;
            overrides.apply(&mut file)?;
            let device = match file.device.clone() {
                None => None,
                Some(device) if device.is_empty() || device == "system" => None,
                Some(device) => {
                    #[cfg(target_os = "macos")]
                    {
                        Some(resolve_selector(&device)?)
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        Some(device)
                    }
                }
            };
            let output_label = device.as_deref().unwrap_or("system default");
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if device.is_none()
                && list_output_devices(true).is_ok_and(|devices| {
                    devices
                        .iter()
                        .find(|candidate| candidate.is_default)
                        .is_some_and(|candidate| candidate.is_salsa_virtual())
                })
            {
                bail!(
                    "the system output is the Salsa virtual device; choose a physical output with `salsa-audiod configure`"
                );
            }
            let stop = Arc::new(AtomicBool::new(false));
            install_signal_handler(&stop);
            let mut config = ServiceConfig::new(CarrierSource::Shared(file.shm_name.clone()));
            file.apply(&mut config)?;
            config.output = OutputConfig::Physical {
                device: device.clone(),
            };
            config.stop = stop;
            if let Some(seconds) = idle_timeout_s {
                config.idle_timeout = Duration::from_secs_f64(seconds.max(0.1));
            }
            eprintln!(
                "salsa-audiod: starting output={output_label} render={}",
                config.render.label()
            );
            let outcome = render(config)?;
            print_report(&outcome.report, json.as_ref())?;
        }
        Command::Compare {
            carrier,
            direct,
            codec,
            sample_rate,
            tolerance,
            json,
            render: render_args,
        } => {
            let mut config = ServiceConfig::new(CarrierSource::File(carrier));
            config.codec = codec.map(Into::into);
            config.sample_rate = sample_rate;
            render_args.apply(&mut config);
            let (report, comparison) = compare(config, direct, tolerance)?;
            print_report(&report, None)?;
            println!(
                "compare: codec={} render={} carrier={} frames direct={} frames compared={} \
                 max_abs_diff={:.9} rms_diff={:.9} tolerance={:.9} {}",
                comparison.codec,
                comparison.render,
                comparison.carrier_frames,
                comparison.direct_frames,
                comparison.compared_frames,
                comparison.max_abs_diff,
                comparison.rms_diff,
                comparison.tolerance,
                if comparison.passed { "PASS" } else { "FAIL" }
            );
            if let Some(path) = json {
                let document = serde_json::json!({
                    "render": report,
                    "comparison": comparison,
                });
                std::fs::write(&path, serde_json::to_vec_pretty(&document)?)
                    .with_context(|| format!("cannot write {}", path.display()))?;
            }
            if !comparison.passed {
                bail!("carrier rendering does not match direct GStreamer rendering");
            }
        }
    }
    Ok(())
}

fn print_report(report: &salsa_audiod::RenderReport, json: Option<&PathBuf>) -> Result<()> {
    let document = serde_json::to_vec_pretty(report)?;
    match json {
        Some(path) => {
            std::fs::write(path, &document)
                .with_context(|| format!("cannot write {}", path.display()))?;
        }
        None => println!("{}", String::from_utf8_lossy(&document)),
    }
    if !report.ledger.allowance_is_safe() {
        eprintln!(
            "warning: scheduling allowance {} ms is below the {} ms parser/renderer lookahead",
            report.ledger.scheduling_allowance_ns as f64 / 1e6,
            report.ledger.minimum_allowance_ns() as f64 / 1e6
        );
    }
    Ok(())
}
