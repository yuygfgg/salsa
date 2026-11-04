//! Salsa Studio application state machine.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Align, Color32, Layout, RichText};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::model::{Phase, SourceInfo, UiError};
use crate::pipeline::{
    Codec, DeviceInfo, DeviceSelection, OutputConfig, OutputMode, enumerate_devices,
    resolve_layout, scan_duration,
};
use crate::player::{Player, PlayerError};
use crate::settings::Settings;
use crate::ui::{
    output::{self, OutputAction},
    source::{self, EmptyAction},
    stage::{self, StageAction, StageState},
    theme,
    transport::{self, TransportAction, TransportState},
};
use crate::visual::VisualBridge;

struct Opened {
    player: Player,
    source: SourceInfo,
    duration: Option<gst::ClockTime>,
    config: OutputConfig,
}

enum SwitchPhase {
    FadeOut,
    FadeIn,
}

struct SwitchState {
    phase: SwitchPhase,
    started: Instant,
    target: gst::ClockTime,
    was_playing: bool,
    config: OutputConfig,
    previous: OutputConfig,
}

pub struct SalsaApp {
    settings: Settings,
    player: Option<Player>,
    active_config: Option<OutputConfig>,
    effective_layout: String,
    visual: VisualBridge,
    devices: Vec<DeviceInfo>,
    source: Option<SourceInfo>,
    phase: Phase,
    error: Option<UiError>,
    warning: Option<String>,
    open_rx: Option<Receiver<Result<Opened, PlayerError>>>,
    open_cancel: Arc<AtomicBool>,
    open_path: Option<PathBuf>,
    duration_rx: Option<Receiver<Option<gst::ClockTime>>>,
    duration_cancel: Arc<AtomicBool>,
    duration_scanning: bool,
    duration: Option<gst::ClockTime>,
    position: gst::ClockTime,
    seek_target: Option<gst::ClockTime>,
    resync_started: Option<Instant>,
    was_playing: bool,
    switch: Option<SwitchState>,
    stage: StageState,
    transport: TransportState,
    focus_mode: bool,
    show_help: bool,
    show_settings: bool,
    dirty: bool,
    last_save: Instant,
}

impl SalsaApp {
    pub fn new(cc: &eframe::CreationContext<'_>, mut settings: Settings) -> Self {
        theme::configure(&cc.egui_ctx);
        if settings
            .binaural
            .model
            .as_ref()
            .is_some_and(|path| !path.exists())
        {
            settings.binaural.model = None;
        }
        if settings.binaural.model.is_none() {
            settings.binaural.model = find_bundled_model();
        }
        let devices = enumerate_devices();
        let effective_layout = if settings.mode == OutputMode::Speaker {
            resolve_layout(&settings.layout, &settings.device, &devices).0
        } else {
            settings.layout.clone()
        };
        let visual = VisualBridge::new(settings.visual_enabled);
        let transport = TransportState {
            volume: settings.volume,
            visual_enabled: settings.visual_enabled,
            ..Default::default()
        };
        Self {
            settings,
            player: None,
            active_config: None,
            effective_layout,
            visual,
            devices,
            source: None,
            phase: Phase::Empty,
            error: None,
            warning: None,
            open_rx: None,
            open_cancel: Arc::new(AtomicBool::new(false)),
            open_path: None,
            duration_rx: None,
            duration_cancel: Arc::new(AtomicBool::new(false)),
            duration_scanning: false,
            duration: None,
            position: gst::ClockTime::ZERO,
            seek_target: None,
            resync_started: None,
            was_playing: false,
            switch: None,
            stage: StageState::default(),
            transport,
            focus_mode: false,
            show_help: false,
            show_settings: false,
            dirty: false,
            last_save: Instant::now(),
        }
    }

