//! Clicking the acquisition software's Z controls.
//!
//! The mechanism is the autoclicker's, deliberately unchanged: `enigo` 0.1.3,
//! `mouse_move_to` then `mouse_click`, `key_sequence` for text, `key_down` /
//! `key_up` in order and in reverse for a hotkey, and the mouse in the top-left
//! corner as an emergency stop. That tool is already trusted on this rig and its
//! recorded workflows are already written in this vocabulary; a second, subtly
//! different click implementation beside it would be a liability.
//!
//! # Being armed
//!
//! Unarmed, this touches nothing. It does not create an `Enigo`, does not move
//! the pointer, and reports what it would have done. That is the default and it
//! is the only sane one: the coordinates in a fresh `config.yaml` are
//! placeholders, and an unarmed first session is how the user finds out that
//! they are.
//!
//! # The emergency stop
//!
//! The pointer in the upper-left corner means stop, checked before every step of
//! every sequence — not once per correction. A sequence is several clicks over a
//! second or more, and the whole point of the corner is that a human who sees
//! something wrong can stop it *now*.
//!
//! This has one consequence worth stating: the program moves the pointer, so the
//! user's own pointer position is not preserved. There is no way around that with
//! a click-driven actuator, and it is why `arm` defaults to false.
//!
//! It has a second consequence, less obvious. Between our own `mouse_move_to`
//! and the next step's check, the pointer is wherever *we* put it — so the check
//! only sees the user's hand if the user got to the mouse during a gap between
//! steps. The `delay` on each step is therefore not merely cosmetic: it is the
//! width of the window in which a human can take the machine back. A sequence of
//! zero-delay steps is a sequence that cannot be interrupted part way.
//!
//! And a third, which is a trap rather than a consequence: a `z_up` that clicks
//! at `(0, 0)` stops the session on its own next step, because the corner is
//! then exactly where the actuator left the pointer. That is the right thing to
//! do with such a coordinate — it is a placeholder nobody replaced — but the
//! reason it stops is worth knowing before spending a session puzzling at it.
//!
//! # Refusal, and why it applies to a dry run too
//!
//! A sequence is resolved completely before its first click: an empty sequence,
//! an unknown key name, or a `{z}` with no z to put in it is [`Applied::Refused`]
//! and nothing happens. Running half a sequence is the worst outcome available —
//! it leaves the software's Z field half-typed, or a modifier held down.
//!
//! That check runs *before* the armed/unarmed branch, which means an unarmed run
//! refuses exactly what an armed run would have refused. This is on purpose. The
//! point of a dry run is to find out that the configuration is wrong while it is
//! still harmless; a dry run that answered `DryRun` where the armed run would
//! have answered `Refused` would be lying about the thing it exists to test.

use crate::config::{Config, Step};

use enigo::{KeyboardControllable, MouseButton, MouseControllable};

/// The `{z}` placeholder a `text_input` uses to ask for the absolute z.
const PLACEHOLDER_Z: &str = "{z}";

pub struct Actuator {
    armed: bool,
    emergency_corner: bool,
    settle: std::time::Duration,
    up: Vec<Step>,
    down: Vec<Step>,
    /// `None` until armed: creating an `Enigo` is what takes hold of the input
    /// system, so an unarmed run never makes one.
    enigo: Option<enigo::Enigo>,
    stopped: bool,
}

/// What happened when a correction was attempted.
#[derive(Debug, Clone, PartialEq)]
pub enum Applied {
    /// Steps carried out.
    Done { steps: i32 },
    /// Not armed; this is what it would have done.
    DryRun { steps: i32 },
    /// The user put the pointer in the corner. Nothing further will be attempted.
    Stopped,
    /// The sequence for that direction is empty, or a key name in it is unknown.
    Refused(String),
}

