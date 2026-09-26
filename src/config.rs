//! `config.yaml`, as types.
//!
//! Every field has a `#[serde(default)]` so that a config file written for an
//! older version of this program still loads, and so that a user can delete the
//! parts they do not care about rather than being made to state all of it. The
//! defaults here and the values in the shipped `config.yaml` are the same
//! numbers; if they ever disagree, these win, because these are what runs.
//!
//! [`Config::validate`] is separate from loading on purpose. Anything that can
//! be checked before the first frame is checked before the first frame — a
//! `reference_stack` mode with no stack path, an ROI larger than the frame, an
//! empty `z_up` while armed — because the alternative is finding out forty
//! minutes into a recording that cannot be repeated.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One action in a `z_up` / `z_down` sequence.
///
/// Deliberately the same vocabulary, spelling and defaults as the autoclicker's
/// workflow files, so that a sequence recorded with that tool's recorder can be
/// pasted in here unchanged. That is the whole reason this is a tagged enum with
/// these names.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "step")]
pub enum Step {
    #[serde(rename = "click")]
    Click {
        x: i32,
        y: i32,
        #[serde(default = "default_delay")]
        delay: f64,
    },
    #[serde(rename = "right_click")]
    RightClick {
        x: i32,
        y: i32,
        #[serde(default = "default_delay")]
        delay: f64,
    },
    /// `{z}` in `text` is replaced with the absolute z being asked for, in
    /// microns, formatted as the acquisition software writes it.
    #[serde(rename = "text_input")]
    TextInput {
        text: String,
        #[serde(default = "default_delay")]
        delay: f64,
    },
    #[serde(rename = "press_key")]
    PressKey {
        key: String,
        #[serde(default = "default_delay")]
        delay: f64,
    },
    #[serde(rename = "hotkey")]
    Hotkey {
        keys: Vec<String>,
        #[serde(default = "default_delay")]
        delay: f64,
    },
}

fn default_delay() -> f64 {
    0.2
}

/// Which channel of the recording focus is measured on.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum ChannelPick {
    /// A channel index, in the order the file first mentions each channel.
    Index(usize),
    /// The literal string `sum`, adding every channel together.
    Named(String),
}

impl Default for ChannelPick {
    fn default() -> Self {
        ChannelPick::Index(0)
    }
}

impl ChannelPick {
    /// `None` means "add them all".
    pub fn index(&self) -> Option<usize> {
        match self {
            ChannelPick::Index(i) => Some(*i),
            ChannelPick::Named(s) if s.eq_ignore_ascii_case("sum") => None,
            // Anything else was rejected by `validate`, so this is unreachable
            // in a running program; falling back to the first channel rather
            // than panicking keeps a typo from being fatal mid-recording.
            ChannelPick::Named(_) => Some(0),
        }
    }
}

/// Which focus metric is tracked. See `config.yaml` for what each one is good
/// and bad at; the short version is that `HighFreqRatio` is the only one that
/// bleaching cannot fool.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    #[default]
    HighFreqRatio,
    NormVariance,
    Brenner,
    Tenengrad,
    TopPercentile,
}

/// How the direction of a drift is decided.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    HillClimb,
    ReferenceStack,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    pub channel: ChannelPick,
    /// `[x, y, width, height]` in pixels of the full frame, or `None`.
    pub roi: Option<[usize; 4]>,
    pub downsample: usize,
    pub poll_interval_ms: u64,
    pub idle_timeout_s: u64,
}