    fn show_open_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Open an Atmos stream")
            .add_filter(
                "Atmos streams",
                &["ec3", "eac3", "mlp", "thd", "truehd", "atmos", "damf"],
            )
            .pick_file()
        {
            self.open_path(path);
        }
    }

    pub fn open_path(&mut self, path: PathBuf) {
        if self.open_rx.is_some() {
            self.open_cancel.store(true, Ordering::Relaxed);
        }
        if let Some(player) = self.player.take() {
            player.close();
        }
        self.visual.detach();
        self.visual.reset();
        self.source = None;
        self.active_config = None;
        self.duration_cancel.store(true, Ordering::Relaxed);
        self.duration = None;
        self.duration_rx = None;
        self.duration_scanning = false;
        self.position = gst::ClockTime::ZERO;
        self.seek_target = None;
        self.resync_started = None;
        self.was_playing = false;
        self.error = None;
        self.warning = None;
        self.switch = None;
        self.phase = Phase::Loading;
        self.open_path = Some(path.clone());
        self.refresh_devices();

        let (effective_layout, warning) = if self.settings.mode == OutputMode::Speaker {
            resolve_layout(&self.settings.layout, &self.settings.device, &self.devices)
        } else {
            (self.settings.layout.clone(), None)
        };
        self.effective_layout = effective_layout.clone();
        self.warning = warning;

        let mut config = self.settings.output_config();
        config.layout = effective_layout;
        let cancel = Arc::new(AtomicBool::new(false));
        self.open_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let devices = self.devices.clone();
        let path_for_worker = path.clone();
        let _ = std::thread::Builder::new()
            .name("salsa-open".into())
            .spawn(move || {
                let result = Player::open(
                    &path_for_worker,
                    &config,
                    &devices,
                    gst::ClockTime::ZERO,
                    false,
                    &cancel,
                    true,
                )
                .and_then(|player| {
                    let source = source_from_player(&player, &path_for_worker)?;
                    let duration = player
                        .duration()
                        .filter(|duration| *duration > gst::ClockTime::ZERO);
                    Ok(Opened {
                        player,
                        source,
                        duration,
                        config,
                    })
                });
                let _ = sender.send(result);
            });
        self.open_rx = Some(receiver);
    }

    fn poll_open(&mut self, ctx: &egui::Context) {
        let Some(receiver) = &self.open_rx else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(opened)) => {
                self.open_rx = None;
                self.accept_opened(opened);
            }
            Ok(Err(error)) => {
                self.open_rx = None;
                let retry = self.open_path.take();
                if matches!(&error, PlayerError::NoObjectAudio)
                    && let Some(path) = &retry
                {
                    self.settings.recent.retain(|entry| &entry.path != path);
                    self.dirty = true;
                }
                self.phase = Phase::Empty;
                self.error = Some(error_to_ui(error, retry));
            }
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Err(TryRecvError::Disconnected) => {
                self.open_rx = None;
                self.phase = Phase::Empty;
                self.error = Some(UiError::new("The loader thread stopped unexpectedly."));
            }
        }
    }

    fn accept_opened(&mut self, opened: Opened) {
        let Opened {
            player,
            source,
            duration,
            config,
        } = opened;
        if let Err(error) = self.visual.attach(player.decoder()) {
            self.warning = Some(error.to_string());
        }
        self.visual.reset();
        player.set_volume(f64::from(self.settings.volume));
        self.duration = duration;
        if self.duration.is_none() {
            self.spawn_duration_scan(&source.path, source.codec);
        }
        self.settings
            .note_recent(&source.path, source.codec.short(), source.objects);
        self.dirty = true;
        self.source = Some(source);
        self.active_config = Some(config);
        self.player = Some(player);
        self.phase = Phase::Paused;
        self.position = gst::ClockTime::ZERO;
        self.open_path = None;
        self.error = None;
        self.resync_started = None;
        self.seek_target = None;
        self.was_playing = false;
    }

    fn spawn_duration_scan(&mut self, path: &std::path::Path, codec: Codec) {
        self.duration_cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.duration_cancel = cancel.clone();
        let (sender, receiver) = mpsc::channel();
        let path = path.to_path_buf();
        let _ = std::thread::Builder::new()
            .name("salsa-duration".into())
            .spawn(move || {
                let _ = sender.send(scan_duration(&path, codec, &cancel));
            });
        self.duration_rx = Some(receiver);
        self.duration_scanning = true;
    }

    fn poll_duration(&mut self) {
        let Some(receiver) = &self.duration_rx else {
            return;
        };
        match receiver.try_recv() {
            Ok(duration) => {
                self.duration_rx = None;
                self.duration_scanning = false;
                if let Some(duration) = duration.filter(|duration| *duration > gst::ClockTime::ZERO)
                {
                    self.duration = Some(duration);
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.duration_rx = None;
                self.duration_scanning = false;
            }
        }
    }

    fn poll_player(&mut self, ctx: &egui::Context) {
        let mut messages = Vec::new();
        if let Some(player) = &self.player {
            while let Some(message) = player.bus().pop() {
                messages.push(message);
            }
        }
        for message in messages {
            match message.view() {
                gst::MessageView::Error(error) => {
                    let text = error.error().to_string();
                    let debug = error
                        .debug()
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    let message = if debug.is_empty() {
                        text
                    } else {
                        format!("{text} ({debug})")
                    };
                    let retry = self.source.as_ref().map(|source| source.path.clone());
                    if let Some(player) = self.player.take() {
                        player.close();
                    }
                    self.visual.detach();
                    self.phase = Phase::Error;
                    self.error = Some(match retry {
                        Some(path) => UiError::with_retry(message, path),
                        None => UiError::new(message),
                    });
                }
                gst::MessageView::Eos(_) => {
                    self.phase = Phase::Eos;
                    if let Some(duration) = self.duration {
                        self.position = duration;
                    }
                }
                _ => {}
            }
        }

        let Some(player) = &self.player else {
            return;
        };
        let position = player.position();
        if position > gst::ClockTime::ZERO || self.phase != Phase::Paused {
            self.position = position;
        }

        if !matches!(self.phase, Phase::Seeking | Phase::Eos | Phase::Error) {
            match (self.phase, player.state()) {
                (Phase::Paused, gst::State::Playing) => self.phase = Phase::Playing,
                (Phase::Playing, gst::State::Paused) => self.phase = Phase::Paused,
                _ => {}
            }
        }

        if self.phase == Phase::Seeking
            && self
                .resync_started
                .is_some_and(|started| started.elapsed() > Duration::from_millis(320))
        {
            self.phase = if self.was_playing {
                Phase::Playing
            } else {
                Phase::Paused
            };
            self.resync_started = None;
            if let Some(target) = self.seek_target.take()
                && self
                    .source
                    .as_ref()
                    .is_some_and(|source| source.codec == Codec::TRUEHD)
            {
                let difference = self.position.nseconds().abs_diff(target.nseconds());
                if difference > 150_000_000 {
                    self.warning = Some("Landed on the nearest TrueHD sync.".to_string());
                }
            }
        }

        if matches!(self.phase, Phase::Playing | Phase::Seeking) {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    fn poll_switch(&mut self) {
        let Some(switch) = self.switch.take() else {
            return;
        };
        let elapsed = switch.started.elapsed().as_secs_f32();
        match switch.phase {
            SwitchPhase::FadeOut => {
                let gain =
                    f64::from(self.settings.volume) * f64::from((1.0 - elapsed / 0.08).max(0.0));
                if let Some(player) = &self.player {
                    player.set_volume(gain);
                }
                if elapsed < 0.08 {
                    self.switch = Some(switch);
                    return;
                }

                let Some(path) = self.source.as_ref().map(|source| source.path.clone()) else {
                    self.switch = None;
                    return;
                };
                if let Some(player) = self.player.take() {
                    player.close();
                }
                self.visual.detach();
                self.visual.begin_segment();

                let cancel = Arc::new(AtomicBool::new(false));
                match Player::open(
                    &path,
                    &switch.config,
                    &self.devices,
                    switch.target,
                    switch.was_playing,
                    &cancel,
                    false,
                ) {
                    Ok(player) => {
                        player.set_volume(0.0);
                        if let Err(error) = self.visual.attach(player.decoder()) {
                            self.warning = Some(error.to_string());
                        }
                        self.visual.begin_segment();
                        self.effective_layout = switch.config.layout.clone();
                        self.active_config = Some(switch.config.clone());
                        self.player = Some(player);
                        self.position = switch.target;
                        self.phase = if switch.was_playing {
                            Phase::Playing
                        } else {
                            Phase::Paused
                        };
                        self.switch = Some(SwitchState {
                            phase: SwitchPhase::FadeIn,
                            started: Instant::now(),
                            target: switch.target,
                            was_playing: switch.was_playing,
                            config: switch.config,
                            previous: switch.previous,
                        });
                    }
                    Err(error) => {
                        let rollback = Player::open(
                            &path,
                            &switch.previous,
                            &self.devices,
                            switch.target,
                            switch.was_playing,
                            &cancel,
                            false,
                        );
                        match rollback {
                            Ok(player) => {
                                if let Err(attach_error) = self.visual.attach(player.decoder()) {
                                    self.warning = Some(attach_error.to_string());
                                }
                                self.visual.reset();
                                player.set_volume(f64::from(self.settings.volume));
                                self.effective_layout = switch.previous.layout.clone();
                                self.active_config = Some(switch.previous.clone());
                                self.player = Some(player);
                                self.phase = if switch.was_playing {
                                    Phase::Playing
                                } else {
                                    Phase::Paused
                                };
                                self.error = Some(UiError::new(format!(
                                    "Could not switch output: {error}. Kept the previous output."
                                )));
                            }
                            Err(rollback_error) => {
                                self.phase = Phase::Error;
                                self.error = Some(UiError::new(format!(
                                    "Could not switch output: {error}. Previous output could not be restored: {rollback_error}."
                                )));
                            }
                        }
                    }
                }
            }
            SwitchPhase::FadeIn => {
                let gain =
                    f64::from(self.settings.volume) * f64::from((elapsed / 0.12).clamp(0.0, 1.0));
                if let Some(player) = &self.player {
                    player.set_volume(gain);
                }
                if elapsed < 0.12 {
                    self.switch = Some(switch);
                } else if let Some(player) = &self.player {
                    player.set_volume(f64::from(self.settings.volume));
                }
            }
        }
    }

    fn refresh_devices(&mut self) {
        let devices = enumerate_devices();
        if devices.is_empty() {
            return;
        }
        self.devices = devices;
        if let DeviceSelection::Named(name) = &self.settings.device
            && !self.devices.iter().any(|info| &info.display_name == name)
        {
            self.settings.device = DeviceSelection::Auto;
            self.dirty = true;
        }
    }

    fn request_output_change(&mut self) {
        let (effective_layout, warning) = if self.settings.mode == OutputMode::Speaker {
            resolve_layout(&self.settings.layout, &self.settings.device, &self.devices)
        } else {
            (self.settings.layout.clone(), None)
        };
        self.effective_layout = effective_layout.clone();
        self.warning = warning;

        if self.settings.mode == OutputMode::Binaural && self.settings.binaural.model.is_none() {
            self.error = Some(UiError::missing_model());
            return;
        }
        let Some(player) = &self.player else {
            self.active_config = None;
            return;
        };
        let mut config = self.settings.output_config();
        config.layout = effective_layout;
        if self.active_config.as_ref() == Some(&config) {
            return;
        }
        if self.source.is_none() {
            return;
        }

        let target = if self.phase == Phase::Eos {
            gst::ClockTime::ZERO
        } else {
            player.position()
        };
        let was_playing = self.phase == Phase::Playing;
        let previous = self.active_config.clone().unwrap_or_else(|| config.clone());
        self.error = None;
        self.switch = Some(SwitchState {
            phase: SwitchPhase::FadeOut,
            started: Instant::now(),
            target,
            was_playing,
            config,
            previous,
        });
    }

    fn apply_output_action(&mut self, action: OutputAction) {
        match action {
            OutputAction::Mode(mode) => {
                let binaural = mode == OutputMode::Binaural;
                self.settings.mode = mode;
                self.dirty = true;
                if binaural && self.settings.binaural.model.is_none() {
                    self.error = Some(UiError::missing_model());
                } else {
                    self.error = None;
                    self.request_output_change();
                }
            }
            OutputAction::Layout(layout) => {
                self.settings.layout = layout;
                self.dirty = true;
                self.request_output_change();
            }
            OutputAction::Device(device) => {
                self.settings.device = device;
                self.refresh_devices();
                self.dirty = true;
                self.request_output_change();
            }
            OutputAction::RefreshDevices => {
                self.refresh_devices();
                self.request_output_change();
            }
            OutputAction::Model(path) => {
                self.settings.binaural.model = path;
                self.dirty = true;
                self.error = None;
                self.request_output_change();
            }
            OutputAction::Filterbank(value) => {
                self.settings.binaural.filterbank = value;
                self.dirty = true;
                self.request_output_change();
            }
            OutputAction::Profile(value) => {
                self.settings.binaural.profile = value;
                self.dirty = true;
                self.request_output_change();
            }
        }
    }

    fn pick_model(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Choose a Rosella personalized-headphone model")
            .add_filter("Rosella model", &["personalized_headphone"])
            .pick_file()
        {
            self.settings.binaural.model = Some(path);
            self.settings.mode = OutputMode::Binaural;
            self.dirty = true;
            self.error = None;
            self.request_output_change();
        }
    }

    fn set_visual(&mut self, enabled: bool) {
        self.settings.visual_enabled = enabled;
        self.visual.set_enabled(enabled);
        self.transport.visual_enabled = enabled;
        self.dirty = true;
    }

    fn set_volume(&mut self, volume: f32) {
        self.settings.volume = volume.clamp(0.0, 1.0);
        self.transport.volume = self.settings.volume;
        if self.switch.is_none()
            && let Some(player) = &self.player
        {
            player.set_volume(f64::from(self.settings.volume));
        }
        self.dirty = true;
    }

    fn toggle_play(&mut self) {
        match self.phase {
            Phase::Empty | Phase::Loading | Phase::Error => {}
            Phase::Playing => {
                if let Some(player) = &self.player {
                    if let Err(error) = player.set_paused() {
                        self.error = Some(UiError::new(error.to_string()));
                    } else {
                        self.phase = Phase::Paused;
                    }
                }
            }
            Phase::Eos => {
                self.seek_to(gst::ClockTime::ZERO);
                if let Some(player) = &self.player {
                    if let Err(error) = player.set_playing() {
                        self.error = Some(UiError::new(error.to_string()));
                    } else {
                        self.phase = Phase::Playing;
                    }
                }
            }
            _ => {
                if let Some(player) = &self.player {
                    if let Err(error) = player.set_playing() {
                        self.error = Some(UiError::new(error.to_string()));
                    } else {
                        self.phase = Phase::Playing;
                    }
                }
            }
        }
    }

    fn seek_to(&mut self, position: gst::ClockTime) {
        let Some(player) = &self.player else {
            return;
        };
        let target = self
            .duration
            .map_or(position, |duration| position.min(duration));
        if let Err(error) = player.seek(target) {
            self.error = Some(UiError::new(error.to_string()));
            return;
        }
        self.position = target;
        self.seek_target = Some(target);
        self.was_playing = self.phase == Phase::Playing;
        self.resync_started = Some(Instant::now());
        self.phase = Phase::Seeking;
        self.visual.begin_segment();
    }

    fn seek_relative(&mut self, seconds: f64) {
        let value = self.position.nseconds() as f64 + seconds * 1e9;
        self.seek_to(gst::ClockTime::from_nseconds(value.max(0.0) as u64));
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let input = ctx.input(|input| {
            (
                input.modifiers.command && input.key_pressed(egui::Key::O),
                input.key_pressed(egui::Key::Space),
                input.key_pressed(egui::Key::ArrowLeft),
                input.key_pressed(egui::Key::ArrowRight),
                input.key_pressed(egui::Key::Home),
                input.key_pressed(egui::Key::End),
                input.key_pressed(egui::Key::Num1),
                input.key_pressed(egui::Key::Num2),
                input.key_pressed(egui::Key::Num3),
                input.key_pressed(egui::Key::F),
                input.key_pressed(egui::Key::V),
                input.key_pressed(egui::Key::Escape),
                input.modifiers.shift,
            )
        });
        let (
            open,
            space,
            left,
            right,
            home,
            end,
            top,
            front,
            perspective,
            follow,
            visual,
            escape,
            shift,
        ) = input;
        if open {
            self.show_open_dialog();
        }
        if space {
            self.toggle_play();
        }
        if left {
            self.seek_relative(if shift { -10.0 } else { -5.0 });
        }
        if right {
            self.seek_relative(if shift { 10.0 } else { 5.0 });
        }
        if home {
            self.seek_to(gst::ClockTime::ZERO);
        }
        if end && let Some(duration) = self.duration {
            self.seek_to(duration);
        }
        if top {
            self.stage.top_view();
        }
        if front {
            self.stage.front_view();
        }
        if perspective {
            self.stage.perspective_view();
        }
        if follow {
            self.stage.follow_selected();
        }
        if visual {
            self.set_visual(!self.settings.visual_enabled);
        }
        if escape {
            self.focus_mode = false;
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        if let Some(path) = dropped.into_iter().find_map(|file| file.path) {
            self.open_path(path);
        }
    }

    fn plugins_ok(&self) -> bool {
        [
            "salsadamfsrc",
            "salsaeac3jocparse",
            "salsaeac3jocdec",
            "salsatruehdparse",
            "salsatruehdatmosdec",
            "salsaoar",
            "salsahrtfrender",
        ]
        .iter()
        .all(|name| gst::ElementFactory::find(name).is_some())
    }

    fn diagnostics(&self) -> String {
        let plugin = |name: &str| {
            if gst::ElementFactory::find(name).is_some() {
                "found"
            } else {
                "MISSING"
            }
        };
        let devices = if self.devices.is_empty() {
            "<none>".to_string()
        } else {
            self.devices
                .iter()
                .map(|info| {
                    format!(
                        "{} ({} ch){}",
                        info.display_name,
                        info.max_channels
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                        if info.is_default { " [default]" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "Salsa Studio 0.1.0\n\
             GStreamer: {}\n\
             GST_PLUGIN_PATH: {}\n\
             GST_REGISTRY_FORK: {}\n\
             salsadamfsrc: {}\n\
             salsaeac3jocparse: {}\n\
             salsaeac3jocdec: {}\n\
             salsatruehdparse: {}\n\
             salsatruehdatmosdec: {}\n\
             salsaoar: {}\n\
             salsahrtfrender: {}\n\
             devices: {}\n\
             model: {}\n\
             source: {}\n\
             phase: {:?}\n\
             error: {}\n\
             visual dropped frames: {}",
            gst::version_string(),
            std::env::var("GST_PLUGIN_PATH").unwrap_or_else(|_| "<unset>".to_string()),
            std::env::var("GST_REGISTRY_FORK").unwrap_or_else(|_| "<unset>".to_string()),
            plugin("salsadamfsrc"),
            plugin("salsaeac3jocparse"),
            plugin("salsaeac3jocdec"),
            plugin("salsatruehdparse"),
            plugin("salsatruehdatmosdec"),
            plugin("salsaoar"),
            plugin("salsahrtfrender"),
            devices,
            self.settings
                .binaural
                .model
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            self.source
                .as_ref()
                .map(|source| source.path.display().to_string())
                .unwrap_or_else(|| "<none>".to_string()),
            self.phase,
            self.error
                .as_ref()
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "<none>".to_string()),
            self.visual.dropped_frames(),
        )
    }

    fn maybe_persist(&mut self, ctx: &egui::Context) {
        if let Some(inner) = ctx.input(|input| input.viewport().inner_rect) {
            let size = [inner.width(), inner.height()];
            if self.settings.window_size.is_none_or(|current| {
                (current[0] - size[0]).abs() > 0.5 || (current[1] - size[1]).abs() > 0.5
            }) {
                self.settings.window_size = Some(size);
                self.dirty = true;
            }
        }
        if let Some(outer) = ctx.input(|input| input.viewport().outer_rect) {
            let position = [outer.min.x, outer.min.y];
            if self.settings.window_position.is_none_or(|current| {
                (current[0] - position[0]).abs() > 0.5 || (current[1] - position[1]).abs() > 0.5
            }) {
                self.settings.window_position = Some(position);
                self.dirty = true;
            }
        }
        if self.dirty && self.last_save.elapsed() > Duration::from_millis(600) {
            self.settings.save();
            self.dirty = false;
            self.last_save = Instant::now();
        }
    }

    fn draw_top(&mut self, ctx: &egui::Context) {
        let mut open_dialog = false;
        let mut show_help = false;
        let mut show_settings = false;
        egui::TopBottomPanel::top("salsa-top").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if ui.button("Open…").clicked() {
                    open_dialog = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Help").clicked() {
                        show_help = true;
                    }
                    if ui.button("Settings").clicked() {
                        show_settings = true;
                    }
                    if self.plugins_ok() {
                        ui.label(
                            RichText::new("Plugins ready").color(Color32::from_rgb(120, 210, 150)),
                        );
                    } else {
                        ui.label(
                            RichText::new("Plugins missing")
                                .color(Color32::from_rgb(240, 130, 120)),
                        );
                    }
                });
            });
        });
        if open_dialog {
            self.show_open_dialog();
        }
        self.show_help |= show_help;
        self.show_settings |= show_settings;
    }

    fn draw_error_banner(&mut self, ctx: &egui::Context) {
        let Some(error) = self.error.clone() else {
            return;
        };
        let mut dismiss = false;
        let mut retry = None;
        let mut choose_model = false;
        egui::TopBottomPanel::top("salsa-error").show(ctx, |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgb(62, 26, 26))
                .inner_margin(egui::Margin::same(8))
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(Color32::from_rgb(255, 190, 185), &error.message);
                        if let Some(path) = &error.retry_path
                            && ui.button("Reopen").clicked()
                        {
                            retry = Some(path.clone());
                        }
                        if error.show_model && ui.button("Choose model…").clicked() {
                            choose_model = true;
                        }
                        if ui.button("Copy diagnostics").clicked() {
                            ui.ctx().copy_text(self.diagnostics());
                        }
                        if ui.button("Dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                });
        });
        if dismiss {
            self.error = None;
        }
        if let Some(path) = retry {
            self.open_path(path);
        }
        if choose_model {
            self.pick_model();
        }
    }

    fn draw_left(&mut self, ctx: &egui::Context) {
        if self.focus_mode {
            return;
        }
        let mut output_action = None;
        egui::SidePanel::left("salsa-left")
            .resizable(true)
            .default_width(300.0)
            .width_range(250.0..=360.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.source.is_some() {
                        source::draw_source(
                            ui,
                            self.source.as_ref(),
                            self.duration,
                            self.duration_scanning,
                        );
                        ui.add_space(8.0);
                    }
                    output_action = output::draw_output(
                        ui,
                        &mut self.settings,
                        &self.devices,
                        &self.effective_layout,
                        self.warning.as_deref(),
                    );
                });
            });
        if let Some(action) = output_action {
            self.apply_output_action(action);
        }
    }

    fn draw_center(&mut self, ctx: &egui::Context) {
        let mut empty_action = None;
        let mut cancel_open = false;
        egui::CentralPanel::default().show(ctx, |ui| match self.phase {
            Phase::Empty => {
                empty_action = source::draw_empty(ui, &self.settings, self.plugins_ok());
            }
            Phase::Loading => {
                cancel_open = source::draw_loading(ui, self.open_path.as_deref());
            }
            _ => {
                let snapshot = self.visual.snapshot(self.position);
                let stale = self.switch.is_some() || self.phase == Phase::Seeking;
                let action = stage::draw(
                    ui,
                    snapshot.as_ref(),
                    &mut self.stage,
                    &stage::StageView {
                        layout_name: &self.effective_layout,
                        mode: &self.settings.mode,
                        visual_enabled: self.settings.visual_enabled,
                        labels_enabled: self.settings.labels_enabled,
                        reduce_motion: self.settings.reduce_motion,
                        stale_hint: stale,
                    },
                );
                match action {
                    Some(StageAction::ToggleFocus) => self.focus_mode = !self.focus_mode,
                    None => {}
                }
            }
        });
        match empty_action {
            Some(EmptyAction::OpenDialog) => self.show_open_dialog(),
            Some(EmptyAction::Open(path)) => self.open_path(path),
            Some(EmptyAction::CopyDiagnostics) => ctx.copy_text(self.diagnostics()),
            None => {}
        }
        if cancel_open {
            self.open_cancel.store(true, Ordering::Relaxed);
        }
    }

    fn draw_bottom(&mut self, ctx: &egui::Context) {
        self.transport.position = self.position;
        self.transport.duration = self.duration;
        self.transport.playing = self.phase == Phase::Playing;
        self.transport.scanning = self.duration_scanning;
        self.transport.visual_enabled = self.settings.visual_enabled;
        self.transport.focus = self.focus_mode;
        self.transport.volume = self.settings.volume;
        let mut action = None;
        egui::TopBottomPanel::bottom("salsa-transport").show(ctx, |ui| {
            ui.add_space(2.0);
            action = transport::draw(ui, &mut self.transport);
            ui.separator();
            ui.horizontal(|ui| {
                if let Some(source) = &self.source {
                    ui.label(format!("{} objects", source.objects));
                    ui.separator();
                    let output = match self.settings.mode {
                        OutputMode::Speaker => {
                            format!("{} speaker virtualization", self.effective_layout)
                        }
                        OutputMode::Binaural => "binaural headphone virtualization".to_string(),
                    };
                    ui.label(output);
                } else {
                    ui.label("No source");
                }
                if self.switch.is_some() {
                    ui.separator();
                    ui.label("Switching output…");
                } else if self.phase == Phase::Seeking {
                    ui.separator();
                    ui.label("Seeking…");
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (text, color) = if self.phase == Phase::Playing {
                        ("Real-time", Color32::from_rgb(120, 210, 150))
                    } else {
                        (self.phase.label(), Color32::from_gray(160))
                    };
                    ui.label(RichText::new(text).color(color));
                    ui.separator();
                    ui.weak(format!("stage drops {}", self.visual.dropped_frames()));
                });
            });
        });
        match action {
            Some(TransportAction::Toggle) => self.toggle_play(),
            Some(TransportAction::Seek(position)) => self.seek_to(position),
            Some(TransportAction::Relative(seconds)) => self.seek_relative(seconds),
            Some(TransportAction::SetVolume(volume)) => self.set_volume(volume),
            Some(TransportAction::ToggleVisual) => self.set_visual(!self.settings.visual_enabled),
            Some(TransportAction::ToggleFocus) => self.focus_mode = !self.focus_mode,
            None => {}
        }
    }

    fn draw_help(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Help")
            .open(&mut open)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.heading("Salsa Studio");
                ui.label(
                    "A preview player for object audio. Every speaker layout is rendered \
                     from objects, not played from the E-AC-3 core or a downmix substream.",
                );
                ui.add_space(6.0);
                ui.strong("Shortcuts");
                egui::Grid::new("shortcuts").num_columns(2).show(ui, |ui| {
                    for (keys, description) in [
                        ("Space", "Play / pause"),
                        ("← / →", "Seek 5 seconds"),
                        ("Shift + ← / →", "Seek 10 seconds"),
                        ("Home / End", "Jump to start / end"),
                        ("1 / 2 / 3", "Top / front / perspective camera"),
                        ("F", "Follow the selected object"),
                        ("V", "Toggle the 3D stage"),
                        ("Esc", "Leave Focus mode"),
                    ] {
                        ui.monospace(keys);
                        ui.label(description);
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                ui.strong("Terminology");
                ui.label(
                    "Speaker: salsaoar renders LFE + objects to the selected layout, \
                     including 2.0 and 5.1. Binaural: salsahrtfrender renders the same \
                     objects through a Rosella HRTF model. Object PCM is a diagnostic \
                     view only (ch0 = LFE).",
                );
                ui.add_space(6.0);
                ui.strong("3D stage");
                egui::Grid::new("stage-legend")
                    .num_columns(2)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            for color in [
                                Color32::from_rgb(64, 140, 242),
                                Color32::from_rgb(51, 217, 191),
                                Color32::from_rgb(242, 217, 64),
                                Color32::from_rgb(242, 89, 64),
                            ] {
                                ui.colored_label(color, "●");
                            }
                        });
                        ui.label("Color is height: blue is the floor, orange is the ceiling.");
                        ui.end_row();

                        ui.colored_label(Color32::from_gray(220), "●");
                        ui.label("Object position; the IDxx label is the object id.");
                        ui.end_row();

                        ui.monospace("size");
                        ui.label("Larger dot = louder decoded object level.");
                        ui.end_row();

                        ui.monospace("◯");
                        ui.label("White ring = selected or followed object (F).");
                        ui.end_row();

                        ui.monospace("line");
                        ui.label("Fading line = the object's recent motion trail.");
                        ui.end_row();

                        ui.monospace("dim");
                        ui.label("Dimmed object = stale metadata or a disabled object.");
                        ui.end_row();

                        ui.monospace("hover");
                        ui.label("Hover a dot for position, gain, and level.");
                        ui.end_row();

                        ui.monospace("■");
                        ui.label("Speaker anchors in Speaker mode (L, R, C, Ls, ...).");
                        ui.end_row();

                        ui.monospace("LFE");
                        ui.label("The LFE marker sits directly behind the listener. Its fill shows the decoded LFE level when routed. Hover for level and routing details. Its position is a display convention.");
                        ui.end_row();

                        ui.monospace("Listener");
                        ui.label("The translucent 3D head faces +Y at the listening position. Headphones indicate Binaural mode.");
                        ui.end_row();

                        ui.monospace("grid");
                        ui.label("Floor grid and height box are spatial references only.");
                        ui.end_row();
                    });
            });
        self.show_help = open;
    }

    fn draw_settings(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut reduce_motion = self.settings.reduce_motion;
        let mut visual = self.settings.visual_enabled;
        let mut labels = self.settings.labels_enabled;
        let diagnostics = self.diagnostics();
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(true)
            .default_width(430.0)
            .show(ctx, |ui| {
                ui.checkbox(&mut visual, "Show the 3D object stage");
                ui.checkbox(
                    &mut labels,
                    "Show all labels (objects, speakers, LFE, listener)",
                );
                ui.checkbox(&mut reduce_motion, "Reduce motion (no trails)");
                ui.add_space(6.0);
                if ui.button("Copy diagnostics").clicked() {
                    ui.ctx().copy_text(diagnostics.clone());
                }
                egui::CollapsingHeader::new("Diagnostics")
                    .default_open(false)
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .show(ui, |ui| {
                                ui.monospace(&diagnostics);
                            });
                    });
                ui.add_space(6.0);
                ui.weak(format!(
                    "Model: {}",
                    self.settings
                        .binaural
                        .model
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<none>".to_string())
                ));
                ui.weak(format!(
                    "Plugin path: {}",
                    std::env::var("GST_PLUGIN_PATH").unwrap_or_else(|_| "<unset>".to_string())
                ));
            });
        self.show_settings = open;
        if reduce_motion != self.settings.reduce_motion {
            self.settings.reduce_motion = reduce_motion;
            self.dirty = true;
        }
        if visual != self.settings.visual_enabled {
            self.set_visual(visual);
        }
        if labels != self.settings.labels_enabled {
            self.settings.labels_enabled = labels;
            self.dirty = true;
        }
    }
}