impl Actuator {
    pub fn new(cfg: &Config) -> Actuator {
        let armed = cfg.actuator.arm;
        Actuator {
            armed,
            emergency_corner: cfg.actuator.emergency_stop_corner,
            settle: duration_secs(cfg.actuator.settle_s),
            up: cfg.actuator.z_up.clone(),
            down: cfg.actuator.z_down.clone(),
            // Made here rather than lazily on the first correction, because the
            // session polls `check_stop` from its very first loop and a stop that
            // only worked once a click had already been issued would be no stop
            // at all. Unarmed, this stays `None` for the whole run and nothing in
            // this file can then reach the input system.
            enigo: armed.then(enigo::Enigo::new),
            stopped: false,
        }
    }

    /// True once the emergency stop has been seen. Sticky: a stop is not undone
    /// by moving the mouse away, because the user who reached for the corner
    /// wanted it to stay stopped.
    // Exercised by the tests rather than by the session, which asks through
    // `check_stop`. Kept because "is it stopped" is part of this type's contract.
    #[allow(dead_code)]
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Is the pointer in the upper-left corner right now?
    ///
    /// Always false when unarmed, since an unarmed run has no `Enigo` to ask and
    /// nothing to stop.
    pub fn check_stop(&mut self) -> bool {
        // Asked first, and not only as an optimisation: this is what makes the
        // stop sticky, and it is why the answer does not depend on where the
        // pointer has wandered since.
        if self.stopped {
            return true;
        }
        if !self.emergency_corner {
            return false;
        }
        let Some(enigo) = self.enigo.as_mut() else {
            return false;
        };
        let (x, y) = enigo.mouse_location();
        if x == 0 && y == 0 {
            self.stopped = true;
        }
        self.stopped
    }

    /// Run the `z_up` or `z_down` sequence `steps.abs()` times.
    ///
    /// `absolute_z` is substituted for `{z}` in any `text_input`, which is how
    /// software driven by a numeric Z field is handled.
    pub fn apply(&mut self, steps: i32, absolute_z: Option<f64>) -> Applied {
        // Ahead of everything, including validation: a stopped actuator has
        // nothing to say about the configuration, it just does not move.
        if self.stopped {
            return Applied::Stopped;
        }

        // Nothing to do. No sequence is needed to do nothing, so none is
        // demanded — a controller that asks for zero steps should not be told its
        // configuration is broken.
        if steps == 0 {
            return if self.armed {
                Applied::Done { steps: 0 }
            } else {
                Applied::DryRun { steps: 0 }
            };
        }

        // Cloned, rather than borrowed, because running it needs `&mut self` for
        // the stop check. A sequence is a handful of steps and a correction
        // happens every few seconds at most, so the copy costs nothing worth
        // restructuring the borrow for.
        let (which, seq) = if steps > 0 {
            ("actuator.z_up", self.up.clone())
        } else {
            ("actuator.z_down", self.down.clone())
        };

        if let Err(why) = check_sequence(which, &seq, absolute_z) {
            return Applied::Refused(why);
        }

        if !self.armed {
            return Applied::DryRun { steps };
        }

        // `unsigned_abs`, not `abs`: `i32::MIN.abs()` panics. The controller caps
        // this at `max_steps_per_event` so it cannot happen, but a panic here
        // would be a panic half-way through a click sequence on an armed rig, and
        // that is not a thing to leave to another module's arithmetic.
        for _ in 0..steps.unsigned_abs() {
            if let Err(why) = self.run_sequence(&seq, absolute_z) {
                return Applied::Refused(why);
            }
            if self.stopped {
                // Some of the repetitions may already have moved the stage. The
                // caller is told `Stopped` and ends the session, so nothing is
                // credited to the controller: after a stop the only honest
                // statement about the stage position is that a human is dealing
                // with it.
                return Applied::Stopped;
            }
        }

        // The stage has to arrive, and the acquisition software may take a frame
        // or two to act on the click at all. Waiting here rather than in the
        // measurement loop keeps "a correction takes this long to settle" in the
        // one place that knows a correction happened.
        std::thread::sleep(self.settle);
        Applied::Done { steps }
    }