impl Default for Input {
    fn default() -> Self {
        Input {
            channel: ChannelPick::default(),
            roi: None,
            downsample: 1,
            poll_interval_ms: 500,
            idle_timeout_s: 60,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Registration {
    pub enabled: bool,
    pub max_shift_px: usize,
    pub taper: f32,
    pub min_peak: f32,
    /// Consecutive failed matches after which the alignment reference is replaced
    /// with the current frame. See `config.yaml`; this is not optional in any
    /// recording that runs for more than a few minutes.
    pub refresh_after: usize,
}

impl Default for Registration {
    fn default() -> Self {
        Registration {
            enabled: true,
            max_shift_px: 64,
            taper: 5.0,
            min_peak: 0.03,
            // Five frames is under a second at 7.5 Hz, so a stale reference costs
            // almost nothing, while five consecutive failures is far more than a
            // single bad frame produces.
            refresh_after: 5,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct HighFreq {
    pub low_cut: f32,
    pub high_cut: f32,
}

impl Default for HighFreq {
    fn default() -> Self {
        // Measured, not chosen. Over the 29-minute reference acquisition — in
        // which a drug application nearly triples the fluorescence, so it is a
        // hard case rather than a gentle one — these cuts gave the best ratio of
        // defocus response to honest drift across a grid of seven low cuts and
        // five high ones. The first guess here was 0.05/0.35, which drifted 40%
        // over that recording because the band above 0.35 Nyquist is mostly shot
        // noise, and shot noise falls as a fraction of the signal when the signal
        // gets brighter. See `measure.high_freq` in config.yaml.
        HighFreq {
            low_cut: 0.12,
            high_cut: 0.30,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Measure {
    pub reference_frames: usize,
    pub reference_skip: usize,
    pub window_frames: usize,
    pub registration: Registration,
    pub metric: Metric,
    pub high_freq: HighFreq,
    pub saturated_fraction: f32,
}

impl Default for Measure {
    fn default() -> Self {
        Measure {
            reference_frames: 30,
            reference_skip: 10,
            window_frames: 30,
            registration: Registration::default(),
            metric: Metric::default(),
            high_freq: HighFreq::default(),
            // 0.20, not the 0.01 this started at. Measured on the reference
            // recording: a real frame from its brighter half clips 2.9% of its
            // pixels, and the focus signal is entirely unharmed by that — a
            // 0.5 px defocus still moves the metric by 31% to 38%, the same as in
            // the dim parts. A 1% limit therefore rejected two thirds of the
            // recording as unmeasurable while nothing was wrong with it.
            //
            // The guard is for a field clipped flat, which is a different thing
            // from a field with bright cell bodies in it.
            saturated_fraction: 0.20,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct HillClimb {
    pub probe_steps: i32,
    /// `1` tries `z_up` first, `-1` tries `z_down`.
    pub initial_direction: i32,
}

impl Default for HillClimb {
    fn default() -> Self {
        HillClimb {
            probe_steps: 1,
            initial_direction: 1,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct ReferenceStack {
    pub path: Option<PathBuf>,
    pub step_um: f64,
    pub min_score: f32,
}

impl Default for ReferenceStack {
    fn default() -> Self {
        ReferenceStack {
            path: None,
            step_um: 1.0,
            min_score: 0.3,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Control {
    pub mode: Mode,
    pub dead_band: f32,
    pub confirm_windows: usize,
    pub cooldown_s: f64,
    pub max_steps_per_event: i32,
    pub max_total_steps: i32,
    pub hill_climb: HillClimb,
    pub reference_stack: ReferenceStack,
}

impl Default for Control {
    fn default() -> Self {
        Control {
            mode: Mode::default(),
            // 0.20, not the 0.03 this started at. Measured by running this
            // program over the whole 29-minute reference acquisition: across
            // 12,492 measured frames the metric spans 0.914 to 1.042 of its
            // starting value with nothing defocused, a worst honest drop of 8.6%.
            // A 3% band would have fired in the first minutes and gone on firing.
            //
            // 0.20 leaves better than a 2x margin over that, and still catches a
            // defocus of about 0.45 px of blur — against which the metric moves
            // more than 30%.
            //
            // This is the number most worth re-measuring per rig, with `--replay`
            // on an old recording. It is a property of the sample and the optics,
            // not of this program.
            dead_band: 0.20,
            confirm_windows: 2,
            cooldown_s: 10.0,
            max_steps_per_event: 3,
            max_total_steps: 40,
            hill_climb: HillClimb::default(),
            reference_stack: ReferenceStack::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Actuator {
    pub arm: bool,
    pub um_per_step: Option<f64>,
    pub verify_with_zposition: bool,
    pub emergency_stop_corner: bool,
    pub settle_s: f64,
    pub z_up: Vec<Step>,
    pub z_down: Vec<Step>,
}

impl Default for Actuator {
    fn default() -> Self {
        Actuator {
            // False, always. An `arm` that defaulted to true would mean a
            // config file with a typo in it — or no config file at all — starts
            // clicking on a rig whose coordinates it does not know.
            arm: false,
            um_per_step: None,
            verify_with_zposition: true,
            emergency_stop_corner: true,
            settle_s: 1.5,
            z_up: Vec::new(),
            z_down: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    pub csv: Option<PathBuf>,
    pub print_every: u64,
}

impl Default for Log {
    fn default() -> Self {
        Log {
            csv: Some(PathBuf::from("zstab-log.csv")),
            print_every: 8,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub input: Input,
    pub measure: Measure,
    pub control: Control,
    pub actuator: Actuator,
    pub log: Log,
}

impl Config {
    /// Read a config file.
    ///
    /// `deny_unknown_fields` throughout means a misspelled key is an error
    /// rather than a setting silently left at its default — the failure mode
    /// where someone writes `dead_zone` for `dead_band`, sees no complaint, and
    /// believes they have changed something.
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The config file to use when none was named: `config.yaml` beside the exe,
    /// then `config.yaml` in the current directory.
    ///
    /// Beside the exe comes first because that is where it belongs on the
    /// acquisition machine — the program is started by dropping a file onto it,
    /// and the current directory is then whatever Explorer happened to pick.
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let beside = dir.join("config.yaml");
                if beside.is_file() {
                    return Some(beside);
                }
            }
        }
        let here = PathBuf::from("config.yaml");
        here.is_file().then_some(here)
    }

    /// Everything that can be known to be wrong before the first frame.
    ///
    /// Returns every problem found rather than the first, so that a config being
    /// set up for a new rig can be fixed in one pass instead of one error per
    /// run.
    pub fn validate(&self) -> Vec<String> {
        let mut bad = Vec::new();

        if let ChannelPick::Named(s) = &self.input.channel {
            if !s.eq_ignore_ascii_case("sum") {
                bad.push(format!(
                    "input.channel: {s:?} is neither a channel number nor `sum`"
                ));
            }
        }
        if self.input.downsample == 0 {
            bad.push("input.downsample: must be at least 1".into());
        }
        if let Some([_, _, w, h]) = self.input.roi {
            if w == 0 || h == 0 {
                bad.push("input.roi: width and height must both be non-zero".into());
            }
        }
        if self.input.poll_interval_ms == 0 {
            bad.push("input.poll_interval_ms: must be at least 1".into());
        }

        if self.measure.reference_frames == 0 {
            bad.push("measure.reference_frames: must be at least 1".into());
        }
        if self.measure.window_frames == 0 {
            bad.push("measure.window_frames: must be at least 1".into());
        }
        if self.measure.registration.refresh_after == 0 {
            bad.push(
                "measure.registration.refresh_after: must be at least 1 — a reference that is \
                 never refreshed goes stale and the program stops measuring part-way through a \
                 recording"
                    .into(),
            );
        }
        let hf = &self.measure.high_freq;
        if !(0.0..1.0).contains(&hf.low_cut) || !(0.0..=1.0).contains(&hf.high_cut) {
            bad.push("measure.high_freq: cuts are fractions of Nyquist, so 0.0 to 1.0".into());
        } else if hf.high_cut <= hf.low_cut {
            bad.push("measure.high_freq: high_cut must be above low_cut".into());
        }

        if !(0.0..1.0).contains(&self.control.dead_band) {
            bad.push("control.dead_band: a relative drop, so 0.0 to 1.0".into());
        }
        if self.control.confirm_windows == 0 {
            bad.push("control.confirm_windows: must be at least 1".into());
        }
        if self.control.max_steps_per_event < 1 {
            bad.push("control.max_steps_per_event: must be at least 1".into());
        }
        if self.control.max_total_steps < 1 {
            bad.push("control.max_total_steps: must be at least 1".into());
        }

        match self.control.mode {
            Mode::ReferenceStack => {
                let rs = &self.control.reference_stack;
                match &rs.path {
                    None => bad.push(
                        "control.reference_stack.path: required by `mode: reference_stack`".into(),
                    ),
                    Some(p) if !p.is_file() => bad.push(format!(
                        "control.reference_stack.path: {} is not a file",
                        p.display()
                    )),
                    Some(_) => {}
                }
                if rs.step_um <= 0.0 {
                    bad.push("control.reference_stack.step_um: must be above zero".into());
                }
                // Turning a measured offset in microns into a number of clicks
                // needs to know what a click is worth, and in this mode there is
                // no probe to learn it from.
                if self.actuator.um_per_step.unwrap_or(0.0) <= 0.0 {
                    bad.push(
                        "actuator.um_per_step: `mode: reference_stack` measures the drift in \
                         microns, so it has to be told how many microns one step moves"
                            .into(),
                    );
                }
            }
            Mode::HillClimb => {
                if ![1, -1].contains(&self.control.hill_climb.initial_direction) {
                    bad.push("control.hill_climb.initial_direction: 1 (up) or -1 (down)".into());
                }
                if self.control.hill_climb.probe_steps < 1 {
                    bad.push("control.hill_climb.probe_steps: must be at least 1".into());
                }
            }
        }

        // Only an armed run needs working click sequences; a dry run is
        // perfectly useful without them, and demanding them would stop the
        // first, most important session on a new rig from happening at all.
        if self.actuator.arm {
            if self.actuator.z_up.is_empty() {
                bad.push("actuator.z_up: armed, but no steps to move up".into());
            }
            if self.actuator.z_down.is_empty() {
                bad.push("actuator.z_down: armed, but no steps to move down".into());
            }
        }
        if self.actuator.um_per_step.is_some_and(|u| u <= 0.0) {
            bad.push("actuator.um_per_step: must be above zero, or null to learn it".into());
        }

        bad
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