impl eframe::App for SalsaApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_shortcuts(ctx);
        self.handle_dropped_files(ctx);
        self.poll_open(ctx);
        self.poll_duration();
        self.poll_switch();
        self.poll_player(ctx);

        self.draw_top(ctx);
        self.draw_error_banner(ctx);
        self.draw_left(ctx);
        self.draw_center(ctx);
        self.draw_bottom(ctx);
        self.draw_help(ctx);
        self.draw_settings(ctx);
        self.maybe_persist(ctx);

        if self.phase.is_playing()
            || self.switch.is_some()
            || self.open_rx.is_some()
            || self.settings.visual_enabled
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.settings.save();
    }
}

fn source_from_player(player: &Player, path: &std::path::Path) -> Result<SourceInfo, PlayerError> {
    let mut caps = None;
    for _ in 0..40 {
        caps = player
            .decoder()
            .static_pad("src")
            .and_then(|pad| pad.current_caps());
        if caps.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !player.object_audio() {
        return Err(PlayerError::NoObjectAudio);
    }
    let Some(caps) = caps else {
        return Err(PlayerError::Playback(
            "The decoder produced no caps; the stream may be unsupported.".to_string(),
        ));
    };
    let structure = caps
        .structure(0)
        .ok_or_else(|| PlayerError::Playback("Empty decoder caps".to_string()))?;
    let rate = structure.get::<i32>("rate").unwrap_or(48_000).max(0) as u32;
    let channels = structure.get::<i32>("channels").unwrap_or(0).max(0) as u32;
    if channels <= 1 {
        return Err(PlayerError::NoObjectAudio);
    }
    Ok(SourceInfo {
        path: path.to_path_buf(),
        codec: player.codec(),
        rate,
        objects: channels - 1,
    })
}

fn error_to_ui(error: PlayerError, retry: Option<PathBuf>) -> UiError {
    match error {
        PlayerError::Cancelled => UiError::new("Open cancelled."),
        PlayerError::Pipeline(crate::pipeline::PipelineError::ModelRequired) => {
            UiError::missing_model()
        }
        PlayerError::NoObjectAudio => UiError::new(
            "No object audio found (no JOC/OAMD metadata). This is not an Atmos stream; open it in a standard player.",
        ),
        other => {
            let message = format!(
                "{} If this is not an object-audio stream, open it in a standard player.",
                other
            );
            match retry {
                Some(path) => UiError::with_retry(message, path),
                None => UiError::new(message),
            }
        }
    }
}

fn find_bundled_model() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let resources = executable.parent()?.parent()?.join("Resources");
    let entries = std::fs::read_dir(resources).ok()?;
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.extension().and_then(|extension| extension.to_str())
                == Some("personalized_headphone")
        })
}