    /// One sequence, once.
    fn run_sequence(&mut self, steps: &[Step], absolute_z: Option<f64>) -> Result<(), String> {
        for step in steps {
            // Before every step, as promised. A stop is reported by leaving
            // `self.stopped` set and returning `Ok`: stopping is not an error in
            // the configuration, which is all `Err` means here.
            if self.check_stop() {
                return Ok(());
            }
            let Some(enigo) = self.enigo.as_mut() else {
                // Unreachable: `apply` only calls this when armed, and an armed
                // actuator has an `Enigo` from construction.
                return Err("not armed".into());
            };
            match step {
                Step::Click { x, y, delay } => {
                    enigo.mouse_move_to(*x, *y);
                    enigo.mouse_click(MouseButton::Left);
                    std::thread::sleep(duration_secs(*delay));
                }
                Step::RightClick { x, y, delay } => {
                    enigo.mouse_move_to(*x, *y);
                    enigo.mouse_click(MouseButton::Right);
                    std::thread::sleep(duration_secs(*delay));
                }
                Step::TextInput { text, delay } => {
                    let text = substitute(text, absolute_z);
                    enigo.key_sequence(&text);
                    std::thread::sleep(duration_secs(*delay));
                }
                Step::PressKey { key, delay } => {
                    // `check_sequence` has already resolved every name, so the
                    // `None` arm is unreachable. It returns rather than skipping
                    // anyway — the autoclicker skips an unknown key and carries
                    // on, which is right for a workflow a human is watching and
                    // wrong for a stage that is being moved unattended.
                    match parse_key(key) {
                        Some(k) => enigo.key_click(k),
                        None => return Err(format!("unknown key name {key:?}")),
                    }
                    std::thread::sleep(duration_secs(*delay));
                }
                Step::Hotkey { keys, delay } => {
                    let mut resolved = Vec::with_capacity(keys.len());
                    for name in keys {
                        match parse_key(name) {
                            Some(k) => resolved.push(k),
                            None => return Err(format!("unknown key name {name:?}")),
                        }
                    }
                    // Down in order, up in reverse, so the modifiers wrap the key
                    // they modify. Releasing in issue order instead would let go
                    // of Ctrl while `c` was still down.
                    for &k in &resolved {
                        enigo.key_down(k);
                    }
                    for &k in resolved.iter().rev() {
                        enigo.key_up(k);
                    }
                    std::thread::sleep(duration_secs(*delay));
                }
            }
        }
        Ok(())
    }
}

/// Everything about a sequence that can be known to be wrong before its first
/// click, checked in one pass so that nothing ever runs half way.
///
/// `which` is the config key being checked, so the message says which of the two
/// directions is at fault — the user has to go and edit one of them.
fn check_sequence(which: &str, seq: &[Step], absolute_z: Option<f64>) -> Result<(), String> {
    if seq.is_empty() {
        return Err(format!(
            "{which} is empty: there is no sequence configured for that direction"
        ));
    }
    for step in seq {
        match step {
            Step::PressKey { key, .. } => {
                if parse_key(key).is_none() {
                    return Err(format!("{which}: press_key names an unknown key {key:?}"));
                }
            }
            Step::Hotkey { keys, .. } => {
                if keys.is_empty() {
                    return Err(format!("{which}: hotkey has no keys in it"));
                }
                for name in keys {
                    if parse_key(name).is_none() {
                        return Err(format!("{which}: hotkey names an unknown key {name:?}"));
                    }
                }
            }
            Step::TextInput { text, .. } => {
                // Typing the literal characters `{z}` into the software's Z field
                // is the quietly wrong thing here, so this refuses instead. The z
                // is unknown early in a recording — before the file has reported
                // a `zPosition`, or while microns-per-step is still being learned
                // — so this is a real state, not a misconfiguration.
                if text.contains(PLACEHOLDER_Z) && absolute_z.is_none() {
                    return Err(format!(
                        "{which}: a text_input asks for {PLACEHOLDER_Z}, but the absolute z is not \
                         known yet — the file has reported no zPosition, or microns per step is \
                         not known"
                    ));
                }
            }
            Step::Click { .. } | Step::RightClick { .. } => {}
        }
    }
    Ok(())
}

/// `{z}` replaced by the absolute z, formatted as the acquisition software writes
/// it: two decimals, as in `9741.19`.
///
/// One limitation, stated because it will eventually bite someone: this writes a
/// decimal point. A Windows install whose locale wants `9741,19` in that field
/// will get a point typed into it, and `config.yaml` has no way to ask for a
/// comma. If that turns out to matter, it is a change here, not in the config.
fn substitute(text: &str, absolute_z: Option<f64>) -> String {
    match absolute_z {
        Some(z) => text.replace(PLACEHOLDER_Z, &format!("{z:.2}")),
        // Only reachable for text with no placeholder in it, since a placeholder
        // with no z was refused before anything ran.
        None => text.to_string(),
    }
}

/// Seconds from `config.yaml` as a `Duration`, without the panic.
///
/// `Duration::from_secs_f64` panics on a negative, on NaN and on an infinity, and
/// all three can be typed into a YAML file (`.inf` and `.nan` are valid YAML
/// floats). A panic in an armed sequence is the one failure mode worth this much
/// fuss, so nonsense becomes zero and an absurd-but-finite wait is capped at a
/// day rather than being allowed to exceed what a `Duration` can hold.
fn duration_secs(secs: f64) -> std::time::Duration {
    if !secs.is_finite() || secs <= 0.0 {
        std::time::Duration::ZERO
    } else {
        std::time::Duration::from_secs_f64(secs.min(86_400.0))
    }
}

/// `enigo::Key` for a key name, the autoclicker's table.
///
/// Kept identical to that tool's `parse_key` so a sequence recorded there means
/// the same thing here — including the aliases (`esc`, `del`, `ins`, `ctrl`) that
/// its recorder writes.
///
/// Two quirks are copied along with it, because a sequence that means one thing
/// in that tool has to mean the same thing here: the name is lower-cased but not
/// trimmed, so `"esc "` is unknown; and the table stops at `f20` even though
/// `enigo` knows `F21` upwards.
// Left exactly as the autoclicker writes it, aligned columns and all: this table
// is the compatibility contract between the two tools, and a reader comparing them
// side by side should see the same shape.
#[rustfmt::skip]
pub fn parse_key(name: &str) -> Option<enigo::Key> {
    use enigo::Key;
    match name.to_lowercase().as_str() {
        // --- Navigation ---
        "tab"                       => Some(Key::Tab),
        "escape" | "esc"            => Some(Key::Escape),
        "space"                     => Some(Key::Space),
        "backspace"                 => Some(Key::Backspace),
        "delete" | "del"            => Some(Key::Delete),
        "insert" | "ins"            => Some(Key::Insert),
        "up"                        => Some(Key::UpArrow),
        "down"                      => Some(Key::DownArrow),
        "left"                      => Some(Key::LeftArrow),
        "right"                     => Some(Key::RightArrow),
        "home"                      => Some(Key::Home),
        "end"                       => Some(Key::End),
        "pageup"   | "page_up"      => Some(Key::PageUp),
        "pagedown" | "page_down"    => Some(Key::PageDown),
        // --- Modifiers ---
        "ctrl" | "control"          => Some(Key::Control),
        "lctrl" | "lcontrol"        => Some(Key::LControl),
        "rctrl" | "rcontrol"        => Some(Key::RControl),
        "alt"                       => Some(Key::Alt),
        "shift"                     => Some(Key::Shift),
        "lshift"                    => Some(Key::LShift),
        "rshift"                    => Some(Key::RShift),
        "super" | "win" | "meta"    => Some(Key::Meta),
        "capslock" | "caps"         => Some(Key::CapsLock),
        "numlock"                   => Some(Key::Numlock),
        // --- System / misc ---
        "return" | "enter"          => Some(Key::Return),
        "pause"                     => Some(Key::Pause),
        "print" | "printscreen"     => Some(Key::Print),
        "help"                      => Some(Key::Help),
        "select"                    => Some(Key::Select),
        "execute"                   => Some(Key::Execute),
        "clear"                     => Some(Key::Clear),
        "cancel"                    => Some(Key::Cancel),
        // --- Media ---
        "volup"   | "volumeup"      => Some(Key::VolumeUp),
        "voldown" | "volumedown"    => Some(Key::VolumeDown),
        "mute" | "volumemute"       => Some(Key::VolumeMute),
        "medianext" | "nexttrack"   => Some(Key::MediaNextTrack),
        "mediaprev" | "prevtrack"   => Some(Key::MediaPrevTrack),
        "mediastop"                 => Some(Key::MediaStop),
        "mediaplay" | "playpause"   => Some(Key::MediaPlayPause),
        // --- Numpad ---
        "num0" | "numpad0"          => Some(Key::Numpad0),
        "num1" | "numpad1"          => Some(Key::Numpad1),
        "num2" | "numpad2"          => Some(Key::Numpad2),
        "num3" | "numpad3"          => Some(Key::Numpad3),
        "num4" | "numpad4"          => Some(Key::Numpad4),
        "num5" | "numpad5"          => Some(Key::Numpad5),
        "num6" | "numpad6"          => Some(Key::Numpad6),
        "num7" | "numpad7"          => Some(Key::Numpad7),
        "num8" | "numpad8"          => Some(Key::Numpad8),
        "num9" | "numpad9"          => Some(Key::Numpad9),
        "numadd"  | "numplus"       => Some(Key::Add),
        "numsub"  | "numminus"      => Some(Key::Subtract),
        "nummul"  | "nummultiply"   => Some(Key::Multiply),
        "numdiv"  | "numdivide"     => Some(Key::Divide),
        "numdec"  | "numdecimal"    => Some(Key::Decimal),
        // --- F-keys (extended to F20) ---
        "f1"  => Some(Key::F1),  "f2"  => Some(Key::F2),
        "f3"  => Some(Key::F3),  "f4"  => Some(Key::F4),
        "f5"  => Some(Key::F5),  "f6"  => Some(Key::F6),
        "f7"  => Some(Key::F7),  "f8"  => Some(Key::F8),
        "f9"  => Some(Key::F9),  "f10" => Some(Key::F10),
        "f11" => Some(Key::F11), "f12" => Some(Key::F12),
        "f13" => Some(Key::F13), "f14" => Some(Key::F14),
        "f15" => Some(Key::F15), "f16" => Some(Key::F16),
        "f17" => Some(Key::F17), "f18" => Some(Key::F18),
        "f19" => Some(Key::F19), "f20" => Some(Key::F20),
        // --- Single character fallback (layout-dependent) ---
        // A single printable char like "a", "1", "+" maps to Key::Layout.
        s if s.chars().count() == 1 => Some(Key::Layout(s.chars().next().unwrap())),
        _ => None,
    }
}

/// Read screen coordinates off the acquisition software with the mouse, printing
/// the position until the pointer is put in the corner.
///
/// This is `--where`, and it is the answer to "what do I put in `z_up`". It is
/// the autoclicker's live-position helper, which is the only practical way to
/// fill in a click coordinate.
///
/// It creates an `Enigo` even though it clicks nothing: asking for the pointer
/// position goes through the same object as moving it.
pub fn show_mouse_position() {
    use std::io::Write;

    let enigo = enigo::Enigo::new();
    println!("\nLive position \nMove mouse to UPPER-LEFT corner (0, 0) to stop\n");
    let mut last = (0, 0);
    loop {
        let (x, y) = enigo.mouse_location();
        print!("\rX: {x:4} | Y: {y:4}");
        let _ = std::io::stdout().flush();
        std::thread::sleep(std::time::Duration::from_millis(200));
        if x == 0 && y == 0 {
            break;
        }
        last = (x, y);
    }
    // The position that was being pointed at, in the form it has to be pasted
    // in. Printed after the loop because the last reading is the corner, not the
    // control — the user has to take their hand off the mouse to stop, so the
    // useful number is the one before that.
    println!("\n\nLast position before the corner, as a config.yaml step:");
    println!(
        "  - {{step: click, x: {}, y: {}, delay: 0.3}}",
        last.0, last.1
    );
}

#[cfg(test)]
#[path = "actuator_tests.rs"]
mod actuator_tests;
