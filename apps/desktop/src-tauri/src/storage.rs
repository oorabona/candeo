//! Storage for effects and settings.
//!
//! Three locations, see
//! [`docs/design/effects-library.md`](../../../../docs/design/effects-library.md):
//!
//! ```text
//! app_data_dir()/effects/<name>.ts        the effects, one file each, named after the effect
//! app_cache_dir()/effects/<name>.json     what compiling them produced: JavaScript, manifest, swatch
//! app_config_dir()/settings.json          preferences · devices · applied effect · effect parameters
//! ```
//!
//! The effect is **content**; the choice of the active effect is
//! **configuration**. On Windows the data and configuration directories are the
//! same, on Linux they are not — hence going through the Tauri API rather than a
//! constant.
//!
//! # An effect is a file, and its name is the file name
//!
//! Listing the folder is the whole library, and adding an effect is saving a
//! file there. The name is the key of everything that refers to an effect:
//! `settings.json`, the engine, the tray menu. Renaming from the application
//! moves those references; renaming outside it makes a new effect.
//!
//! The Rust side cannot strip TypeScript types, the window can: it compiles the
//! files whose cache is stale and hands the JavaScript back through
//! [`Store::cache_effect`], which runs the module once to read what it declares.
//! Nothing runs code whose cache does not match the file's current bytes.
//!
//! # The shape of the file: preferences on one side, devices on the other
//!
//! `settings.json` holds two things that do not belong together: what applies to
//! the whole application ([`Preferences`]) and what is **indexed by device**
//! (`devices`, `activeEffects`, `effectParams`). Mixing them at the root is what
//! produced the three single-device leftovers removed here: `activeEffect`,
//! `device` and `brightness` described **one** effect, **one** device and **one**
//! level, while the engine has run one effect per device since issue #26. It was
//! not the wrong value, it was the wrong **shape** — and bringing it back as it
//! was would have produced a file that misdescribes reality.
//!
//! The rule that follows holds for everything added later: **a global preference
//! goes in `preferences`, anything that depends on a keyboard goes in an indexed
//! list.** The language, when it arrives, therefore has nothing to decide.
//!
//! All file handling lives in [`Store`], which receives its base paths as
//! arguments; the Tauri commands only resolve them. That is what makes it all
//! testable in a temporary directory, without an application.
//!
//! # Shipped effects are files too
//!
//! The effects Candeo ships are copied into the folder once, at startup, and are
//! from then on files: listed and run like any other. `settings.json` remembers
//! what was copied, to update an unchanged copy and never bring back one someone
//! removed — see [`Store::seed_shipped`]. The application does not delete, rename
//! or overwrite them, and brings them back on request — see
//! [`Store::restore_shipped`].

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::failure::Failure;
use crate::journal::LogLevel;
use crate::language::LanguageSetting;
use crate::runtime::swatch::{self, Swatch};
use crate::shipped::Shipped;
use crate::{AppState, CmdResult, DeviceRef};

/// Version of the effects API provided by this version of the application.
///
/// A module may declare the version it was written against (`apiVersion`,
/// 1 when absent): that is what allows cleanly refusing an effect written
/// against an API this version does not know, rather than letting it fail on
/// the first frame.
pub const EFFECTS_API_VERSION: u32 = 1;

/// Extension of an effect's source file.
const SOURCE_EXTENSION: &str = "ts";

/// Names reserved by Windows: a file with such a name is refused by the system,
/// in any directory and whatever its extension.
const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

// ---------------------------------------------------------------- exposed types

/// What an effect declares about itself, as the gallery and the tray need it.
///
/// For a file, `name` is the file name and the rest is what the module exports,
/// read by running it once — never written by hand.
///
/// camelCase like the other exposed types: `params` already holds JSON written on
/// the TypeScript side. A single snake_case field in the middle would only show
/// at run time.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub name: String,
    /// A string, or a map of languages: see [`text_or_empty`].
    #[serde(default = "empty_text")]
    pub description: serde_json::Value,
    /// Declared parameters, as the interface will present them.
    ///
    /// Kept as raw JSON: their shape is that of `ParamSpec` on the TypeScript
    /// side, it evolves with the editor, and the Rust side does not interpret
    /// them. Typing them here would create a second, unused source of truth.
    #[serde(default)]
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Version of the effects API the effect was written against.
    pub api_version: u32,
    /// The effect declares `inputs: ['keys']`: key presses are read while it
    /// runs, which the gallery shows.
    #[serde(default)]
    pub reads_keys: bool,
}

/// Kind of effect: shipped with the application, or the user's.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EffectKind {
    /// A file of the shipped folder, copied by the application and recorded as
    /// such, modified or not.
    Builtin,
    /// A file of the user's folder.
    User,
}

/// Whether an effect can run, as far as its cache says.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EffectState {
    /// Compiled for the file's current bytes: it can be previewed, applied and
    /// listed in the tray. Built-in effects are always ready.
    Ready,
    /// Never compiled, or changed since: the window compiles it at startup and
    /// on Refresh. Until then its manifest is only its name.
    Stale,
    /// Compiled for the file's current bytes, and it does not load: `error`
    /// says why. Not retried until the file changes.
    Broken,
}

/// Library entry: the manifest, plus what is not part of it.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EffectEntry {
    /// The effect's key, `<source>:<name>`: see [`EffectKey`].
    pub id: String,
    pub kind: EffectKind,
    pub state: EffectState,
    /// Why a [`EffectState::Broken`] effect does not load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// SHA-256 of the source file, to hand back to [`Store::cache_effect`]
    /// with its JavaScript. Absent for built-ins.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// A built-in whose file no longer has the hash recorded when it was copied:
    /// edited outside the application. Always false for the user's effects.
    pub modified: bool,
    /// Color swatch, **sampled by running the effect**.
    ///
    /// It is not in the manifest, and that is not a filing detail: the manifest
    /// is what the author declares, the swatch is what the effect does. Mixing
    /// them would reopen the door to a hand-written swatch, and so to a swatch
    /// that lies.
    ///
    /// Carried by the entry so that the list is enough to show it: a thumbnail
    /// needing a second call per effect would make as many round trips as the
    /// library has entries.
    ///
    /// Empty when it could not be computed — see [`crate::runtime::swatch`].
    /// The interface then falls back to a neutral dot.
    pub swatch: Swatch,
    #[serde(flatten)]
    pub manifest: Manifest,
}

/// Effect ids from the directory layout and the names they became, filled by
/// the migration at startup and read by the window for its drafts. See
/// [`Store::migrate_directories`].
#[derive(Default)]
pub struct MigratedEffects(pub Mutex<BTreeMap<String, String>>);

/// What [`Store::seed_shipped`] did, for the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Seeding {
    /// Shipped effects copied for the first time.
    pub copied: usize,
    /// Unchanged copies replaced by a newer shipped version.
    pub updated: usize,
}

/// Brightness of a keyboard that was just plugged in: full.
///
/// It is the least surprising default, and it is also the value the file
/// **does not write** — see [`DeviceRecord::brightness`].
pub const DEFAULT_BRIGHTNESS: u8 = 255;

/// Decision made for a device, once, and kept.
///
/// The default is [`Detected`](DeviceState::Detected): **a device never seen
/// before is not controlled**. Writing to a USB device we barely understand is
/// not harmless, and for a catalog that grows — keyboards, mice, memory, fans —
/// adopting by default is how you break someone's hardware.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeviceState {
    /// Listed, but **not** opened. The state of every device nobody has made a
    /// decision about yet.
    #[default]
    Detected,
    /// Opened automatically at startup, without asking anything.
    Adopted,
    /// Left alone, and stays that way.
    Ignored,
}

/// What `settings.json` keeps about a device: its identity, and the decision.
///
/// # Identity is VID / PID / serial number
///
/// **Neither the variant nor the firmware.** The same keyboard reported itself as
/// `v1.4 / Unkown Variant` then `v1.5 / Quartz` during the protocol survey: a
/// binding that matches on those fields breaks on update, and the adopted device
/// becomes a stranger overnight.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub vid: u16,
    pub pid: u16,
    /// Serial number, when the system reports one.
    ///
    /// Absent from the file rather than `null`: most entries will not have one,
    /// and a repeated empty key tells nothing to whoever rereads their settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub state: DeviceState,
    /// Brightness kept for **this** device.
    ///
    /// # Why here rather than at the root
    ///
    /// It already is everywhere else: `Keyboard::set_brightness` is a device
    /// command (`0x0f`/`0x04`), separate from the running effect, and
    /// `set_brightness(device, level)` has taken a [`DeviceRef`] since day one.
    /// The global scalar in `Settings` was the only place that said otherwise —
    /// and two keyboards have no reason to share a level.
    ///
    /// # `None` means "the default", not "off"
    ///
    /// The file then holds nothing at all: writing [`DEFAULT_BRIGHTNESS`] for
    /// every device merely plugged in would grow it with entries that decide
    /// nothing. Same economy as `devices` and `effectParams` — an entry exists
    /// only if someone moved something. It is also why an entry back to
    /// `detected` **without** brightness disappears: see [`Self::is_inert`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
}

impl DeviceRecord {
    /// True if this entry no longer keeps any decision.
    ///
    /// `detected` without brightness says exactly what the **absence** of an
    /// entry says. Keeping it would tell nothing to whoever rereads their
    /// settings, and would grow the file by one line per device touched once.
    fn is_inert(&self) -> bool {
        self.state == DeviceState::Detected && self.brightness.is_none()
    }

    /// True if this entry designates the enumerated device.
    ///
    /// VID and PID must match; the serial is compared only if **both sides**
    /// carry one. This is not laxity, it is the only rule that holds both ways:
    ///
    /// - the serial tells apart two units of the same model — without it,
    ///   adopting one would adopt the other;
    /// - but a silent enumeration — hidraw without a udev rule, a hub that
    ///   passes nothing on — must not unmatch an already adopted device,
    ///   otherwise the decision would have to be made again on every plug-in.
    pub fn matches(&self, vid: u16, pid: u16, serial: Option<&str>) -> bool {
        if self.vid != vid || self.pid != pid {
            return false;
        }
        match (self.serial.as_deref(), serial) {
            (Some(ours), Some(theirs)) => ours == theirs,
            _ => true,
        }
    }
}

/// The effect **applied** on a device, the one driving its LEDs.
///
/// # A list, not a scalar
///
/// The field before it — `activeEffect: Option<String>` — described **one**
/// active effect, while the engine has run one per device since issue #26. No
/// value could make that field right: its shape was wrong. One entry per
/// device, absent until something has been applied, is the only one that
/// describes what the engine actually does.
///
/// # It is not what is being previewed
///
/// **Preview never writes here.** Previewing an effect does not keep it: "Apply"
/// decides, and it is the action that sends to the keyboard. See
/// [`crate::runtime::start_preview`], which has no access to the disk.
///
/// # The key is the [`DeviceRef`], without serial number
///
/// Same reason as [`EffectParamsRecord`]: the engine loops are indexed by
/// VID/PID, two units of the same model share one, and telling them apart here
/// would promise a separation the engine does not keep.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActiveEffectRecord {
    pub vid: u16,
    pub pid: u16,
    /// Id of the applied effect.
    pub effect: String,
}

/// An effect's parameters, kept for **one** device.
///
/// # Why device and effect together
///
/// "The wave, but slower" is tuned on a given keyboard: the same effect has no
/// reason to run at the same speed on two devices, and two effects on the same
/// device do not have the same parameters. The key is therefore the pair, and
/// switching effects then coming back finds its parameters again.
///
/// # Without the serial number, unlike [`DeviceRecord`]
///
/// Deliberate: every engine command targets a [`DeviceRef`], that is a VID and a
/// PID. Two units of the same model already share their render loop — telling
/// them apart *here* would promise a separation the rest of the application does
/// not keep, and the setting would seem lost one time out of two. Adoption, on
/// the other hand, decides to open a specific device: it needs the serial, and
/// that is why it carries it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EffectParamsRecord {
    pub vid: u16,
    pub pid: u16,
    /// Id of the tuned effect.
    pub effect: String,
    /// The values, as the interface sends them to the engine.
    ///
    /// Raw JSON, like [`Manifest::params`]: their shape is that of `ParamValue`
    /// on the TypeScript side — a number, a string, a boolean or a `{r,g,b}`
    /// color — and the Rust side does not interpret them. Typing them here would
    /// create a second source of truth, which would diverge at the first
    /// parameter type added.
    pub values: serde_json::Map<String, serde_json::Value>,
}

/// What applies to the whole application, and to no device in particular.
///
/// A separate object rather than fields at the root: this layout is what
/// prevents the confusion this module just came out of. Anything that depends on
/// a keyboard lives in an indexed list; what does not lives here, and the
/// language — when it arrives — will have nothing to decide.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    /// Log level, when someone changed it from the application.
    ///
    /// `None` — and so absent from the file — means "the default", not "no
    /// log": writing the default would suggest a decision where there was none,
    /// and would freeze along the way a choice the next version might want to
    /// revisit.
    ///
    /// **It survives a restart**, and that is a trade-off: automatically going
    /// back to the default would protect against a full disk, persistence serves
    /// whoever is tracking a bug **at startup** — device adoption is one — who
    /// cannot be asked to raise the level after the fact. The price is paid by
    /// the notice the interface shows about it. See [`crate::journal`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    /// Interface language. `system`, the default, is not written.
    #[serde(skip_serializing_if = "LanguageSetting::is_system")]
    pub language: LanguageSetting,
    /// A device that opens starts its applied effect again. On by default, and
    /// written only when turned off. See [`crate::runtime::resume_applied`].
    #[serde(skip_serializing_if = "is_true")]
    pub resume_effects: bool,
    /// Log files kept, one per day; `0` keeps them all. Written only when it differs
    /// from [`crate::journal::DEFAULT_FILES_KEPT`].
    #[serde(skip_serializing_if = "is_default_files_kept")]
    pub log_files_kept: u32,
    /// Light or dark interface. `system`, the default, is not written.
    #[serde(skip_serializing_if = "Theme::is_system")]
    pub theme: Theme,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            log_level: None,
            language: LanguageSetting::default(),
            resume_effects: true,
            log_files_kept: crate::journal::DEFAULT_FILES_KEPT,
            theme: Theme::default(),
        }
    }
}

/// The interface theme. Only the window reads it: it sets `data-theme` on the
/// document, and `system` leaves the system's setting to decide.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    fn is_system(&self) -> bool {
        *self == Self::System
    }
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_default_files_kept(value: &u32) -> bool {
    *value == crate::journal::DEFAULT_FILES_KEPT
}

/// Persistent settings.
///
/// `#[serde(default)]` on the whole struct: a `settings.json` written by an
/// earlier version, missing a field added since, reloads without error instead
/// of leaving the application silent at startup.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Shape of the file, for migrations that cannot be told from the content.
    ///
    /// A file without it predates the field: it reads as 0, not as
    /// [`SETTINGS_VERSION`], which only the settings of a first launch get. See
    /// [`Store::migrate_directories`].
    #[serde(default)]
    pub version: u32,
    /// What depends on no device. See [`Preferences`].
    pub preferences: Preferences,
    /// Decisions made device by device, brightness included.
    ///
    /// Holds only those that **differ from the default**: a device absent from
    /// this list is `detected` and at full brightness, which is exactly the state
    /// of a device never encountered. The file therefore does not grow by one
    /// entry for each device plugged in once.
    pub devices: Vec<DeviceRecord>,
    /// The effect **applied** on each device. See [`ActiveEffectRecord`].
    ///
    /// Same economy as the rest: no entry until something has been applied, and
    /// the entry goes when the effect stops or is deleted.
    pub active_effects: Vec<ActiveEffectRecord>,
    /// Effect parameters kept, per device and per effect.
    ///
    /// Same economy as `devices`: an entry exists only if someone **moved** a
    /// slider. Restoring the declared values removes it, rather than writing a
    /// copy of the defaults that the next version of the effect would
    /// contradict.
    pub effect_params: Vec<EffectParamsRecord>,
    /// The shipped effects copied into the folder, by name: the hash of the
    /// version copied, or `None` when a file of that name was already there and
    /// was left alone. See [`Store::seed_shipped`].
    ///
    /// An entry stays when its file is deleted or renamed: it is what keeps a
    /// shipped effect someone removed from coming back at the next launch.
    pub shipped_effects: BTreeMap<String, Option<String>>,
    /// The log level as an earlier version wrote it, **at the root**.
    ///
    /// Read, never written back (`skip_serializing`): [`Store::read_settings`]
    /// moves it into [`Preferences`], and it disappears from the file on the first
    /// write. Without this bridge, moving `logLevel` would have reset to the
    /// default the level of whoever was **in the middle of** chasing a failure —
    /// that is, at the worst moment, since it is the only one where this setting
    /// matters.
    ///
    /// The rejected alternative: do nothing and accept it. It cost twelve fewer
    /// lines and one lost debugging run. To remove once no `settings.json` older
    /// than v2.1 is around any more.
    #[serde(default, rename = "logLevel", skip_serializing)]
    legacy_log_level: Option<LogLevel>,
}

/// Current shape of `settings.json`.
///
/// - 0: effects referenced by their directory id;
/// - 1: user effects referenced by their name, built-in effects by their id;
/// - 2: every effect referenced by its name;
/// - 3: every effect referenced by its key, `<source>:<name>`.
pub const SETTINGS_VERSION: u32 = 3;

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            preferences: Preferences::default(),
            devices: Vec::new(),
            active_effects: Vec::new(),
            effect_params: Vec::new(),
            shipped_effects: BTreeMap::new(),
            legacy_log_level: None,
        }
    }
}

impl Settings {
    /// Moves into [`Preferences`] what an earlier file carried at the root.
    ///
    /// What is already in place wins: a file written by this version is right
    /// against a legacy key a text editor may have left in it.
    fn absorb_legacy(&mut self) {
        if let Some(legacy_level) = self.legacy_log_level.take() {
            self.preferences.log_level.get_or_insert(legacy_level);
        }
    }

    /// Index of the entry describing this device, if there is one.
    ///
    /// Exact identity first — serial included, `None` included — then the
    /// tolerant rule of [`DeviceRecord::matches`]. The order matters: an entry
    /// without a serial must not decide in place of one that carries a serial,
    /// otherwise two units of the same model would be mixed up as soon as one of
    /// them had been adopted without a serial.
    fn position(&self, vid: u16, pid: u16, serial: Option<&str>) -> Option<usize> {
        self.devices
            .iter()
            .position(|r| r.vid == vid && r.pid == pid && r.serial.as_deref() == serial)
            .or_else(|| {
                self.devices
                    .iter()
                    .position(|r| r.matches(vid, pid, serial))
            })
    }

    /// Decision kept for this device, or [`DeviceState::Detected`].
    pub fn device_state(&self, vid: u16, pid: u16, serial: Option<&str>) -> DeviceState {
        self.position(vid, pid, serial)
            .map(|i| self.devices[i].state)
            .unwrap_or_default()
    }

    /// Keeps a decision for this device.
    pub fn set_device_state(
        &mut self,
        vid: u16,
        pid: u16,
        serial: Option<&str>,
        state: DeviceState,
    ) {
        match self.position(vid, pid, serial) {
            Some(i) => {
                let record = &mut self.devices[i];
                record.state = state;
                // The serial is filled in if we just learned it, but never
                // erased: a silent enumeration must not make the entry lose
                // what tells it apart from the unit next to it.
                if record.serial.is_none() {
                    record.serial = serial.map(str::to_owned);
                }
            }
            None => self.devices.push(DeviceRecord {
                vid,
                pid,
                serial: serial.map(str::to_owned),
                state,
                brightness: None,
            }),
        }
        self.prune();
    }

    /// The brightness kept for this device, or [`DEFAULT_BRIGHTNESS`].
    pub fn brightness(&self, vid: u16, pid: u16, serial: Option<&str>) -> u8 {
        self.position(vid, pid, serial)
            .and_then(|i| self.devices[i].brightness)
            .unwrap_or(DEFAULT_BRIGHTNESS)
    }

    /// Keeps a brightness. [`DEFAULT_BRIGHTNESS`] **forgets** the entry.
    ///
    /// The counterpart of "restore the declared values" for effect parameters:
    /// pushing the slider all the way up must not write 255 to the file, it must
    /// remove the line. A device for which this was the only decision then
    /// disappears entirely — see [`DeviceRecord::is_inert`].
    ///
    /// Returns true if something changed, so the temporary file and its rename
    /// are skipped when there is nothing to write.
    pub fn set_brightness(&mut self, vid: u16, pid: u16, serial: Option<&str>, level: u8) -> bool {
        let kept = (level != DEFAULT_BRIGHTNESS).then_some(level);
        match self.position(vid, pid, serial) {
            Some(i) => {
                if self.devices[i].brightness == kept {
                    return false;
                }
                self.devices[i].brightness = kept;
                // Same rule as [`Self::set_device_state`]: the serial is filled
                // in if we just learned it, it is never erased.
                if self.devices[i].serial.is_none() {
                    self.devices[i].serial = serial.map(str::to_owned);
                }
            }
            None => {
                // The default, on a device we keep nothing about: there is no
                // entry to create just to put nothing in it.
                let Some(level) = kept else { return false };
                self.devices.push(DeviceRecord {
                    vid,
                    pid,
                    serial: serial.map(str::to_owned),
                    state: DeviceState::default(),
                    brightness: Some(level),
                });
            }
        }
        self.prune();
        true
    }

    /// Removes device entries that no longer keep anything.
    fn prune(&mut self) {
        self.devices.retain(|r| !r.is_inert());
    }

    /// The effect applied on this device, if there is one.
    pub fn active_effect(&self, vid: u16, pid: u16) -> Option<&str> {
        self.active_effects
            .iter()
            .find(|r| r.vid == vid && r.pid == pid)
            .map(|r| r.effect.as_str())
    }

    /// Keeps the applied effect, or forgets it with `None`.
    ///
    /// Returns true if something changed: starting the same effect twice on the
    /// same keyboard — which is what a double-click does — must not rewrite the
    /// file.
    pub fn set_active_effect(&mut self, vid: u16, pid: u16, effect: Option<&str>) -> bool {
        let position = self
            .active_effects
            .iter()
            .position(|r| r.vid == vid && r.pid == pid);

        match (position, effect) {
            (Some(i), None) => {
                self.active_effects.remove(i);
                true
            }
            (Some(i), Some(e)) if self.active_effects[i].effect == e => false,
            (Some(i), Some(e)) => {
                self.active_effects[i].effect = e.to_owned();
                true
            }
            (None, None) => false,
            (None, Some(e)) => {
                self.active_effects.push(ActiveEffectRecord {
                    vid,
                    pid,
                    effect: e.to_owned(),
                });
                true
            }
        }
    }

    /// The parameters kept for this effect on this device, if any.
    pub fn effect_params(
        &self,
        vid: u16,
        pid: u16,
        effect: &str,
    ) -> Option<&serde_json::Map<String, serde_json::Value>> {
        self.effect_params
            .iter()
            .find(|r| r.vid == vid && r.pid == pid && r.effect == effect)
            .map(|r| &r.values)
    }

    /// Keeps parameters. An **empty** map erases the entry.
    ///
    /// That is what makes "restore the declared values" a forget and not a copy:
    /// the effect then starts again from its manifest, including when a later
    /// version changes its defaults.
    pub fn set_effect_params(
        &mut self,
        vid: u16,
        pid: u16,
        effect: &str,
        values: serde_json::Map<String, serde_json::Value>,
    ) {
        let position = self
            .effect_params
            .iter()
            .position(|r| r.vid == vid && r.pid == pid && r.effect == effect);

        match (position, values.is_empty()) {
            (Some(i), true) => {
                self.effect_params.remove(i);
            }
            (Some(i), false) => self.effect_params[i].values = values,
            (None, true) => {}
            (None, false) => self.effect_params.push(EffectParamsRecord {
                vid,
                pid,
                effect: effect.to_owned(),
                values,
            }),
        }
    }

    /// Forgets an effect **everywhere**: its parameters, and where it is applied.
    ///
    /// Called when the effect is deleted. Without it, its parameters would stay
    /// in `settings.json` for an id nothing designates any more, and the file
    /// would only grow.
    ///
    /// # Why where it is applied goes too, and not only the parameters
    ///
    /// This is the trap issue #48 had pointed out and that #64 settles here:
    /// `delete_effect` left a **dangling id**. As long as the field was dead, the
    /// question did not arise; now that `activeEffects` is written, it does, and
    /// the two possible answers do not cost the same:
    ///
    /// - **purge on delete** — chosen: the file never holds an id the library
    ///   does not know, and that invariant can be checked without running
    ///   anything;
    /// - silently fall back at startup — rejected *as the only measure*: a silent
    ///   failure at launch is exactly the kind of breakage that costs a debugging
    ///   run, and the id would survive as many startups as you like.
    ///
    /// The fallback is still needed as a **second** barrier — an effect directory
    /// removed by hand does not go through here — but it is no longer the only
    /// one.
    ///
    /// Returns true if something was removed, so the file is not rewritten when
    /// there is nothing to change in it.
    pub fn forget_effect(&mut self, effect: &str) -> bool {
        let before = self.effect_params.len() + self.active_effects.len();
        self.effect_params.retain(|r| r.effect != effect);
        self.active_effects.retain(|r| r.effect != effect);
        self.effect_params.len() + self.active_effects.len() != before
    }

    /// Moves every reference to an effect to its new name.
    ///
    /// Returns true if something changed, so the file is not rewritten when there
    /// is nothing to change in it.
    pub fn rename_effect(&mut self, from: &str, to: &str) -> bool {
        let renames = BTreeMap::from([(from.to_owned(), to.to_owned())]);
        let before = self.clone();
        self.rename_effects(&renames);
        *self != before
    }

    /// Every effect this file refers to, once each.
    fn effect_references(&self) -> Vec<String> {
        let references: BTreeSet<&str> = self
            .active_effects
            .iter()
            .map(|r| r.effect.as_str())
            .chain(self.effect_params.iter().map(|r| r.effect.as_str()))
            .collect();
        references.into_iter().map(str::to_owned).collect()
    }

    /// Rewrites every effect reference found in `renames`.
    ///
    /// A reference that is not in the table stays as it is: it names an effect
    /// that is not there, which the startup fallback already copes with, and
    /// guessing what it meant would be worse.
    fn rename_effects(&mut self, renames: &BTreeMap<String, String>) {
        for effect in self
            .active_effects
            .iter_mut()
            .map(|r| &mut r.effect)
            .chain(self.effect_params.iter_mut().map(|r| &mut r.effect))
        {
            if let Some(name) = renames.get(effect.as_str()) {
                effect.clone_from(name);
            }
        }
    }
}

/// The values an effect must start with: what it **declares**, overridden by
/// what was **kept** for this device.
///
/// # Why this computation exists in Rust
///
/// The window already does it, in two pieces — `startingParams` for the manifest
/// defaults, `merge` for the override. But the tray icon starts an effect **with
/// no window**: it cannot borrow anything from the TypeScript, and starting an
/// effect with an empty parameter object would not give the same lighting as the
/// same click made from the gallery. See [`crate::tray`].
///
/// Both implementations of the rule must therefore stay in agreement. What they
/// say, and it is the only thing to remember: **limited to declared
/// parameters**. A value kept for a parameter the effect no longer has
/// disappears on its own, instead of travelling indefinitely to a loop that no
/// longer reads it — and a declared parameter with no kept value takes its
/// default, never nothing.
///
/// A manifest parameter that declares no `default` is left out: `params` is raw
/// JSON the Rust side does not interpret (see [`Manifest::params`]), and
/// inventing a value for a kind of parameter we do not know would be worse than
/// letting the effect apply its own.
pub(crate) fn starting_params(
    manifest: &Manifest,
    stored: Option<&serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut out = serde_json::Map::new();
    for (id, spec) in &manifest.params {
        if let Some(default_value) = spec.get("default") {
            out.insert(id.clone(), default_value.clone());
        }
    }
    if let Some(stored) = stored {
        for (id, value) in stored {
            // Only what the manifest still declares: the same limit as on the
            // window side, and it is what makes an orphaned value disappear.
            if manifest.params.contains_key(id) {
                out.insert(id.clone(), value.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- names

/// Longest effect name, in characters.
const MAX_NAME_LEN: usize = 64;

/// Characters no Windows file name may hold. Refused on every system, so that a
/// folder copied between machines means the same thing.
const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// True if Windows reserves this name as a device: `CON`, and `CON.txt` alike.
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end();
    RESERVED_NAMES.contains(&stem.to_ascii_lowercase().as_str())
}

/// Checks that `name` can be an effect's name, and so its file name.
///
/// The rules are those of Windows, applied everywhere. `:` is among the refused
/// characters, which is what lets the tray menu use it as a separator.
pub(crate) fn validate_name(name: &str) -> CmdResult<()> {
    if name.is_empty() {
        return Err(Failure::new("nameEmpty"));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(Failure::new("nameTooLong").with("max", MAX_NAME_LEN));
    }
    if name
        .chars()
        .any(|c| c.is_control() || FORBIDDEN_CHARS.contains(&c))
    {
        let characters: Vec<String> = FORBIDDEN_CHARS.iter().map(char::to_string).collect();
        return Err(Failure::new("nameForbiddenCharacter")
            .with("name", name)
            .with("characters", characters.join(" ")));
    }
    if name.starts_with([' ', '.']) || name.ends_with([' ', '.']) {
        return Err(Failure::new("nameEdges").with("name", name));
    }
    if is_reserved(name) {
        return Err(Failure::new("nameReserved").with("name", name));
    }
    Ok(())
}

/// Where an effect's file lives. See `docs/design/effects-sources.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// `app_data_dir()/effects/`: the effects Candeo ships, seeded and updated
    /// by it.
    Shipped,
    /// `Documents/candeo/effects/`: the effects people write, duplicate or drop
    /// in. Outside `AppData`, so a packaged build neither hides nor removes them.
    User,
}

impl Source {
    const ALL: [Source; 2] = [Source::Shipped, Source::User];

    fn prefix(self) -> &'static str {
        match self {
            Self::Shipped => "shipped",
            Self::User => "user",
        }
    }
}

/// An effect's key, written `<source>:<name>`: `shipped:Breathing`, `user:Rain`.
///
/// What refers to an effect holds it: `settings.json`, the engine, the tray menu,
/// editor drafts. Relative to its source's folder, never an absolute path: a
/// path carries the user name into logs and settings, and breaks when the folder
/// moves. Unambiguous, since names exclude `:`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EffectKey {
    pub source: Source,
    pub name: String,
}

impl EffectKey {
    pub fn shipped(name: &str) -> Self {
        Self {
            source: Source::Shipped,
            name: name.to_owned(),
        }
    }

    pub fn user(name: &str) -> Self {
        Self {
            source: Source::User,
            name: name.to_owned(),
        }
    }

    /// The key a string holds, refused like a name that designates nothing.
    pub fn parse(key: &str) -> CmdResult<Self> {
        let not_found = || Failure::new("effectNotFound").with("name", key);
        let (prefix, name) = key.split_once(':').ok_or_else(not_found)?;
        let source = Source::ALL
            .into_iter()
            .find(|s| s.prefix() == prefix)
            .ok_or_else(not_found)?;
        validate_name(name)?;
        Ok(Self {
            source,
            name: name.to_owned(),
        })
    }
}

impl std::fmt::Display for EffectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source.prefix(), self.name)
    }
}

/// A valid name made out of any text: forbidden characters become `-`, the ends
/// are trimmed, the length is capped.
///
/// Only for the migration of the directory layout, whose names were free text.
/// A name typed in the application is refused with a reason instead: silently
/// changing what someone just typed would be worse than saying no.
fn sanitize_name(wanted: &str) -> String {
    let replaced: String = wanted
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if FORBIDDEN_CHARS.contains(&c) { '-' } else { c })
        .collect();
    let trimmed: String = replaced
        .trim_matches([' ', '.'])
        .chars()
        .take(MAX_NAME_LEN - " effect".len())
        .collect();
    let name = trimmed.trim_end_matches([' ', '.']);

    if name.is_empty() {
        "Effect".into()
    } else if is_reserved(name) {
        format!("{name} effect")
    } else {
        name.to_owned()
    }
}

/// `base`, or `base (2)`, `base (3)`… — the first one `taken` does not hold.
///
/// `taken` holds lowercased names: two names differing only by case are the
/// same file on Windows.
fn free_name(base: &str, taken: &BTreeSet<String>) -> String {
    if !taken.contains(&base.to_lowercase()) {
        return base.to_owned();
    }
    (2..)
        .map(|n| {
            let suffix = format!(" ({n})");
            let stem: String = base
                .chars()
                .take(MAX_NAME_LEN - suffix.chars().count())
                .collect();
            format!("{}{suffix}", stem.trim_end_matches([' ', '.']))
        })
        .find(|candidate| !taken.contains(&candidate.to_lowercase()))
        .expect("an unbounded range always has a free name")
}

/// SHA-256 of a source, in lowercase hexadecimal.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------- storage

/// What compiling an effect produced, for one version of its file.
///
/// Written to the cache folder, next to nothing the user edits: it can be deleted
/// at any time and is rebuilt at the next Refresh.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
struct CacheRecord {
    /// SHA-256 of the source it was compiled from. A record whose hash differs
    /// from the file's describes another version, and is ignored.
    hash: String,
    js: String,
    #[serde(default = "empty_text")]
    description: serde_json::Value,
    #[serde(default)]
    params: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    api_version: u32,
    #[serde(default)]
    reads_keys: bool,
    #[serde(default)]
    swatch: Swatch,
    /// Why the module does not load, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Disk access to effects, their cache and settings.
///
/// The base paths come from outside: nothing here knows about Tauri, which makes
/// the whole thing testable in a temporary directory.
pub struct Store {
    shipped_dir: PathBuf,
    user_dir: PathBuf,
    cache_dir: PathBuf,
    settings_file: PathBuf,
}

impl Store {
    /// `data_dir` holds the shipped effects, `user_dir` is the user's effects
    /// folder itself, `config_dir` holds the configuration, `cache_dir` what can
    /// be rebuilt.
    pub fn new(data_dir: &Path, user_dir: &Path, config_dir: &Path, cache_dir: &Path) -> Self {
        Self {
            shipped_dir: data_dir.join("effects"),
            user_dir: user_dir.to_owned(),
            cache_dir: cache_dir.join("effects"),
            settings_file: config_dir.join("settings.json"),
        }
    }

    fn dir(&self, source: Source) -> &Path {
        match source {
            Source::Shipped => &self.shipped_dir,
            Source::User => &self.user_dir,
        }
    }

    fn source_path(&self, key: &EffectKey) -> PathBuf {
        self.dir(key.source)
            .join(format!("{}.{SOURCE_EXTENSION}", key.name))
    }

    /// One cache tree per source: a shipped and a user effect may share a name.
    fn cache_path(&self, key: &EffectKey) -> PathBuf {
        self.cache_dir
            .join(key.source.prefix())
            .join(format!("{}.json", key.name))
    }

    /// The effect names in a source's folder, in a stable order.
    ///
    /// A file whose name the application would refuse is skipped rather than
    /// failing the whole list: a library of twenty effects must not disappear
    /// because of one foreign file.
    fn names(&self, source: Source) -> CmdResult<Vec<String>> {
        let dir = self.dir(source);
        // Missing folder: first launch, no effect written. This is not an error.
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Failure::unexpected(format!(
                    "cannot read {}: {e}",
                    crate::paths::shown(dir)
                )))
            }
        };

        let mut names = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|e| Failure::unexpected(format!("library listing interrupted: {e}")))?;
            let path = entry.path();
            let is_source = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == SOURCE_EXTENSION);
            if !is_source || !path.is_file() {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if validate_name(name).is_err() {
                continue;
            }
            names.push(name.to_owned());
        }

        // Stable order: the file system guarantees none, and a gallery that
        // reorders itself on every opening is unreadable.
        names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b)));
        // Two names differing only by case can sit side by side on Linux. They
        // would be one file on Windows, and one library key is all the rest of
        // the application can tell apart: the first stays.
        names.dedup_by(|next, kept| next.to_lowercase() == kept.to_lowercase());
        Ok(names)
    }

    /// The name as it is on disk in a source's folder, for a name given in any
    /// case.
    fn existing(&self, source: Source, name: &str) -> CmdResult<Option<String>> {
        let wanted = name.to_lowercase();
        Ok(self
            .names(source)?
            .into_iter()
            .find(|n| n.to_lowercase() == wanted))
    }

    /// Fails unless an effect has exactly this key.
    fn require(&self, key: &EffectKey) -> CmdResult<()> {
        validate_name(&key.name)?;
        match self.existing(key.source, &key.name)? {
            Some(existing) if existing == key.name => Ok(()),
            _ => Err(Failure::new("effectNotFound").with("name", &key.name)),
        }
    }

    fn read_cache(&self, key: &EffectKey) -> Option<CacheRecord> {
        fs::read_to_string(self.cache_path(key))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
    }

    /// The library: every file of both sources, with its state.
    ///
    /// Listing reads files and hashes them, and runs nothing: whatever has to run
    /// happened in [`Self::cache_effect`].
    pub fn list_effects(&self) -> CmdResult<Vec<EffectEntry>> {
        let shipped = self.shipped_records();
        let mut effects = Vec::new();
        for source in Source::ALL {
            for name in self.names(source)? {
                // A file Candeo did not put in the shipped folder is not a
                // built-in: the next startup moves it to the user's folder
                // ([`Self::migrate_sources`]).
                if source == Source::Shipped && !matches!(shipped.get(&name), Some(Some(_))) {
                    continue;
                }
                let key = EffectKey { source, name };
                // A file that cannot be read right now — locked by an editor,
                // say — is skipped for this listing rather than failing all of it.
                let Ok(bytes) = fs::read(self.source_path(&key)) else {
                    continue;
                };
                let hash = sha256_hex(&bytes);
                let record = self.read_cache(&key).filter(|r| r.hash == hash);
                effects.push(library_entry(key, hash, record, &shipped));
            }
        }
        Ok(effects)
    }

    /// An effect's source, to open it in the editor.
    pub fn effect_source(&self, key: &EffectKey) -> CmdResult<String> {
        self.require(key)?;
        read(&self.source_path(key))
    }

    /// Writes an effect's source and returns its hash.
    ///
    /// `create` says which of the two gestures this is, so that neither can do
    /// the other's job by accident: creating never overwrites an effect that
    /// exists under that name in any case, and saving again never creates one.
    ///
    /// Written through a temporary file and a rename: a Refresh reading the
    /// folder at that instant must never compile half a file.
    ///
    /// Only the user's effects are written: a built-in is not saved over, see
    /// [`Store::restore_shipped`] for why, and nothing is created among them.
    pub fn save_effect_source(
        &self,
        key: &EffectKey,
        source: &str,
        create: bool,
    ) -> CmdResult<String> {
        validate_name(&key.name)?;
        if key.source == Source::Shipped {
            return Err(Failure::new("builtinNotSaved").with("name", &key.name));
        }
        match (self.existing(Source::User, &key.name)?, create) {
            (Some(existing), true) => {
                return Err(Failure::new("effectExists").with("name", existing));
            }
            (Some(existing), false) if existing == key.name => {}
            (_, false) => return Err(Failure::new("effectNotFound").with("name", &key.name)),
            (None, true) => {}
        }
        create_dir(&self.user_dir)?;
        write_atomically(&self.source_path(key), source)?;
        Ok(sha256_hex(source.as_bytes()))
    }

    /// Records the JavaScript the window compiled for this version of the file.
    ///
    /// The module is loaded once, under the time budget swatch sampling uses, to
    /// read what it declares; a module that does not load is recorded **with its
    /// error**, so that it is not retried until the file changes.
    ///
    /// Refused when the file no longer has `hash`: it changed while the window
    /// was compiling, and recording would pair this JavaScript with other code.
    pub fn cache_effect(&self, key: &EffectKey, hash: &str, js: &str) -> CmdResult<EffectEntry> {
        self.require(key)?;
        let current = sha256_hex(
            &fs::read(self.source_path(key))
                .map_err(|e| Failure::unexpected(format!("cannot read “{key}”: {e}")))?,
        );
        if current != hash {
            return Err(Failure::new("effectChanged").with("name", &key.name));
        }

        let record = compile_record(hash, js);
        let path = self.cache_path(key);
        create_dir(path.parent().unwrap_or(&self.cache_dir))?;
        let json = serde_json::to_string(&record)
            .map_err(|e| Failure::unexpected(format!("cache record not serialisable: {e}")))?;
        write_atomically(&path, &json)?;
        Ok(library_entry(
            key.clone(),
            hash.to_owned(),
            Some(record),
            &self.shipped_records(),
        ))
    }

    /// An effect's executable JavaScript: what the engine loads.
    ///
    /// For a file, only the JavaScript compiled from its **current** bytes: the
    /// engine and the tray never run code that differs from what is on disk.
    pub fn effect_js(&self, key: &EffectKey) -> CmdResult<String> {
        self.require(key)?;
        let hash = sha256_hex(
            &fs::read(self.source_path(key))
                .map_err(|e| Failure::unexpected(format!("cannot read “{key}”: {e}")))?,
        );
        match self.read_cache(key).filter(|r| r.hash == hash) {
            Some(CacheRecord {
                error: Some(error), ..
            }) => Err(Failure::new("effectBroken")
                .with("name", &key.name)
                .with("error", error)),
            Some(record) => Ok(record.js),
            None => Err(Failure::new("effectNotCompiled").with("name", &key.name)),
        }
    }

    /// Renames one of the user's effects, and its cache with it, and returns its
    /// new key.
    ///
    /// The references in `settings.json` and in the engine are the command's
    /// business: see [`rename_effect`].
    pub fn rename_effect(&self, from: &EffectKey, to: &str) -> CmdResult<EffectKey> {
        self.require(from)?;
        if from.source == Source::Shipped {
            return Err(Failure::new("builtinNotRenamed").with("name", &from.name));
        }
        validate_name(to)?;
        if let Some(existing) = self.existing(Source::User, to)? {
            // The same file under another case is a rename too: `Onde` → `onde`.
            if existing != from.name {
                return Err(Failure::new("effectExists").with("name", existing));
            }
        }
        let target = EffectKey::user(to);
        if *from == target {
            return Ok(target);
        }
        let (source, path) = (self.source_path(from), self.source_path(&target));
        fs::rename(&source, &path).map_err(|e| {
            Failure::unexpected(format!(
                "cannot rename {}: {e}",
                crate::paths::shown(&source)
            ))
        })?;
        // A cache that does not follow only costs a compilation at the next
        // Refresh: not a reason to undo a rename that succeeded.
        let _ = fs::rename(self.cache_path(from), self.cache_path(&target));
        Ok(target)
    }

    /// Tells whether this effect can be deleted, **without deleting anything**.
    ///
    /// Separate from [`Self::delete_effect`] because the command stops the loops
    /// **between** the refusal and the erasure: refusing afterwards would make a
    /// running effect pay for a stop it was not owed.
    pub fn check_deletable(&self, key: &EffectKey) -> CmdResult<()> {
        self.require(key)?;
        if key.source == Source::Shipped {
            return Err(Failure::new("builtinNotDeleted").with("name", &key.name));
        }
        Ok(())
    }

    /// What `settings.json` records about shipped effects. Unreadable settings
    /// only lose the built-in mark, not the library.
    fn shipped_records(&self) -> BTreeMap<String, Option<String>> {
        self.read_settings()
            .map(|s| s.shipped_effects)
            .unwrap_or_default()
    }

    /// Copies an effect into the user's folder and returns the copy's key: its
    /// own name when the user's folder has none of that name — a built-in's
    /// first copy keeps its name, under `user:` — then `<name> (2)`, `(3)`…
    ///
    /// Numbered like every other name made free ([`free_name`]), rather than a
    /// word in the interface language: a file name does not change with the
    /// language, and a folder shared between two languages stays consistent.
    ///
    /// The cache is copied too: same bytes, same hash, so the copy is ready
    /// without a compilation. A copy of a built-in is the user's.
    pub fn duplicate_effect(&self, key: &EffectKey) -> CmdResult<EffectKey> {
        self.require(key)?;
        let source = read(&self.source_path(key))?;
        let taken: BTreeSet<String> = self
            .names(Source::User)?
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        let copy = EffectKey::user(&free_name(&key.name, &taken));

        create_dir(&self.user_dir)?;
        write_atomically(&self.source_path(&copy), &source)?;
        if let Ok(record) = fs::read_to_string(self.cache_path(key)) {
            // A cache that does not follow only costs a compilation at the next
            // Refresh.
            let path = self.cache_path(&copy);
            let _ = create_dir(path.parent().unwrap_or(&self.cache_dir))
                .and_then(|()| write_atomically(&path, &record));
        }
        Ok(copy)
    }

    /// The user's effects folder, created if it does not exist yet: opening it
    /// must work on a first launch too, since saving a file there is how an
    /// effect is added.
    pub fn user_effects_dir(&self) -> CmdResult<&Path> {
        create_dir(&self.user_dir)?;
        Ok(&self.user_dir)
    }

    /// Deletes one of the user's effects, and its cache.
    pub fn delete_effect(&self, key: &EffectKey) -> CmdResult<()> {
        // Checked again rather than assumed: between the command's refusal and
        // this call, the file may have disappeared — and this is where it is
        // reported.
        self.check_deletable(key)?;
        let path = self.source_path(key);
        fs::remove_file(&path).map_err(|e| {
            Failure::unexpected(format!("cannot delete {}: {e}", crate::paths::shown(&path)))
        })?;
        let _ = fs::remove_file(self.cache_path(key));
        Ok(())
    }

    /// Moves effects from the directory layout — `effects/<id>/source.ts` next to
    /// `effect.js`, `manifest.json` and `swatch.json` — to `effects/<name>.ts`,
    /// and rewrites the settings that refer to them. Returns the old ids and the
    /// names they became.
    ///
    /// Runs at every startup and does nothing once no such directory remains.
    ///
    /// # The order
    ///
    /// Names are planned first, then **`settings.json` is rewritten**, then the
    /// files are moved. A crash in the middle then leaves settings pointing at
    /// names whose files the next startup writes, under the same names: the plan
    /// depends only on what is on disk. The other order would leave moved files
    /// whose settings still name directories that no longer exist.
    ///
    /// The directory is removed only once its file is written: the source is the
    /// one thing that cannot be rebuilt.
    pub fn migrate_directories(&self) -> CmdResult<BTreeMap<String, String>> {
        let entries = match fs::read_dir(&self.shipped_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => {
                return Err(Failure::unexpected(format!(
                    "cannot read {}: {e}",
                    crate::paths::shown(&self.shipped_dir)
                )))
            }
        };
        let mut dirs: Vec<(String, PathBuf)> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().join(LEGACY_SOURCE_FILE).is_file())
            .filter_map(|e| Some((e.file_name().to_str()?.to_owned(), e.path())))
            .collect();
        if dirs.is_empty() {
            return Ok(BTreeMap::new());
        }
        dirs.sort();

        // Written in the folder the directories were in, which is now the shipped
        // one: [`Self::migrate_sources`] moves them to the user's next.
        let mut taken: BTreeSet<String> = self
            .names(Source::Shipped)?
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        let mut plan = Vec::with_capacity(dirs.len());
        for (id, dir) in dirs {
            let wanted = fs::read_to_string(dir.join(LEGACY_MANIFEST_FILE))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|m| m.get("name")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| id.clone());
            let source = read(&dir.join(LEGACY_SOURCE_FILE))?;
            let base = sanitize_name(&wanted);
            // A file already holding this very source was written by a run
            // interrupted before it removed the directory: it is this effect,
            // not a name to avoid.
            let name = if fs::read_to_string(self.source_path(&EffectKey::shipped(&base)))
                .is_ok_and(|s| s == source)
            {
                base
            } else {
                free_name(&base, &taken)
            };
            taken.insert(name.to_lowercase());
            plan.push((id, dir, name, source));
        }
        let renames: BTreeMap<String, String> = plan
            .iter()
            .map(|(id, _, name, _)| (id.clone(), name.clone()))
            .collect();

        let mut settings = self.read_settings()?;
        if settings.version < 1 {
            settings.rename_effects(&renames);
            settings.version = 1;
            self.write_settings(&settings)?;
        }

        create_dir(&self.shipped_dir)?;
        for (_, dir, name, source) in &plan {
            write_atomically(&self.source_path(&EffectKey::shipped(name)), source)?;
            fs::remove_dir_all(dir).map_err(|e| {
                Failure::unexpected(format!("cannot delete {}: {e}", crate::paths::shown(dir)))
            })?;
        }
        Ok(renames)
    }

    /// Moves the references to effects that were compiled into the binary from
    /// their ids to the names of their files, **once**. Returns the ids it
    /// renamed, and the names they became.
    ///
    /// Only the table in [`crate::shipped`] knows those ids: nothing on disk
    /// holds them any more.
    pub fn migrate_former_ids(&self, shipped: &[Shipped]) -> CmdResult<BTreeMap<String, String>> {
        let renames: BTreeMap<String, String> = shipped
            .iter()
            .filter_map(|s| Some((s.former_id?.to_owned(), s.name.to_owned())))
            .collect();
        let mut settings = self.read_settings()?;
        if settings.version >= 2 {
            return Ok(BTreeMap::new());
        }
        settings.rename_effects(&renames);
        settings.version = 2;
        self.write_settings(&settings)?;
        Ok(renames)
    }

    /// Copies the shipped effects into the shipped folder, once each, and updates
    /// the copies nobody changed.
    ///
    /// | State | Action |
    /// |---|---|
    /// | not recorded, no file of that name | copy it, record its hash |
    /// | not recorded, a file of that name exists | leave the file, record it as not ours |
    /// | recorded, file unchanged, shipped version changed | overwrite it, record the new hash |
    /// | recorded, file modified | leave it |
    /// | recorded, file missing (deleted or renamed) | leave it: never copied again |
    /// | recorded, no longer shipped | forget the record |
    ///
    /// "Unchanged" means the file still has the hash recorded when it was copied:
    /// an update never overwrites a line someone wrote. A file left without a
    /// record is not Candeo's: [`Self::migrate_sources`] moves it to the user's
    /// folder.
    pub fn seed_shipped(&self, shipped: &[Shipped]) -> CmdResult<Seeding> {
        let before = self.read_settings()?;
        let mut settings = before.clone();
        let mut seeding = Seeding::default();

        for effect in shipped {
            let hash = sha256_hex(effect.source.as_bytes());
            let recorded = settings.shipped_effects.get(effect.name).cloned();
            let on_disk = self.existing(Source::Shipped, effect.name)?;
            let path = self.source_path(&EffectKey::shipped(effect.name));

            let record = match (recorded, on_disk) {
                (None, Some(_)) => None,
                (None, None) => {
                    create_dir(&self.shipped_dir)?;
                    write_atomically(&path, effect.source)?;
                    seeding.copied += 1;
                    Some(hash)
                }
                (Some(Some(recorded)), Some(existing))
                    if recorded != hash
                        && existing == effect.name
                        && fs::read(&path).is_ok_and(|bytes| sha256_hex(&bytes) == recorded) =>
                {
                    write_atomically(&path, effect.source)?;
                    seeding.updated += 1;
                    Some(hash)
                }
                (Some(record), _) => record,
            };
            settings
                .shipped_effects
                .insert(effect.name.to_owned(), record);
        }
        // An effect this version no longer ships becomes the user's: nothing
        // could restore or update it, and the built-in mark would keep it from
        // being deleted.
        settings
            .shipped_effects
            .retain(|name, _| shipped.iter().any(|s| s.name == name));

        if settings != before {
            self.write_settings(&settings)?;
        }
        Ok(seeding)
    }

    /// Moves to the user's folder every file of the shipped folder Candeo did not
    /// put there, rewriting the references to it, and brings `settings.json` to
    /// effect keys once. Returns, when it did so, each name of that time with the
    /// key it became, for the window's drafts.
    ///
    /// Runs at every startup: a file dropped among the shipped effects, or one
    /// this version no longer ships, moves at the next launch. See
    /// `docs/design/effects-sources.md` §5.
    ///
    /// # The order
    ///
    /// As in [`Self::migrate_directories`]: names are planned from what is on
    /// disk, `settings.json` is rewritten, then files move. A run interrupted in
    /// between plans the same moves again, and a file already copied with the
    /// same bytes is recognized rather than copied twice.
    pub fn migrate_sources(&self) -> CmdResult<BTreeMap<String, String>> {
        let before = self.read_settings()?;
        let mut settings = before.clone();
        let recorded = |name: &str| matches!(before.shipped_effects.get(name), Some(Some(_)));

        let mut taken: BTreeSet<String> = self
            .names(Source::User)?
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        let mut keys = BTreeMap::new();
        let mut moves = Vec::new();
        for name in self.names(Source::Shipped)? {
            if recorded(&name) {
                keys.insert(name.clone(), EffectKey::shipped(&name).to_string());
                continue;
            }
            let from = EffectKey::shipped(&name);
            let same = EffectKey::user(&name);
            let already_copied = fs::read(self.source_path(&same))
                .is_ok_and(|bytes| fs::read(self.source_path(&from)).is_ok_and(|b| b == bytes));
            let to = if already_copied {
                same
            } else {
                EffectKey::user(&free_name(&name, &taken))
            };
            taken.insert(to.name.to_lowercase());
            keys.insert(name, to.to_string());
            moves.push((from, to));
        }

        let mut renames: BTreeMap<String, String> = moves
            .iter()
            .map(|(from, to)| (from.to_string(), to.to_string()))
            .collect();
        let migrating = settings.version < 3;
        if migrating {
            // Names, as version 2 wrote them. One that names no file is the
            // user's: it stays a missing effect, as it was.
            for name in settings.effect_references() {
                if !name.contains(':') {
                    let key = keys
                        .get(&name)
                        .cloned()
                        .unwrap_or_else(|| EffectKey::user(&name).to_string());
                    renames.insert(name, key);
                }
            }
            settings.version = 3;
        }
        settings.rename_effects(&renames);
        settings
            .shipped_effects
            .retain(|_, record| record.is_some());
        if settings != before {
            self.write_settings(&settings)?;
        }

        let mut failure = None;
        for (from, to) in &moves {
            let (source, target) = (self.source_path(from), self.source_path(to));
            let moved = create_dir(&self.user_dir).and_then(|()| {
                if target.exists() {
                    fs::remove_file(&source).map_err(|e| {
                        Failure::unexpected(format!(
                            "cannot delete {}: {e}",
                            crate::paths::shown(&source)
                        ))
                    })
                } else {
                    move_file(&source, &target)
                }
            });
            match moved {
                // Compiled again from its new place.
                Ok(()) => {
                    let _ = fs::remove_file(self.cache_path(from));
                }
                // The others still move; the next launch retries this one.
                Err(e) => failure = failure.or(Some(e)),
            }
        }
        if migrating {
            // The cache of version 2 was one flat folder: compiled again.
            if let Ok(entries) = fs::read_dir(&self.cache_dir) {
                for entry in entries.filter_map(Result::ok) {
                    if entry.path().is_file() {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(if migrating { keys } else { BTreeMap::new() }),
        }
    }

    /// The shipped effects with no file of their name, to offer them back.
    pub fn missing_shipped(&self, shipped: &[Shipped]) -> CmdResult<Vec<String>> {
        let names: BTreeSet<String> = self
            .names(Source::Shipped)?
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        Ok(shipped
            .iter()
            .filter(|s| !names.contains(&s.name.to_lowercase()))
            .map(|s| s.name.to_owned())
            .collect())
    }

    /// Writes a shipped effect's file again and records it: a missing one comes
    /// back, a modified one is overwritten, and updates resume for both.
    ///
    /// The application refuses to delete, rename or save over a built-in, because
    /// each of those loses the same thing: an edited copy stops receiving updates
    /// without a word, a renamed or deleted one never comes back. Duplicating is
    /// how a built-in becomes someone's own. The folder stays theirs, though, and
    /// this is what repairs what was done there.
    ///
    /// Refused when a file Candeo did not put there holds the name in the shipped
    /// folder: restoring never overwrites a line of someone's own.
    pub fn restore_shipped(&self, shipped: &[Shipped], name: &str) -> CmdResult<()> {
        let effect = shipped
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| Failure::new("notBuiltin").with("name", name))?;
        let mut settings = self.read_settings()?;
        if let Some(existing) = self.existing(Source::Shipped, name)? {
            let recorded = matches!(settings.shipped_effects.get(name), Some(Some(_)));
            if existing != name || !recorded {
                return Err(Failure::new("builtinNameTaken").with("name", existing));
            }
        }
        // Recorded first: a file that then fails to be written is simply still
        // missing, and offered again.
        settings
            .shipped_effects
            .insert(name.to_owned(), Some(sha256_hex(effect.source.as_bytes())));
        self.write_settings(&settings)?;
        create_dir(&self.shipped_dir)?;
        write_atomically(&self.source_path(&EffectKey::shipped(name)), effect.source)
    }

    /// Every startup step that brings the library up to date, in order, and the
    /// old effect ids with the keys they became, for the window's drafts.
    ///
    /// Directories first, since their migration assumes version 0 settings; then
    /// the former built-in ids; then the copies, which record themselves in the
    /// settings the first two steps wrote; then the move of what is not Candeo's
    /// to the user's folder, which reads those records. Seeding runs once more:
    /// a shipped effect whose name a moved file held can be copied now.
    pub fn migrate(&self, shipped: &[Shipped]) -> CmdResult<(BTreeMap<String, String>, Seeding)> {
        let mut renames = self.migrate_directories()?;
        renames.extend(self.migrate_former_ids(shipped)?);
        let mut seeding = self.seed_shipped(shipped)?;
        let keys = self.migrate_sources()?;
        let again = self.seed_shipped(shipped)?;
        seeding.copied += again.copied;
        seeding.updated += again.updated;

        // A draft kept under a directory id went through a name to a key.
        for target in renames.values_mut() {
            if let Some(key) = keys.get(target.as_str()) {
                target.clone_from(key);
            }
        }
        renames.extend(keys);
        Ok((renames, seeding))
    }

    /// Reads `settings.json`, or returns the default values if it does not exist.
    pub fn read_settings(&self) -> CmdResult<Settings> {
        let raw = match fs::read_to_string(&self.settings_file) {
            Ok(raw) => raw,
            // First launch: no file, so the defaults. An error here would force
            // the interface to treat the nominal case as an incident.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => {
                return Err(Failure::unexpected(format!(
                    "cannot read {}: {e}",
                    crate::paths::shown(&self.settings_file)
                )))
            }
        };
        let mut settings: Settings = serde_json::from_str(&raw).map_err(|e| {
            Failure::new("settingsUnreadable")
                .with("path", crate::paths::shown(&self.settings_file))
                .with("detail", e)
        })?;
        // Here and nowhere else: this is the only path by which a file enters the
        // application, so the only place where a legacy key can be translated
        // once and for all.
        settings.absorb_legacy();
        Ok(settings)
    }

    /// Writes `settings.json`.
    ///
    /// Goes through a temporary file then a rename: a power cut in the middle of
    /// writing would otherwise leave truncated settings, and so an application
    /// that no longer starts.
    ///
    /// A single temporary name, and it can stay that way: **synchronous** Tauri
    /// commands run on the main thread, so two read-modify-write sequences do not
    /// interleave. A unique name per write was tried then removed — it defended
    /// against an interleaving nothing produces, and left a file behind on every
    /// failure, where a fixed name is simply overwritten on the next attempt.
    ///
    /// ⚠️ **This reasoning holds within one process, not between two.** Two Candeo
    /// instances would write to the *same* `settings.json.tmp`, and one would
    /// rename what the other is writing: the rename would stay atomic, but what it
    /// published would not — truncated settings, or the other instance's. What
    /// keeps this name fixed is therefore [`crate::single_instance`], and the two
    /// decisions are not undone one without the other: making Candeo
    /// multi-instance would require revisiting this name, and revisiting it
    /// without that would buy nothing.
    pub fn write_settings(&self, settings: &Settings) -> CmdResult<()> {
        let Some(parent) = self.settings_file.parent() else {
            return Err(Failure::unexpected("settings path has no parent folder"));
        };
        create_dir(parent)?;

        let json = serde_json::to_string_pretty(settings)
            .map_err(|e| Failure::unexpected(format!("settings not serialisable: {e}")))?;
        let tmp = self.settings_file.with_extension("json.tmp");
        write(&tmp, &json)?;
        fs::rename(&tmp, &self.settings_file).map_err(|e| {
            Failure::unexpected(format!(
                "cannot write {}: {e}",
                crate::paths::shown(&self.settings_file)
            ))
        })
    }

    /// Rewrites `settings.json` with the default values.
    ///
    /// **Touches no effect**, and could not: it only writes to `settings_file`.
    /// This is the distinction the whole module carries — an effect is content,
    /// the choice of the active effect is configuration — and here it protects
    /// something: whoever only wants to un-adopt a keyboard must not lose
    /// hand-written code.
    ///
    /// The file is rewritten rather than deleted. Both read back the same —
    /// [`Self::read_settings`] returns the defaults when there is no file — but a
    /// file that disappears looks like damage, whereas a file reset to defaults
    /// can be read and compared.
    ///
    /// The record of shipped effects stays: it describes the folder, not a
    /// preference, and without it every built-in would become the user's at the
    /// next launch.
    pub fn reset_settings(&self) -> CmdResult<()> {
        self.write_settings(&Settings {
            shipped_effects: self.shipped_records(),
            ..Settings::default()
        })
    }
}

/// The source file of an effect in the directory layout, before named files.
const LEGACY_SOURCE_FILE: &str = "source.ts";
/// Its manifest, which held the name the file is given.
const LEGACY_MANIFEST_FILE: &str = "manifest.json";

/// The library entry of a file, from what its cache holds for its current hash.
///
/// A shipped effect is modified when its file no longer has the hash recorded
/// when it was copied.
fn library_entry(
    key: EffectKey,
    hash: String,
    record: Option<CacheRecord>,
    shipped: &BTreeMap<String, Option<String>>,
) -> EffectEntry {
    let (state, error, swatch, declared) = match record {
        None => (EffectState::Stale, None, Swatch::new(), Declared::nothing()),
        Some(r) if r.error.is_some() => (
            EffectState::Broken,
            r.error,
            Swatch::new(),
            Declared::nothing(),
        ),
        Some(r) => (
            EffectState::Ready,
            None,
            r.swatch,
            Declared {
                description: r.description,
                params: r.params,
                api_version: r.api_version,
                reads_keys: r.reads_keys,
            },
        ),
    };
    let (kind, modified) = match key.source {
        Source::Shipped => (
            EffectKind::Builtin,
            !matches!(shipped.get(&key.name), Some(Some(recorded)) if *recorded == hash),
        ),
        Source::User => (EffectKind::User, false),
    };
    EffectEntry {
        id: key.to_string(),
        kind,
        state,
        error,
        hash: Some(hash),
        modified,
        swatch,
        manifest: Manifest {
            name: key.name,
            description: declared.description,
            params: declared.params,
            api_version: declared.api_version,
            reads_keys: declared.reads_keys,
        },
    }
}

/// Loads the JavaScript once and records what it declares, or why it does not
/// load.
///
/// The swatch is sampled **here**, once per version of the file, and not every
/// time the list is shown: it is a thumbnail that does not move as long as the
/// effect does not. **Nothing is reported about the swatch**, not even a failure:
/// an effect that loads but throws while sampling is ready, just without a
/// thumbnail.
fn compile_record(hash: &str, js: &str) -> CacheRecord {
    let declared = crate::runtime::declared_manifest(js).and_then(|raw| declared_fields(&raw));
    match declared {
        Ok(declared) => CacheRecord {
            hash: hash.to_owned(),
            js: js.to_owned(),
            description: declared.description,
            params: declared.params,
            api_version: declared.api_version,
            reads_keys: declared.reads_keys,
            // The default layout, never the one of the plugged-in keyboard: a
            // swatch that depended on the hardware present would be comparable
            // neither from one effect to another, nor from one machine to another.
            swatch: swatch::sample(js, crate::default_layout()),
            error: None,
        },
        Err(error) => CacheRecord {
            hash: hash.to_owned(),
            js: js.to_owned(),
            description: empty_text(),
            params: serde_json::Map::new(),
            api_version: EFFECTS_API_VERSION,
            reads_keys: false,
            swatch: Swatch::new(),
            error: Some(error),
        },
    }
}

/// Text shown to the user, when an effect declares none.
fn empty_text() -> serde_json::Value {
    serde_json::Value::String(String::new())
}

/// Text an effect declares for the interface: a string, or a map from language
/// codes to strings — `{ en: 'Speed', fr: 'Vitesse' }` — kept as is for the
/// window, which picks the language. Anything else is dropped, not refused: a
/// description is no reason to keep an effect from loading.
fn text_or_empty(value: Option<&serde_json::Value>) -> serde_json::Value {
    match value {
        Some(text @ serde_json::Value::String(_)) => text.clone(),
        Some(serde_json::Value::Object(map))
            if !map.is_empty() && map.values().all(serde_json::Value::is_string) =>
        {
            serde_json::Value::Object(map.clone())
        }
        _ => empty_text(),
    }
}

/// What a module declares that the library keeps.
struct Declared {
    description: serde_json::Value,
    params: serde_json::Map<String, serde_json::Value>,
    api_version: u32,
    reads_keys: bool,
}

impl Declared {
    /// For an effect not compiled, or that does not load.
    fn nothing() -> Self {
        Self {
            description: empty_text(),
            params: serde_json::Map::new(),
            api_version: EFFECTS_API_VERSION,
            reads_keys: false,
        }
    }
}

/// The fields of a declared manifest the library keeps, and the API version check.
///
/// Its error is the effect's, for its author: English, like TypeScript's.
fn declared_fields(raw: &str) -> Result<Declared, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("unreadable manifest: {e}"))?;
    let description = text_or_empty(value.get("description"));
    let params = value
        .get("params")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default();
    // A version that is not a whole number is refused, not read as 1: `'2'` or
    // `2.5` would otherwise pass for an effect of this API (#44).
    let api_version = match value.get("apiVersion") {
        None | Some(serde_json::Value::Null) => EFFECTS_API_VERSION,
        Some(v) => v
            .as_u64()
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
            .ok_or_else(|| format!("apiVersion must be a whole number, not {v}"))?,
    };
    if api_version == 0 || api_version > EFFECTS_API_VERSION {
        return Err(format!(
            "effect written for version {api_version} of the effects API; this version of Candeo only knows version {EFFECTS_API_VERSION}"
        ));
    }
    let reads_keys = value
        .get("inputs")
        .and_then(|i| i.as_array())
        .is_some_and(|inputs| inputs.iter().any(|i| i == "keys"));
    Ok(Declared {
        description,
        params,
        api_version,
        reads_keys,
    })
}

fn create_dir(path: &Path) -> CmdResult<()> {
    fs::create_dir_all(path).map_err(|e| {
        Failure::unexpected(format!("cannot create {}: {e}", crate::paths::shown(path)))
    })
}

fn read(path: &Path) -> CmdResult<String> {
    fs::read_to_string(path)
        .map_err(|e| Failure::unexpected(format!("cannot read {}: {e}", crate::paths::shown(path))))
}

fn write(path: &Path, contents: &str) -> CmdResult<()> {
    fs::write(path, contents).map_err(|e| {
        Failure::unexpected(format!("cannot write {}: {e}", crate::paths::shown(path)))
    })
}

/// Moves a file, across volumes too: `Documents` is often on another drive, or
/// redirected to OneDrive, where a rename fails.
fn move_file(from: &Path, to: &Path) -> CmdResult<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    fs::copy(from, to).map_err(|e| {
        Failure::unexpected(format!("cannot copy {}: {e}", crate::paths::shown(from)))
    })?;
    fs::remove_file(from).map_err(|e| {
        Failure::unexpected(format!("cannot delete {}: {e}", crate::paths::shown(from)))
    })
}

/// Writes through a temporary file and a rename, so that a reader never sees
/// half a file.
///
/// The temporary name ends in `.tmp`, which the library does not list.
fn write_atomically(path: &Path, contents: &str) -> CmdResult<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    write(&tmp, contents)?;
    fs::rename(&tmp, path).map_err(|e| {
        Failure::unexpected(format!("cannot write {}: {e}", crate::paths::shown(path)))
    })
}

// ---------------------------------------------------------------- commands

/// Resolves the system locations. No path is hard-coded: on Windows the data and
/// configuration calls return the same directory, on Linux they do not.
pub(crate) fn store(app: &AppHandle) -> CmdResult<Store> {
    let data = app
        .path()
        .app_data_dir()
        .map_err(|e| Failure::unexpected(format!("no data folder: {e}")))?;
    let config = app
        .path()
        .app_config_dir()
        .map_err(|e| Failure::unexpected(format!("no configuration folder: {e}")))?;
    let cache = app
        .path()
        .app_cache_dir()
        .map_err(|e| Failure::unexpected(format!("no cache folder: {e}")))?;
    Ok(Store::new(&data, &user_dir(app, &data), &config, &cache))
}

/// The user's effects folder: `Documents/candeo/effects`, or, where the system
/// names no documents folder, `app_data_dir()/user-effects`.
fn user_dir(app: &AppHandle, data: &Path) -> PathBuf {
    match app.path().document_dir() {
        Ok(documents) => documents.join("candeo").join("effects"),
        Err(e) => {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                tracing::warn!("no documents folder, user effects kept with the application: {e}");
            });
            data.join("user-effects")
        }
    }
}

/// The library: every file of both sources, with its state.
#[tauri::command]
pub fn list_effects(app: AppHandle) -> CmdResult<Vec<EffectEntry>> {
    store(&app)?.list_effects()
}

/// Writes one of the user's effects, and returns its hash. See
/// [`Store::save_effect_source`].
#[tauri::command]
pub fn save_effect_source(
    app: AppHandle,
    key: String,
    source: String,
    create: bool,
) -> CmdResult<String> {
    store(&app)?.save_effect_source(&EffectKey::parse(&key)?, &source, create)
}

/// Records the JavaScript compiled from this version of an effect. See
/// [`Store::cache_effect`].
#[tauri::command]
pub fn cache_effect(
    app: AppHandle,
    key: String,
    hash: String,
    js: String,
) -> CmdResult<EffectEntry> {
    let entry = store(&app)?.cache_effect(&EffectKey::parse(&key)?, &hash, &js)?;
    // An applied effect edited outside Candeo waits for this compilation to
    // resume: see [`crate::runtime::resume_applied`].
    crate::runtime::resume_waiting(&app, &key);
    Ok(entry)
}

/// Renames one of the user's effects, **and everything that refers to it**: its
/// settings on every device, and the loops running it. Returns its new key.
///
/// A running effect keeps running: the loops keep the code they loaded, only
/// the id they report changes. Stopping them would turn a rename into a gesture
/// that switches the lighting off.
#[tauri::command]
pub fn rename_effect(
    app: AppHandle,
    state: State<'_, AppState>,
    from: String,
    to: String,
) -> CmdResult<String> {
    let store = store(&app)?;
    let target = store
        .rename_effect(&EffectKey::parse(&from)?, &to)?
        .to_string();
    state.engine.rename_everywhere(&from, &target);

    let mut settings = store.read_settings()?;
    if settings.rename_effect(&from, &target) {
        store.write_settings(&settings)?;
    }
    Ok(target)
}

/// Deletes an effect, **and everything `settings.json` kept about it**: its
/// parameters, and where it is applied on devices.
///
/// Both go together: leaving the parameters behind would grow `settings.json`
/// with entries pointing at a name nothing holds any more, and an effect written
/// later under the same name would silently inherit the parameters of its
/// vanished namesake. Leaving the **applied effect** behind would also leave a
/// dangling name — see [`Settings::forget_effect`], where this choice is made.
///
/// Forgetting comes **after** the deletion: if the deletion fails, the effect is
/// still there and its parameters must be too.
///
/// # Three steps, in this order
///
/// 1. **the refusal**, before everything else: a name that designates nothing
///    gets a no without anything having been stopped;
/// 2. **stopping the loops**, on every device where the effect runs, and before
///    the erasure: the engine runs JavaScript loaded at start and kept in memory,
///    so it would carry on with no visible error on a file that is gone;
/// 3. **the erasure**, then forgetting the parameters.
///
/// Stopping on the Rust side rather than in the window: it is the only place that
/// guarantees it whoever the caller is, and the invariant — no loop runs a deleted
/// effect — only holds if it holds everywhere.
#[tauri::command]
pub fn delete_effect(app: AppHandle, state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let key = EffectKey::parse(&id)?;
    let store = store(&app)?;
    store.check_deletable(&key)?;
    state.engine.stop_everywhere(&id);
    store.delete_effect(&key)?;

    let mut settings = store.read_settings()?;
    if settings.forget_effect(&id) {
        store.write_settings(&settings)?;
    }
    Ok(())
}

/// Copies an effect into the user's folder, and returns the copy's key. See
/// [`Store::duplicate_effect`].
#[tauri::command]
pub fn duplicate_effect(app: AppHandle, id: String) -> CmdResult<String> {
    let copy = store(&app)?.duplicate_effect(&EffectKey::parse(&id)?)?;
    Ok(copy.to_string())
}

/// The shipped effects the folder no longer holds. See [`Store::missing_shipped`].
#[tauri::command]
pub fn missing_builtins(app: AppHandle) -> CmdResult<Vec<String>> {
    store(&app)?.missing_shipped(&crate::shipped::ALL)
}

/// Writes a shipped effect's file again. See [`Store::restore_shipped`].
///
/// Loops running a modified version keep the code they loaded, as after any
/// change to a file: applying it again runs the restored one.
#[tauri::command]
pub fn restore_builtin(app: AppHandle, name: String) -> CmdResult<()> {
    store(&app)?.restore_shipped(&crate::shipped::ALL, &name)
}

/// Opens the user's effects folder in the system file manager.
///
/// Adding an effect is saving a `.ts` file there: the folder has to be one click
/// away, not a path to look up.
#[tauri::command]
pub fn open_effects_dir(app: AppHandle) -> CmdResult<()> {
    let store = store(&app)?;
    let dir = store.user_effects_dir()?;
    app.opener()
        .open_path(dir.display().to_string(), None::<&str>)
        .map_err(|e| Failure::unexpected(format!("cannot open {}: {e}", crate::paths::shown(dir))))
}

/// Forgets what `settings.json` keeps about an effect the folder no longer holds:
/// its parameters on every device, and where it was applied.
///
/// Only on request: putting the file back under its name restores everything as
/// long as nobody asked to forget.
#[tauri::command]
pub fn forget_effect_settings(app: AppHandle, id: String) -> CmdResult<()> {
    EffectKey::parse(&id)?;
    let store = store(&app)?;
    let mut settings = store.read_settings()?;
    if settings.forget_effect(&id) {
        store.write_settings(&settings)?;
    }
    Ok(())
}

/// Returns an effect's source, to open it in the editor.
#[tauri::command]
pub fn read_effect_source(app: AppHandle, id: String) -> CmdResult<String> {
    store(&app)?.effect_source(&EffectKey::parse(&id)?)
}

/// What the effects were called before this run's migration, and the keys they
/// became, when it migrated some.
///
/// For the editor's drafts, kept in the web view's storage, which the migration
/// cannot reach. Empty on every later run: the drafts were renamed by the window
/// of the run that migrated.
#[tauri::command]
pub fn legacy_effect_ids(migrated: State<'_, MigratedEffects>) -> BTreeMap<String, String> {
    migrated.0.lock().unwrap().clone()
}

#[tauri::command]
pub fn get_settings(app: AppHandle) -> CmdResult<Settings> {
    store(&app)?.read_settings()
}

#[tauri::command]
pub fn set_settings(app: AppHandle, settings: Settings) -> CmdResult<()> {
    store(&app)?.write_settings(&settings)
}

/// Turns resuming applied effects on or off. Read, changed and written here, so a
/// decision taken meanwhile elsewhere in the file is kept.
#[tauri::command]
pub fn set_resume_effects(app: AppHandle, on: bool) -> CmdResult<()> {
    let store = store(&app)?;
    let mut settings = store.read_settings()?;
    if settings.preferences.resume_effects == on {
        return Ok(());
    }
    settings.preferences.resume_effects = on;
    store.write_settings(&settings)
}

/// Saves the interface theme; the window applies it itself.
#[tauri::command]
pub fn set_theme(app: AppHandle, theme: Theme) -> CmdResult<()> {
    let store = store(&app)?;
    let mut settings = store.read_settings()?;
    if settings.preferences.theme == theme {
        return Ok(());
    }
    settings.preferences.theme = theme;
    store.write_settings(&settings)
}

/// Resets the configuration to the default, and releases the devices.
///
/// # What it does not do
///
/// **No effect is touched.** Written effects live in `app_data_dir()/effects/`,
/// the configuration in `settings.json`: two locations, two actions. Removing an
/// effect is another command, [`delete_effect`], one per effect — mixing the two
/// would make whoever only wanted to un-adopt a keyboard lose hand-written code.
///
/// Nor is it a place to free resources on the effects side: each loop owns its
/// QuickJS `Runtime` and `Context`, and both are destroyed with it — the whole
/// JavaScript heap goes with them.
///
/// # The order
///
/// Devices are released **before** the write: resetting the device table while an
/// effect runs would leave loops that no decision designates any more. See
/// [`crate::release_devices`] for what "release" means in detail.
///
/// The store is resolved first, even before stopping: a configuration directory
/// that cannot be found must be reported without having turned anything off.
///
/// The log level goes back to the default with the rest, **and right away**: it
/// was just erased from the file, and leaving it applied until the next launch
/// would make the screen showing it lie.
#[tauri::command]
pub fn reset_settings(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    let store = store(&app)?;
    crate::release_devices(&state);
    store.reset_settings()?;
    crate::journal::reset_level_to_default();
    Ok(())
}

/// Keeps an effect's parameters for a device, without touching the rest.
///
/// A dedicated command rather than a `set_settings` from the interface: the read,
/// the change and the write happen here, in one go.
///
/// It is not a guard against interleaving — synchronous commands run on the main
/// thread, they do not overlap. It is a guard against a **stale copy**: the window
/// reads the settings once, when the screen mounts, and a `set_settings` posted on
/// the first slider move would send that snapshot back as is, erasing everything
/// decided since. This is not a textbook case — the Rust side writes
/// `settings.json` on every `adopt_device`, and adopting a device is exactly what
/// one does between two adjustments.
///
/// It changes **nothing** in the running effect: adjusting live is
/// [`crate::runtime::set_effect_params`]. The two are separate because they have
/// neither the same rate nor the same destination — dozens of calls per second to
/// the render loop, a single one to disk when the slider stops.
#[tauri::command]
pub fn remember_effect_params(
    app: AppHandle,
    device: DeviceRef,
    effect: String,
    params: serde_json::Map<String, serde_json::Value>,
) -> CmdResult<()> {
    EffectKey::parse(&effect)?;
    let store = store(&app)?;
    let mut settings = store.read_settings()?;

    // Nothing new: the file is not rewritten. A slider moved then brought back
    // comes through here, and so does switching back and forth between two
    // effects — one disk write per pass would tell nobody anything.
    let stored = settings.effect_params(device.vid, device.pid, &effect);
    if stored == Some(&params) || (stored.is_none() && params.is_empty()) {
        return Ok(());
    }

    settings.set_effect_params(device.vid, device.pid, &effect, params);
    store.write_settings(&settings)
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Separate directories, as on Linux: a test that merged them would let a
    /// data / documents / configuration / cache mix-up slip through.
    fn temp_store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().expect("temp dir");
        let store = Store::new(
            &tmp.path().join("data"),
            &effects_dir(&tmp),
            &tmp.path().join("config"),
            &tmp.path().join("cache"),
        );
        (tmp, store)
    }

    /// The user's effects folder.
    fn effects_dir(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("documents").join("candeo").join("effects")
    }

    fn shipped_dir(tmp: &tempfile::TempDir) -> PathBuf {
        tmp.path().join("data").join("effects")
    }

    /// A user effect's cache file.
    fn cache_file(tmp: &tempfile::TempDir, name: &str) -> PathBuf {
        tmp.path()
            .join("cache")
            .join("effects")
            .join("user")
            .join(format!("{name}.json"))
    }

    fn user(name: &str) -> EffectKey {
        EffectKey::user(name)
    }

    fn shipped(name: &str) -> EffectKey {
        EffectKey::shipped(name)
    }

    /// The user's effects in the library.
    fn user_effects(store: &Store) -> Vec<EffectEntry> {
        store
            .list_effects()
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EffectKind::User)
            .collect()
    }

    fn user_effect(store: &Store, name: &str) -> EffectEntry {
        user_effects(store)
            .into_iter()
            .find(|e| e.id == user(name).to_string())
            .unwrap_or_else(|| panic!("\"{name}\" is not listed"))
    }

    fn user_names(store: &Store) -> Vec<String> {
        user_effects(store)
            .into_iter()
            .map(|e| e.manifest.name)
            .collect()
    }

    /// A module that loads, declares a parameter and paints one color. It is
    /// valid JavaScript, so it stands for both the source and what compiling it
    /// gives.
    fn solid_effect(hex: &str) -> String {
        format!(
            "export default {{ description: 'Uni', \
             params: {{ speed: {{ kind: 'number', label: 'Vitesse', min: 0, max: 400, default: 120 }} }}, \
             render({{ layout, frame }}) {{ \
             for (const key of layout.keys) frame.set(key, {{ r: 0x{}, g: 0x{}, b: 0x{} }}) }} }}",
            &hex[0..2],
            &hex[2..4],
            &hex[4..6]
        )
    }

    /// Creates then compiles, as the window does.
    fn create_and_cache(store: &Store, name: &str, js: &str) -> EffectEntry {
        let hash = store.save_effect_source(&user(name), js, true).unwrap();
        store.cache_effect(&user(name), &hash, js).unwrap()
    }

    /// What the gallery marks: an effect declaring keys, and only that one.
    #[test]
    fn the_library_knows_which_effects_read_keys() {
        let (_tmp, store) = temp_store();
        let keys = "export default { inputs: ['keys'], render() {} }";

        assert!(
            create_and_cache(&store, "Frappes", keys)
                .manifest
                .reads_keys
        );
        assert!(user_effect(&store, "Frappes").manifest.reads_keys);
        assert!(
            !create_and_cache(&store, "Uni", &solid_effect("00ff00"))
                .manifest
                .reads_keys
        );
    }

    // ------------------------------------------------------------ files

    #[test]
    fn a_saved_effect_is_a_file_named_after_it() {
        let (tmp, store) = temp_store();
        let js = solid_effect("00ff00");

        let key = user("Onde circulaire");
        let hash = store.save_effect_source(&key, &js, true).unwrap();
        assert_eq!(
            fs::read_to_string(effects_dir(&tmp).join("Onde circulaire.ts")).unwrap(),
            js
        );
        assert_eq!(hash, sha256_hex(js.as_bytes()));

        // Listed right away, as a name only, until the window compiles it.
        let stale = user_effect(&store, "Onde circulaire");
        assert_eq!(stale.state, EffectState::Stale);
        assert_eq!(stale.hash.as_deref(), Some(hash.as_str()));
        assert!(store.effect_js(&key).is_err());

        let entry = store.cache_effect(&key, &hash, &js).unwrap();
        assert_eq!(entry.state, EffectState::Ready);
        assert_eq!(entry.id, "user:Onde circulaire");
        assert_eq!(entry.manifest.name, "Onde circulaire");
        assert_eq!(entry.manifest.description, serde_json::json!("Uni"));
        assert!(entry.manifest.params.contains_key("speed"));
        assert_eq!(entry.manifest.api_version, EFFECTS_API_VERSION);
        assert_eq!(entry.swatch, vec!["#00ff00"; 4]);

        assert_eq!(user_effect(&store, "Onde circulaire"), entry);
        assert_eq!(store.effect_js(&key).unwrap(), js);
        assert_eq!(store.effect_source(&key).unwrap(), js);
    }

    /// **The engine never runs code that differs from the file.** A file changed
    /// outside the application is stale again, and refused until recompiled.
    #[test]
    fn a_file_changed_on_disk_is_stale_until_recompiled() {
        let (tmp, store) = temp_store();
        create_and_cache(&store, "Onde", &solid_effect("00ff00"));

        fs::write(effects_dir(&tmp).join("Onde.ts"), solid_effect("ff0000")).unwrap();

        assert_eq!(user_effect(&store, "Onde").state, EffectState::Stale);
        let err = store.effect_js(&user("Onde")).unwrap_err();
        assert_eq!(err.code, "effectNotCompiled");
    }

    /// Recording JavaScript for a hash the file no longer has would pair this code
    /// with another version of the source.
    #[test]
    fn caching_refuses_code_compiled_from_another_version() {
        let (tmp, store) = temp_store();
        let hash = store
            .save_effect_source(&user("Onde"), &solid_effect("00ff00"), true)
            .unwrap();
        fs::write(effects_dir(&tmp).join("Onde.ts"), solid_effect("ff0000")).unwrap();

        let err = store
            .cache_effect(&user("Onde"), &hash, &solid_effect("00ff00"))
            .unwrap_err();
        assert_eq!(err.code, "effectChanged");
        assert!(!cache_file(&tmp, "Onde").exists());
    }

    /// A module that does not load is recorded with its error, so that it is not
    /// retried at every startup — and it stays openable, to be fixed.
    #[test]
    fn a_module_that_does_not_load_is_broken_until_it_changes() {
        let (tmp, store) = temp_store();
        let entry = create_and_cache(
            &store,
            "Cassé",
            "export default { description: 'sans render' }",
        );

        assert_eq!(entry.state, EffectState::Broken);
        assert!(entry.error.as_deref().is_some_and(|e| !e.is_empty()));
        assert_eq!(user_effect(&store, "Cassé").state, EffectState::Broken);
        let err = store.effect_js(&user("Cassé")).unwrap_err();
        assert_eq!(err.code, "effectBroken");
        assert_eq!(
            store.effect_source(&user("Cassé")).unwrap(),
            "export default { description: 'sans render' }"
        );

        fs::write(effects_dir(&tmp).join("Cassé.ts"), solid_effect("00ff00")).unwrap();
        assert_eq!(user_effect(&store, "Cassé").state, EffectState::Stale);
    }

    /// **A swatch that cannot be sampled does not break the effect**: it loads,
    /// it only throws while rendering, and it must stay runnable and editable.
    #[test]
    fn an_effect_that_throws_while_rendering_is_ready_without_a_swatch() {
        let (_tmp, store) = temp_store();
        let js = "export default { render() { throw new Error('boum') } }";
        let entry = create_and_cache(&store, "Instable", js);

        assert_eq!(entry.state, EffectState::Ready);
        assert!(entry.swatch.is_empty());
        assert_eq!(store.effect_js(&user("Instable")).unwrap(), js);
    }

    #[test]
    fn an_effect_written_for_a_future_api_is_broken() {
        let (_tmp, store) = temp_store();
        let js = format!(
            "export default {{ apiVersion: {}, render() {{}} }}",
            EFFECTS_API_VERSION + 1
        );
        let entry = create_and_cache(&store, "Futur", &js);

        assert_eq!(entry.state, EffectState::Broken);
        let error = entry.error.unwrap();
        assert!(error.contains("effects API"), "message: {error}");
    }

    #[test]
    fn an_api_version_that_is_not_a_whole_number_is_broken() {
        let (_tmp, store) = temp_store();
        for (name, version) in [("Chaine", "'1'"), ("Decimale", "1.5")] {
            let js = format!("export default {{ apiVersion: {version}, render() {{}} }}");
            let entry = create_and_cache(&store, name, &js);
            assert_eq!(entry.state, EffectState::Broken, "{version}");
            let error = entry.error.unwrap();
            assert!(error.contains("whole number"), "{version}: {error}");
        }
    }

    /// A description in several languages is kept for the window; one that is
    /// not text is dropped without keeping the effect from loading.
    #[test]
    fn a_localized_description_is_kept_and_anything_else_dropped() {
        let (_tmp, store) = temp_store();
        let localized = create_and_cache(
            &store,
            "Bilingue",
            "export default { description: { en: 'A wave', fr: 'Une onde' }, render() {} }",
        );
        assert_eq!(localized.state, EffectState::Ready);
        assert_eq!(
            localized.manifest.description,
            serde_json::json!({ "en": "A wave", "fr": "Une onde" })
        );

        for (name, js) in [
            ("Nombre", "export default { description: 42, render() {} }"),
            (
                "Carte mixte",
                "export default { description: { en: 1 }, render() {} }",
            ),
            (
                "Carte vide",
                "export default { description: {}, render() {} }",
            ),
        ] {
            let entry = create_and_cache(&store, name, js);
            assert_eq!(entry.state, EffectState::Ready, "{name}");
            assert_eq!(entry.manifest.description, serde_json::json!(""), "{name}");
        }
    }

    /// Creating never overwrites, saving again never creates: neither gesture can
    /// do the other's job by accident.
    #[test]
    fn creating_never_overwrites_and_saving_again_never_creates() {
        let (_tmp, store) = temp_store();
        store
            .save_effect_source(&user("Onde"), &solid_effect("00ff00"), true)
            .unwrap();

        for name in ["Onde", "onde", "ONDE"] {
            let err = store
                .save_effect_source(&user(name), "x", true)
                .unwrap_err();
            assert_eq!(err.code, "effectExists", "\"{name}\"");
        }
        let err = store
            .save_effect_source(&user("Absent"), "x", false)
            .unwrap_err();
        assert_eq!(err.code, "effectNotFound");
        // Saving again under another case is not this effect.
        assert!(store.save_effect_source(&user("onde"), "x", false).is_err());

        store
            .save_effect_source(&user("Onde"), "v2", false)
            .unwrap();
        assert_eq!(store.effect_source(&user("Onde")).unwrap(), "v2");
        assert_eq!(user_effects(&store).len(), 1);
    }

    /// A key names its source, and nothing else is a key.
    #[test]
    fn a_key_is_a_source_and_a_valid_name() {
        assert_eq!(EffectKey::parse("user:Rain").unwrap(), user("Rain"));
        assert_eq!(EffectKey::parse("shipped:Rain").unwrap(), shipped("Rain"));
        assert_eq!(user("Rain").to_string(), "user:Rain");
        for bad in [
            "Rain",
            "hardware:wave",
            "user:",
            "user:a/b",
            ":Rain",
            "user:a:b",
        ] {
            assert!(EffectKey::parse(bad).is_err(), "\"{bad}\" was accepted");
        }
    }

    /// A built-in and one of the user's may share a name: two effects, two keys.
    #[test]
    fn a_shipped_and_a_user_effect_may_share_a_name() {
        let (_tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();
        create_and_cache(&store, "Livré", &solid_effect("00ff00"));

        let ids: Vec<String> = store
            .list_effects()
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["shipped:Livré", "user:Livré"]);
        assert_eq!(
            store.effect_source(&shipped("Livré")).unwrap(),
            shipped_v1()[0].source
        );
        assert_eq!(
            store.effect_js(&user("Livré")).unwrap(),
            solid_effect("00ff00")
        );
    }

    /// Only `.ts` files with an acceptable name make the library: a temporary
    /// file, a directory or a foreign file do not.
    #[test]
    fn the_library_lists_only_effect_files() {
        let (tmp, store) = temp_store();
        let dir = effects_dir(&tmp);
        fs::create_dir_all(dir.join("dossier.ts")).unwrap();
        fs::write(dir.join("Onde.ts.tmp"), "x").unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        fs::write(dir.join(" espace.ts"), "x").unwrap();
        fs::write(dir.join("Vague.ts"), "x").unwrap();

        assert_eq!(user_names(&store), ["Vague"]);
    }

    /// Two names differing only by case can sit side by side on Linux; they are
    /// one file on Windows, and one library key: the first stays.
    #[test]
    fn names_differing_only_by_case_are_listed_once() {
        let (tmp, store) = temp_store();
        let dir = effects_dir(&tmp);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Onde.ts"), "first").unwrap();
        fs::write(dir.join("onde.ts"), "second").unwrap();

        // A case-insensitive file system already made them one file.
        if fs::read_dir(&dir).unwrap().count() == 2 {
            assert_eq!(user_names(&store), ["Onde"]);
        }
    }

    #[test]
    fn the_library_is_empty_before_any_effect() {
        let (_tmp, store) = temp_store();
        assert!(store.list_effects().unwrap().is_empty());
    }

    // ------------------------------------------------------------ names

    #[test]
    fn names_follow_the_windows_rules_everywhere() {
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        for bad in [
            "",
            "a/b",
            "a\\b",
            "C:",
            "a*b",
            "a?b",
            "\"guillemets\"",
            "<chevrons>",
            "a|b",
            " devant",
            "derrière ",
            "point.",
            ".caché",
            "CON",
            "con.txt",
            "Nul",
            "lpt1",
            "cloche\u{7}",
            too_long.as_str(),
        ] {
            assert!(validate_name(bad).is_err(), "\"{bad}\" was accepted");
        }

        let longest = "é".repeat(MAX_NAME_LEN);
        for good in [
            "Onde radiale (copie)",
            "été à l'ombre",
            "v1.2",
            "Mon effet 2",
            "console",
            longest.as_str(),
        ] {
            validate_name(good).unwrap_or_else(|e| panic!("\"{good}\": {e}"));
        }
    }

    /// The refusal must come from validation, never from the disk: a name that
    /// reached it would have been taken for an acceptable path.
    #[test]
    fn dangerous_names_never_reach_the_disk() {
        let (tmp, store) = temp_store();
        let sibling = tmp.path().join("documents").join("candeo").join("secrets");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("x.ts"), "secret").unwrap();

        for name in [
            "..",
            "../secrets/x",
            "..\\secrets\\x",
            "C:\\Windows",
            "/etc/passwd",
            "a/b",
        ] {
            for err in [
                store.delete_effect(&user(name)).unwrap_err(),
                store.effect_source(&user(name)).unwrap_err(),
                store
                    .save_effect_source(&user(name), "x", true)
                    .unwrap_err(),
            ] {
                assert!(
                    !["effectNotFound", "unexpected"].contains(&err.code),
                    "\"{name}\" reached the disk: {err}"
                );
            }
        }
        assert_eq!(fs::read_to_string(sibling.join("x.ts")).unwrap(), "secret");
    }

    #[test]
    fn sanitized_names_are_valid() {
        let taken = BTreeSet::from(["onde".to_owned(), "onde (2)".to_owned()]);
        for wanted in [
            "Onde / Vague : v2",
            "CON",
            "  ..  ",
            "🙂🙂🙂",
            &"x".repeat(200),
            "fin.",
        ] {
            let name = free_name(&sanitize_name(wanted), &taken);
            validate_name(&name).unwrap_or_else(|e| panic!("\"{wanted}\" -> \"{name}\": {e}"));
        }
        assert_eq!(sanitize_name("Onde / Vague : v2"), "Onde - Vague - v2");
        assert_eq!(sanitize_name("CON"), "CON effect");
        assert_eq!(sanitize_name("  ..  "), "Effect");
        assert_eq!(free_name("Onde", &taken), "Onde (3)");
        assert_eq!(free_name("ONDE", &taken), "ONDE (3)");
        assert_eq!(free_name("Vague", &taken), "Vague");
    }

    // ------------------------------------------------------------ rename, delete

    /// Renaming moves the file and its cache: the effect stays compiled.
    #[test]
    fn renaming_moves_the_file_and_its_cache() {
        let (tmp, store) = temp_store();
        let js = solid_effect("00ff00");
        create_and_cache(&store, "Onde", &js);
        create_and_cache(&store, "Autre", &solid_effect("ff0000"));

        assert_eq!(
            store.rename_effect(&user("Onde"), "Vague").unwrap(),
            user("Vague")
        );
        assert!(!effects_dir(&tmp).join("Onde.ts").exists());
        assert!(!cache_file(&tmp, "Onde").exists());
        let entry = user_effect(&store, "Vague");
        assert_eq!(entry.state, EffectState::Ready);
        assert_eq!(store.effect_js(&user("Vague")).unwrap(), js);

        let err = store.rename_effect(&user("Vague"), "autre").unwrap_err();
        assert_eq!(err.code, "effectExists");
        assert!(store.rename_effect(&user("Absent"), "Nouveau").is_err());
        assert!(store.rename_effect(&user("Vague"), "a:b").is_err());

        // Only the case changes: the same file, renamed.
        store.rename_effect(&user("Vague"), "vague").unwrap();
        assert_eq!(user_effect(&store, "vague").state, EffectState::Ready);
    }

    #[test]
    fn delete_removes_the_file_and_its_cache() {
        let (tmp, store) = temp_store();
        create_and_cache(&store, "Onde", &solid_effect("00ff00"));

        store.check_deletable(&user("Onde")).unwrap();
        assert!(
            effects_dir(&tmp).join("Onde.ts").is_file(),
            "the check took the file away"
        );
        store.delete_effect(&user("Onde")).unwrap();
        assert!(!effects_dir(&tmp).join("Onde.ts").exists());
        assert!(!cache_file(&tmp, "Onde").exists());
        assert!(user_effects(&store).is_empty());

        let err = store.delete_effect(&user("Onde")).unwrap_err();
        assert_eq!(err.code, "effectNotFound");
    }

    /// A copy is a new effect: the same source, already compiled, numbered only
    /// when the user's folder already holds its name.
    #[test]
    fn duplicating_makes_a_ready_copy_under_a_free_name() {
        let (tmp, store) = temp_store();
        let js = solid_effect("00ff00");
        create_and_cache(&store, "Onde", &js);

        let duplicate = |key: EffectKey| store.duplicate_effect(&key).unwrap();
        assert_eq!(duplicate(user("Onde")), user("Onde (2)"));
        assert_eq!(duplicate(user("Onde")), user("Onde (3)"));
        assert_eq!(duplicate(user("Onde (2)")), user("Onde (2) (2)"));

        let copy = user_effect(&store, "Onde (2)");
        assert_eq!(copy.state, EffectState::Ready);
        assert_eq!(store.effect_js(&user("Onde (2)")).unwrap(), js);
        assert!(effects_dir(&tmp).join("Onde (3).ts").is_file());
        assert!(store.duplicate_effect(&user("Absent")).is_err());
    }

    /// A built-in's first copy keeps its name, under `user:`: nothing of that
    /// name is the user's yet. The next ones are numbered.
    #[test]
    fn a_builtins_first_copy_keeps_its_name() {
        let (_tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();

        let first = store.duplicate_effect(&shipped("Livré")).unwrap();
        assert_eq!(first, user("Livré"));
        assert_eq!(user_effect(&store, "Livré").kind, EffectKind::User);
        builtin(&store, "Livré");

        assert_eq!(
            store.duplicate_effect(&shipped("Livré")).unwrap(),
            user("Livré (2)")
        );
    }

    #[test]
    fn a_numbered_name_stays_valid() {
        let long = "a".repeat(MAX_NAME_LEN);
        let taken = BTreeSet::from([long.clone()]);
        let name = free_name(&long, &taken);
        validate_name(&name).unwrap_or_else(|e| panic!("\"{name}\": {e}"));
        assert!(name.ends_with(" (2)"), "name: {name}");
    }

    // ------------------------------------------------------------ migration

    /// A data folder as the directory layout left it: two effects with the same
    /// name, one whose name is not a valid file name, and settings pointing at
    /// them, at a built-in and at an effect removed by hand.
    fn plant_directory_layout(tmp: &tempfile::TempDir) {
        for (id, name, source) in [
            ("onde-circulaire", "Onde circulaire", "source circulaire"),
            ("onde-vague-v2", "Onde / Vague : v2", "source vague"),
            ("onde-circulaire-bis", "Onde circulaire", "source bis"),
        ] {
            let dir = shipped_dir(tmp).join(id);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("source.ts"), source).unwrap();
            fs::write(dir.join("effect.js"), "compiled").unwrap();
            fs::write(dir.join("swatch.json"), "[]").unwrap();
            fs::write(
                dir.join("manifest.json"),
                format!(
                    "{{\n  \"name\": {},\n  \"description\": \"\",\n  \"params\": {{}},\n  \"apiVersion\": 1\n}}",
                    serde_json::to_string(name).unwrap()
                ),
            )
            .unwrap();
        }

        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("settings.json"),
            r#"{
              "preferences": {},
              "devices": [{ "vid": 5426, "pid": 658, "state": "adopted" }],
              "activeEffects": [
                { "vid": 5426, "pid": 658, "effect": "onde-circulaire-bis" },
                { "vid": 5426, "pid": 659, "effect": "respiration" }
              ],
              "effectParams": [
                { "vid": 5426, "pid": 658, "effect": "onde-vague-v2", "values": { "speed": 4 } },
                { "vid": 5426, "pid": 658, "effect": "disparu", "values": { "speed": 1 } }
              ]
            }"#,
        )
        .unwrap();
    }

    #[test]
    fn the_directory_layout_moves_to_named_files() {
        let (tmp, store) = temp_store();
        plant_directory_layout(&tmp);

        let renames = store.migrate_directories().unwrap();
        assert_eq!(
            renames,
            BTreeMap::from([
                ("onde-circulaire".to_owned(), "Onde circulaire".to_owned()),
                (
                    "onde-circulaire-bis".to_owned(),
                    "Onde circulaire (2)".to_owned()
                ),
                ("onde-vague-v2".to_owned(), "Onde - Vague - v2".to_owned()),
            ])
        );

        // In the folder the directories were in: `migrate_sources` moves them next.
        let dir = shipped_dir(&tmp);
        for (name, source) in [
            ("Onde circulaire", "source circulaire"),
            ("Onde circulaire (2)", "source bis"),
            ("Onde - Vague - v2", "source vague"),
        ] {
            assert_eq!(
                fs::read_to_string(dir.join(format!("{name}.ts"))).unwrap(),
                source
            );
        }
        for id in renames.keys() {
            assert!(!dir.join(id).exists(), "\"{id}\" was not removed");
        }

        let settings = store.read_settings().unwrap();
        assert_eq!(settings.version, 1);
        assert_eq!(
            settings.active_effect(VID, PID),
            Some("Onde circulaire (2)")
        );
        // Former built-in ids are the next step's: see `migrate_former_ids`.
        assert_eq!(settings.active_effect(VID, PID + 1), Some("respiration"));
        assert_eq!(
            settings.effect_params(VID, PID, "Onde - Vague - v2"),
            Some(&to_map(&[("speed", serde_json::json!(4))]))
        );
        // An id that names nothing is left as it was, not guessed.
        assert!(settings.effect_params(VID, PID, "disparu").is_some());
        assert_eq!(settings.device_state(VID, PID, None), DeviceState::Adopted);
    }

    #[test]
    fn migrating_twice_changes_nothing() {
        let (tmp, store) = temp_store();
        plant_directory_layout(&tmp);
        store.migrate_directories().unwrap();
        let settings = fs::read_to_string(tmp.path().join("config").join("settings.json")).unwrap();

        assert!(store.migrate_directories().unwrap().is_empty());
        assert_eq!(store.names(Source::Shipped).unwrap().len(), 3);
        assert_eq!(
            fs::read_to_string(tmp.path().join("config").join("settings.json")).unwrap(),
            settings
        );
    }

    /// A run interrupted after writing a file but before removing its directory:
    /// the next one recognizes the file instead of writing a second copy.
    #[test]
    fn an_interrupted_migration_does_not_duplicate_an_effect() {
        let (tmp, store) = temp_store();
        plant_directory_layout(&tmp);
        fs::write(
            shipped_dir(&tmp).join("Onde circulaire.ts"),
            "source circulaire",
        )
        .unwrap();

        store.migrate_directories().unwrap();
        let names = store.names(Source::Shipped).unwrap();
        assert_eq!(
            names,
            [
                "Onde - Vague - v2",
                "Onde circulaire",
                "Onde circulaire (2)"
            ]
        );
    }

    #[test]
    fn without_directories_the_settings_are_not_touched() {
        let (tmp, store) = temp_store();
        assert!(store.migrate_directories().unwrap().is_empty());
        assert!(!tmp.path().join("config").join("settings.json").exists());
    }

    // ------------------------------------------------------------ shipped effects

    /// Two versions of a shipped effect, to watch an update happen.
    fn shipped_v1() -> [Shipped; 1] {
        [Shipped {
            name: "Livré",
            former_id: Some("livre"),
            source: "export default { render() {} } // v1",
        }]
    }

    fn shipped_v2() -> [Shipped; 1] {
        [Shipped {
            name: "Livré",
            former_id: Some("livre"),
            source: "export default { render() {} } // v2",
        }]
    }

    #[test]
    fn a_first_launch_copies_every_shipped_effect() {
        let (tmp, store) = temp_store();

        let seeding = store.seed_shipped(&crate::shipped::ALL).unwrap();
        assert_eq!(seeding.copied, crate::shipped::ALL.len());

        let settings = store.read_settings().unwrap();
        assert_eq!(settings.version, SETTINGS_VERSION);
        for s in &crate::shipped::ALL {
            assert_eq!(
                fs::read_to_string(shipped_dir(&tmp).join(format!("{}.ts", s.name))).unwrap(),
                s.source
            );
            assert_eq!(
                settings.shipped_effects.get(s.name),
                Some(&Some(sha256_hex(s.source.as_bytes())))
            );
        }
        let library = store.list_effects().unwrap();
        assert_eq!(library.len(), crate::shipped::ALL.len());
        assert!(library.iter().all(|e| e.kind == EffectKind::Builtin));
        assert!(library.iter().all(|e| e.id.starts_with("shipped:")));
        assert!(!effects_dir(&tmp).exists(), "the user's folder was created");
        // Copied, not compiled: the window compiles them at startup.
        assert!(library.iter().all(|e| e.state == EffectState::Stale));

        // And the next launch finds nothing to do.
        assert_eq!(
            store.seed_shipped(&crate::shipped::ALL).unwrap(),
            Seeding::default()
        );
    }

    /// An update never overwrites a line someone wrote: only a copy that still
    /// has the hash recorded when it was copied is replaced.
    #[test]
    fn an_unchanged_copy_is_updated_and_a_modified_one_is_not() {
        let (tmp, store) = temp_store();
        let file = shipped_dir(&tmp).join("Livré.ts");

        store.seed_shipped(&shipped_v1()).unwrap();
        let seeding = store.seed_shipped(&shipped_v2()).unwrap();
        assert_eq!(seeding.updated, 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), shipped_v2()[0].source);

        fs::write(&file, "export default { render() {} } // mine").unwrap();
        let seeding = store
            .seed_shipped(&[Shipped {
                name: "Livré",
                former_id: Some("livre"),
                source: "export default { render() {} } // v3",
            }])
            .unwrap();
        assert_eq!(seeding, Seeding::default());
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "export default { render() {} } // mine"
        );
        // Modified, it is still the shipped one.
        assert_eq!(user_effects(&store).len(), 0);
    }

    /// A shipped effect someone deleted in the folder does not come back by
    /// itself; renamed there, it is not Candeo's any more, and moves to the
    /// user's folder.
    #[test]
    fn a_deleted_or_renamed_copy_never_comes_back() {
        let (tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();
        fs::remove_file(shipped_dir(&tmp).join("Livré.ts")).unwrap();
        assert_eq!(
            store.seed_shipped(&shipped_v2()).unwrap(),
            Seeding::default()
        );
        assert!(!shipped_dir(&tmp).join("Livré.ts").exists());

        let (tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();
        fs::rename(
            shipped_dir(&tmp).join("Livré.ts"),
            shipped_dir(&tmp).join("Le mien.ts"),
        )
        .unwrap();
        let (_, seeding) = store.migrate(&shipped_v2()).unwrap();
        assert_eq!(seeding, Seeding::default());
        assert!(!shipped_dir(&tmp).join("Livré.ts").exists());
        assert!(effects_dir(&tmp).join("Le mien.ts").is_file());
        assert_eq!(user_effect(&store, "Le mien").kind, EffectKind::User);
    }

    /// A file of the user's among the shipped effects, under a shipped effect's
    /// name, moves to the user's folder, and the shipped effect is copied then.
    #[test]
    fn a_users_file_with_a_shipped_name_moves_to_the_users_folder() {
        let (tmp, store) = temp_store();
        fs::create_dir_all(shipped_dir(&tmp)).unwrap();
        fs::write(shipped_dir(&tmp).join("Livré.ts"), "mine").unwrap();

        let (_, seeding) = store.migrate(&shipped_v1()).unwrap();
        assert_eq!(seeding.copied, 1);
        assert_eq!(
            fs::read_to_string(effects_dir(&tmp).join("Livré.ts")).unwrap(),
            "mine"
        );
        assert_eq!(
            fs::read_to_string(shipped_dir(&tmp).join("Livré.ts")).unwrap(),
            shipped_v1()[0].source
        );
        builtin(&store, "Livré");
        assert_eq!(user_effect(&store, "Livré").kind, EffectKind::User);
    }

    fn builtin(store: &Store, name: &str) -> EffectEntry {
        store
            .list_effects()
            .unwrap()
            .into_iter()
            .find(|e| e.id == shipped(name).to_string() && e.kind == EffectKind::Builtin)
            .unwrap_or_else(|| panic!("\"{name}\" is not a listed built-in"))
    }

    #[test]
    fn a_builtin_is_not_deleted_renamed_or_saved_over() {
        let (tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();
        let livre = shipped("Livré");

        let codes = [
            store.delete_effect(&livre).unwrap_err().code,
            store.rename_effect(&livre, "Le mien").unwrap_err().code,
            store
                .save_effect_source(&livre, "mine", false)
                .unwrap_err()
                .code,
        ];
        assert_eq!(
            codes,
            ["builtinNotDeleted", "builtinNotRenamed", "builtinNotSaved"]
        );
        assert_eq!(
            fs::read_to_string(shipped_dir(&tmp).join("Livré.ts")).unwrap(),
            shipped_v1()[0].source
        );

        // Duplicating is how it becomes someone's own.
        let copy = store.duplicate_effect(&livre).unwrap();
        store.save_effect_source(&copy, "mine", false).unwrap();
        let renamed = store.rename_effect(&copy, "Le mien").unwrap();
        store.delete_effect(&renamed).unwrap();
    }

    #[test]
    fn a_builtin_edited_in_the_folder_is_marked_and_restored() {
        let (tmp, store) = temp_store();
        let file = shipped_dir(&tmp).join("Livré.ts");
        store.seed_shipped(&shipped_v1()).unwrap();
        assert!(!builtin(&store, "Livré").modified);

        fs::write(&file, "export default { render() {} } // mine").unwrap();
        assert!(builtin(&store, "Livré").modified);

        store.restore_shipped(&shipped_v1(), "Livré").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), shipped_v1()[0].source);
        assert!(!builtin(&store, "Livré").modified);
    }

    #[test]
    fn a_missing_builtin_comes_back_on_request_and_is_updated_again() {
        let (tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();
        assert!(store.missing_shipped(&shipped_v1()).unwrap().is_empty());

        fs::remove_file(shipped_dir(&tmp).join("Livré.ts")).unwrap();
        assert_eq!(store.missing_shipped(&shipped_v1()).unwrap(), ["Livré"]);

        store.restore_shipped(&shipped_v1(), "Livré").unwrap();
        assert!(store.missing_shipped(&shipped_v1()).unwrap().is_empty());
        assert!(!builtin(&store, "Livré").modified);
        assert_eq!(store.seed_shipped(&shipped_v2()).unwrap().updated, 1);
    }

    /// An effect of the user's named after a missing built-in lives in another
    /// folder: the built-in stays missing, and restoring it touches neither.
    #[test]
    fn a_users_effect_named_after_a_missing_builtin_does_not_hide_it() {
        let (tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();
        fs::remove_file(shipped_dir(&tmp).join("Livré.ts")).unwrap();
        store
            .save_effect_source(&user("Livré"), "mine", true)
            .unwrap();

        assert_eq!(store.missing_shipped(&shipped_v1()).unwrap(), ["Livré"]);
        store.restore_shipped(&shipped_v1(), "Livré").unwrap();
        assert_eq!(
            fs::read_to_string(effects_dir(&tmp).join("Livré.ts")).unwrap(),
            "mine"
        );
        assert!(!builtin(&store, "Livré").modified);
    }

    #[test]
    fn only_shipped_effects_are_restored() {
        let (_tmp, store) = temp_store();
        let err = store.restore_shipped(&shipped_v1(), "Autre").unwrap_err();
        assert_eq!(err.code, "notBuiltin");
    }

    /// No longer shipped, it moves to the user's folder, with what refers to it.
    #[test]
    fn an_effect_no_longer_shipped_becomes_the_users() {
        let (tmp, store) = temp_store();
        store.migrate(&shipped_v1()).unwrap();
        let mut settings = store.read_settings().unwrap();
        settings.set_active_effect(VID, PID, Some("shipped:Livré"));
        store.write_settings(&settings).unwrap();

        store.migrate(&[]).unwrap();

        let settings = store.read_settings().unwrap();
        assert!(settings.shipped_effects.is_empty());
        assert_eq!(settings.active_effect(VID, PID), Some("user:Livré"));
        assert!(!shipped_dir(&tmp).join("Livré.ts").exists());
        assert_eq!(user_effect(&store, "Livré").kind, EffectKind::User);
        store.delete_effect(&user("Livré")).unwrap();
    }

    #[test]
    fn a_compiled_builtin_is_still_listed_as_one() {
        let (_tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();
        let hash = sha256_hex(shipped_v1()[0].source.as_bytes());

        let entry = store
            .cache_effect(&shipped("Livré"), &hash, &solid_effect("00ff00"))
            .unwrap();

        assert_eq!(entry.kind, EffectKind::Builtin);
        assert_eq!(entry.id, "shipped:Livré");
    }

    #[test]
    fn reset_keeps_the_record_of_shipped_effects() {
        let (_tmp, store) = temp_store();
        store.seed_shipped(&shipped_v1()).unwrap();

        store.reset_settings().unwrap();

        assert_eq!(
            store.seed_shipped(&shipped_v1()).unwrap(),
            Seeding::default()
        );
        builtin(&store, "Livré");
    }

    #[test]
    fn former_builtin_ids_move_to_names_once() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("settings.json"),
            r#"{
              "version": 1,
              "activeEffects": [{ "vid": 5426, "pid": 658, "effect": "onde-matricielle" }],
              "effectParams": [
                { "vid": 5426, "pid": 658, "effect": "respiration", "values": { "period": 7 } },
                { "vid": 5426, "pid": 658, "effect": "Mon effet", "values": { "speed": 1 } }
              ]
            }"#,
        )
        .unwrap();

        let renames = store.migrate_former_ids(&crate::shipped::ALL).unwrap();
        assert_eq!(
            renames.get("respiration").map(String::as_str),
            Some("Breathing")
        );

        let settings = store.read_settings().unwrap();
        assert_eq!(settings.version, 2);
        assert_eq!(settings.active_effect(VID, PID), Some("Diagonal wave"));
        assert!(settings.effect_params(VID, PID, "Breathing").is_some());
        assert!(settings.effect_params(VID, PID, "Mon effet").is_some());

        assert!(store
            .migrate_former_ids(&crate::shipped::ALL)
            .unwrap()
            .is_empty());
    }

    /// Every step together, on a folder of the directory layout: the library the
    /// application starts with afterwards.
    #[test]
    fn an_old_installation_is_brought_up_to_date() {
        let (tmp, store) = temp_store();
        plant_directory_layout(&tmp);

        let (renames, seeding) = store.migrate(&crate::shipped::ALL).unwrap();
        // Directory ids and names, each to the key it became, for the drafts.
        assert_eq!(
            renames.get("onde-circulaire").map(String::as_str),
            Some("user:Onde circulaire")
        );
        assert_eq!(
            renames.get("respiration").map(String::as_str),
            Some("shipped:Breathing")
        );
        assert_eq!(
            renames.get("Onde circulaire").map(String::as_str),
            Some("user:Onde circulaire")
        );
        assert_eq!(seeding.copied, crate::shipped::ALL.len());

        let settings = store.read_settings().unwrap();
        assert_eq!(settings.version, SETTINGS_VERSION);
        assert_eq!(
            settings.active_effect(VID, PID),
            Some("user:Onde circulaire (2)")
        );
        assert_eq!(
            settings.active_effect(VID, PID + 1),
            Some("shipped:Breathing")
        );
        // A name that designated nothing stays missing, as one of the user's.
        assert!(settings.effect_params(VID, PID, "user:disparu").is_some());
        assert_eq!(
            store.list_effects().unwrap().len(),
            3 + crate::shipped::ALL.len()
        );
        assert_eq!(
            user_names(&store),
            [
                "Onde - Vague - v2",
                "Onde circulaire",
                "Onde circulaire (2)"
            ]
        );
        // Nothing but Candeo's files stays among the shipped effects.
        assert_eq!(
            store.names(Source::Shipped).unwrap().len(),
            crate::shipped::ALL.len()
        );

        // And the next launch finds nothing to do.
        let settings = store.read_settings().unwrap();
        let (renames, seeding) = store.migrate(&crate::shipped::ALL).unwrap();
        assert!(renames.is_empty());
        assert_eq!(seeding, Seeding::default());
        assert_eq!(store.read_settings().unwrap(), settings);
    }

    /// Version 2 of an installation: every effect in one folder, referenced by
    /// its name, with a compiled cache.
    #[test]
    fn a_version_2_folder_is_split_by_source() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(shipped_dir(&tmp)).unwrap();
        let livre = shipped_v1()[0].source;
        fs::write(shipped_dir(&tmp).join("Livré.ts"), livre).unwrap();
        fs::write(shipped_dir(&tmp).join("Mon effet.ts"), "mine").unwrap();
        let old_cache = tmp.path().join("cache").join("effects");
        fs::create_dir_all(&old_cache).unwrap();
        fs::write(old_cache.join("Mon effet.json"), "{}").unwrap();
        fs::write(
            config.join("settings.json"),
            format!(
                r#"{{
                  "version": 2,
                  "activeEffects": [
                    {{ "vid": 5426, "pid": 658, "effect": "Mon effet" }},
                    {{ "vid": 5426, "pid": 659, "effect": "Livré" }}
                  ],
                  "effectParams": [{{ "vid": 5426, "pid": 658, "effect": "Livré", "values": {{ "speed": 2 }} }}],
                  "shippedEffects": {{ "Livré": "{}" }}
                }}"#,
                sha256_hex(livre.as_bytes())
            ),
        )
        .unwrap();

        let (renames, _) = store.migrate(&shipped_v1()).unwrap();
        assert_eq!(
            renames,
            BTreeMap::from([
                ("Livré".to_owned(), "shipped:Livré".to_owned()),
                ("Mon effet".to_owned(), "user:Mon effet".to_owned()),
            ])
        );

        assert_eq!(
            fs::read_to_string(effects_dir(&tmp).join("Mon effet.ts")).unwrap(),
            "mine"
        );
        assert!(!shipped_dir(&tmp).join("Mon effet.ts").exists());
        assert!(
            !old_cache.join("Mon effet.json").exists(),
            "the flat cache was kept"
        );
        let settings = store.read_settings().unwrap();
        assert_eq!(settings.version, 3);
        assert_eq!(settings.active_effect(VID, PID), Some("user:Mon effet"));
        assert_eq!(settings.active_effect(VID, PID + 1), Some("shipped:Livré"));
        assert!(settings.effect_params(VID, PID, "shipped:Livré").is_some());
        builtin(&store, "Livré");
        assert_eq!(user_effect(&store, "Mon effet").state, EffectState::Stale);
    }

    /// A run interrupted after copying a file to the user's folder, before
    /// removing it from the shipped one: the next run recognizes the copy.
    #[test]
    fn an_interrupted_move_does_not_duplicate_an_effect() {
        let (tmp, store) = temp_store();
        fs::create_dir_all(shipped_dir(&tmp)).unwrap();
        fs::create_dir_all(effects_dir(&tmp)).unwrap();
        fs::write(shipped_dir(&tmp).join("Mon effet.ts"), "mine").unwrap();
        fs::write(effects_dir(&tmp).join("Mon effet.ts"), "mine").unwrap();

        store.migrate_sources().unwrap();

        assert_eq!(user_names(&store), ["Mon effet"]);
        assert!(!shipped_dir(&tmp).join("Mon effet.ts").exists());
    }

    /// A file written before the version field reads as version 0 — not as the
    /// current version the defaults carry, which would skip the migration.
    #[test]
    fn a_file_without_version_reads_as_version_zero() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("settings.json"), r#"{"devices":[]}"#).unwrap();

        assert_eq!(store.read_settings().unwrap().version, 0);
        assert_eq!(Settings::default().version, SETTINGS_VERSION);
    }

    #[test]
    fn without_a_file_settings_are_the_default() {
        let (_tmp, store) = temp_store();
        assert_eq!(store.read_settings().unwrap(), Settings::default());
    }

    /// An error names the file it is about, without the home directory: it is
    /// logged, and the log goes into bug reports. The temporary folder sits in
    /// the home directory on Windows.
    #[test]
    fn an_error_names_its_file_without_the_home_directory() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("settings.json"), "{ not json").unwrap();

        let message = store.read_settings().unwrap_err().to_string();
        assert!(message.contains("settings.json"), "{message}");
        let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
        if let Some(home) = home.ok().filter(|h| !h.is_empty()) {
            assert!(!message.contains(&home), "{message}");
        }
    }

    #[test]
    fn settings_round_trip() {
        let (tmp, store) = temp_store();
        let settings = Settings {
            version: SETTINGS_VERSION,
            preferences: Preferences {
                log_level: Some(LogLevel::Debug),
                language: LanguageSetting::Fr,
                resume_effects: false,
                log_files_kept: 30,
                theme: Theme::Light,
            },
            devices: vec![DeviceRecord {
                vid: 0x1532,
                pid: 0x0292,
                serial: Some("XY01".into()),
                state: DeviceState::Adopted,
                brightness: Some(128),
            }],
            active_effects: vec![ActiveEffectRecord {
                vid: 0x1532,
                pid: 0x0292,
                effect: "onde".into(),
            }],
            effect_params: vec![EffectParamsRecord {
                vid: 0x1532,
                pid: 0x0292,
                effect: "respiration".into(),
                values: to_map(&[("period", serde_json::json!(12.5))]),
            }],
            shipped_effects: BTreeMap::from([
                ("Breathing".to_owned(), Some("abc".to_owned())),
                ("Sweep".to_owned(), None),
            ]),
            legacy_log_level: None,
        };

        store.write_settings(&settings).unwrap();
        assert_eq!(store.read_settings().unwrap(), settings);
        assert!(
            tmp.path().join("config").join("settings.json").is_file(),
            "settings go to the configuration directory, not the data directory"
        );
    }

    #[test]
    fn a_setting_missing_from_the_file_takes_its_default() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("settings.json"),
            r#"{"devices":[{"vid":5426,"pid":658,"state":"adopted","brightness":10}]}"#,
        )
        .unwrap();

        let settings = store.read_settings().unwrap();
        assert_eq!(settings.brightness(VID, PID, None), 10);
        assert_eq!(settings.active_effect(VID, PID), None);
        assert_eq!(settings.preferences, Preferences::default());
    }

    /// **The three single-device leftovers are gone from the file.** Keeping them
    /// would have produced a `settings.json` describing one active effect, one
    /// chosen device and one brightness level, while the engine has run one per
    /// device since issue #26.
    #[test]
    fn the_file_no_longer_has_single_device_fields() {
        let (_tmp, store) = temp_store();
        let mut settings = Settings::default();
        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Adopted);
        settings.set_active_effect(VID, PID, Some("onde"));
        settings.set_brightness(VID, PID, Some("XY01"), 40);
        store.write_settings(&settings).unwrap();

        let json = serde_json::to_string(&settings).unwrap();
        for dead in [r#""activeEffect""#, r#""device":"#, r#""brightness":40,"#] {
            assert!(!json.contains(dead), "\"{dead}\" remains: {json}");
        }
        // What replaces them is there, and indexed by device.
        assert!(json.contains(r#""activeEffects":[{"vid":5426,"pid":658,"effect":"onde"}]"#));
        assert!(json.contains(r#""brightness":40"#));
    }

    /// Brightness is a decision **of the device**: two keyboards do not share a
    /// level, and that is the whole point of the move.
    #[test]
    fn brightness_is_stored_per_device() {
        let mut settings = Settings::default();
        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Adopted);
        settings.set_device_state(VID, PID + 1, Some("ZZ02"), DeviceState::Adopted);

        assert!(settings.set_brightness(VID, PID, Some("XY01"), 40));
        assert_eq!(settings.brightness(VID, PID, Some("XY01")), 40);
        assert_eq!(
            settings.brightness(VID, PID + 1, Some("ZZ02")),
            DEFAULT_BRIGHTNESS,
            "the first device's level spilled over onto the second"
        );

        // Nothing new: no file rewrite for the same value.
        assert!(!settings.set_brightness(VID, PID, Some("XY01"), 40));
    }

    /// The default is not written, and going back to the maximum is what
    /// **removes**: the counterpart of "restore the declared values" on the
    /// effects side.
    #[test]
    fn default_brightness_leaves_no_entry() {
        let mut settings = Settings::default();

        // On a device we keep nothing about: no entry is created.
        assert!(!settings.set_brightness(VID, PID, None, DEFAULT_BRIGHTNESS));
        assert!(settings.devices.is_empty());

        // Set then brought back to the maximum: the entry appears then
        // disappears, because that was all it kept.
        assert!(settings.set_brightness(VID, PID, None, 40));
        assert_eq!(settings.devices.len(), 1);
        assert!(settings.set_brightness(VID, PID, None, DEFAULT_BRIGHTNESS));
        assert!(
            settings.devices.is_empty(),
            "an entry that no longer decides anything stayed: {:?}",
            settings.devices
        );

        // But an adoption decision does keep the entry.
        settings.set_device_state(VID, PID, None, DeviceState::Ignored);
        settings.set_brightness(VID, PID, None, 40);
        settings.set_brightness(VID, PID, None, DEFAULT_BRIGHTNESS);
        assert_eq!(settings.devices.len(), 1);
        assert_eq!(settings.device_state(VID, PID, None), DeviceState::Ignored);
    }

    /// The applied effect is an indexed list, not a scalar: two keyboards carry
    /// two effects, which is exactly what the engine does.
    #[test]
    fn the_applied_effect_is_stored_per_device() {
        let mut settings = Settings::default();

        assert!(settings.set_active_effect(VID, PID, Some("onde")));
        assert!(settings.set_active_effect(VID, PID + 1, Some("respiration")));
        assert_eq!(settings.active_effect(VID, PID), Some("onde"));
        assert_eq!(settings.active_effect(VID, PID + 1), Some("respiration"));

        // Starting the same effect again does not rewrite the file: it is a
        // double-click.
        assert!(!settings.set_active_effect(VID, PID, Some("onde")));
        // Switching effects replaces the entry, it does not stack a second one.
        assert!(settings.set_active_effect(VID, PID, Some("balayage")));
        assert_eq!(settings.active_effects.len(), 2);

        // Stopping forgets, rather than leaving an id that describes nothing.
        assert!(settings.set_active_effect(VID, PID, None));
        assert_eq!(settings.active_effect(VID, PID), None);
        assert_eq!(settings.active_effects.len(), 1);
        assert!(!settings.set_active_effect(VID, PID, None));
    }

    /// The log level **survives a restart** — the trade-off chosen for whoever
    /// tracks a bug at startup — but as long as nobody has changed it, it does not
    /// appear in the file: writing the default would suggest a decision where
    /// there was none.
    #[test]
    fn log_level_is_kept_and_written_only_when_chosen() {
        let (tmp, store) = temp_store();
        let settings_path = tmp.path().join("config").join("settings.json");

        store.write_settings(&Settings::default()).unwrap();
        let written = fs::read_to_string(&settings_path).unwrap();
        assert!(
            !written.contains("logLevel"),
            "the default was written: {written}"
        );

        let settings = Settings {
            preferences: Preferences {
                log_level: Some(LogLevel::Trace),
                ..Preferences::default()
            },
            ..Settings::default()
        };
        store.write_settings(&settings).unwrap();
        assert!(fs::read_to_string(&settings_path)
            .unwrap()
            .contains(r#""logLevel": "trace""#));
        assert_eq!(
            store.read_settings().unwrap().preferences.log_level,
            Some(LogLevel::Trace)
        );
    }

    /// The language follows the system until someone chooses, and only a choice
    /// is written.
    #[test]
    fn the_language_is_written_only_when_chosen() {
        let (tmp, store) = temp_store();
        let file = tmp.path().join("config").join("settings.json");
        store.write_settings(&Settings::default()).unwrap();
        assert!(!fs::read_to_string(&file).unwrap().contains("language"));

        let mut settings = store.read_settings().unwrap();
        settings.preferences.language = LanguageSetting::En;
        store.write_settings(&settings).unwrap();
        assert!(fs::read_to_string(&file)
            .unwrap()
            .contains(r#""language": "en""#));
        assert_eq!(
            store.read_settings().unwrap().preferences.language,
            LanguageSetting::En
        );
    }

    /// On by default, so a file written before the setting existed resumes; only
    /// turning it off is written.
    #[test]
    fn resuming_effects_is_on_unless_turned_off() {
        let (tmp, store) = temp_store();
        let file = tmp.path().join("config").join("settings.json");
        store.write_settings(&Settings::default()).unwrap();
        assert!(!fs::read_to_string(&file).unwrap().contains("resumeEffects"));
        assert!(store.read_settings().unwrap().preferences.resume_effects);

        let mut settings = store.read_settings().unwrap();
        settings.preferences.resume_effects = false;
        store.write_settings(&settings).unwrap();
        assert!(fs::read_to_string(&file)
            .unwrap()
            .contains(r#""resumeEffects": false"#));
        assert!(!store.read_settings().unwrap().preferences.resume_effects);
    }

    /// **The v2.1 bridge.** `logLevel` left the root for [`Preferences`]; an
    /// earlier file must still get there, otherwise the level would drop back to
    /// the default under whoever was precisely chasing a failure.
    #[test]
    fn a_log_level_written_at_the_root_is_recovered() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("settings.json"), r#"{"logLevel":"debug"}"#).unwrap();

        let settings = store.read_settings().unwrap();
        assert_eq!(settings.preferences.log_level, Some(LogLevel::Debug));

        // And it does not go back to the root: the bridge translates once.
        store.write_settings(&settings).unwrap();
        let written = fs::read_to_string(config.join("settings.json")).unwrap();
        assert!(written.contains(r#""preferences""#), "written: {written}");
        assert_eq!(
            written.matches(r#""logLevel""#).count(),
            1,
            "the level is written twice: {written}"
        );
    }

    /// What is already in place wins over the legacy key: a file written by this
    /// version is right against a root key a text editor may have left in it.
    #[test]
    fn preferences_win_over_the_legacy_key() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("settings.json"),
            r#"{"logLevel":"debug","preferences":{"logLevel":"error"}}"#,
        )
        .unwrap();

        assert_eq!(
            store.read_settings().unwrap().preferences.log_level,
            Some(LogLevel::Error)
        );
    }

    #[test]
    fn unreadable_settings_give_a_readable_message() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join("settings.json"), "{ this is not JSON").unwrap();

        let err = store.read_settings().unwrap_err();
        assert_eq!(err.code, "settingsUnreadable");
    }

    /// **The distinction this whole module keeps**, checked where it is most
    /// costly to lose: resetting the configuration to the default does not empty
    /// the library. Whoever only wants to un-adopt a keyboard must not lose
    /// hand-written code in the process.
    #[test]
    fn reset_forgets_configuration_and_keeps_effects() {
        let (tmp, store) = temp_store();
        let js = solid_effect("00ff00");
        let id = create_and_cache(&store, "Onde", &js).id;

        let mut settings = Settings::default();
        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Adopted);
        settings.set_brightness(VID, PID, Some("XY01"), 12);
        settings.set_active_effect(VID, PID, Some(&id));
        settings.set_effect_params(VID, PID, &id, to_map(&[("speed", serde_json::json!(3))]));
        store.write_settings(&settings).unwrap();

        store.reset_settings().unwrap();

        assert_eq!(store.read_settings().unwrap(), Settings::default());
        assert!(
            tmp.path().join("config").join("settings.json").is_file(),
            "the file disappeared instead of being reset"
        );

        // And the library is intact, source included: that is what cannot be
        // reinstalled.
        assert_eq!(user_effects(&store).len(), 1);
        assert_eq!(store.effect_source(&user("Onde")).unwrap(), js);
        assert_eq!(store.effect_js(&user("Onde")).unwrap(), js);
    }

    // ------------------------------------------------------- adoption

    const VID: u16 = 0x1532;
    const PID: u16 = 0x0292;

    /// The default, and it is the heart of the decision: plugging in is not
    /// adopting.
    #[test]
    fn a_never_seen_device_is_detected_not_controlled() {
        let settings = Settings::default();
        assert_eq!(
            settings.device_state(VID, PID, Some("XY01")),
            DeviceState::Detected
        );
        assert!(settings.devices.is_empty());
    }

    #[test]
    fn a_decision_is_stored_then_changed() {
        let (_tmp, store) = temp_store();
        let mut settings = Settings::default();

        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Adopted);
        store.write_settings(&settings).unwrap();
        assert_eq!(
            store
                .read_settings()
                .unwrap()
                .device_state(VID, PID, Some("XY01")),
            DeviceState::Adopted
        );

        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Ignored);
        store.write_settings(&settings).unwrap();
        let reloaded = store.read_settings().unwrap();
        assert_eq!(
            reloaded.device_state(VID, PID, Some("XY01")),
            DeviceState::Ignored
        );
        // Changing one's mind modifies the entry, it does not stack a second one:
        // otherwise the oldest would end up answering in place of the right one.
        assert_eq!(reloaded.devices.len(), 1);
    }

    /// The serial is the identity, and this is what it is for: two identical
    /// keyboards, a single decision. Without it, adopting one would adopt the
    /// other.
    #[test]
    fn two_units_of_the_same_model_differ_by_serial() {
        let mut settings = Settings::default();
        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Adopted);

        assert_eq!(
            settings.device_state(VID, PID, Some("XY01")),
            DeviceState::Adopted
        );
        assert_eq!(
            settings.device_state(VID, PID, Some("XY02")),
            DeviceState::Detected,
            "the second unit inherited the decision made for the first"
        );

        settings.set_device_state(VID, PID, Some("XY02"), DeviceState::Ignored);
        assert_eq!(settings.devices.len(), 2);
        assert_eq!(
            settings.device_state(VID, PID, Some("XY01")),
            DeviceState::Adopted
        );
    }

    /// The other way: an enumeration that reports no serial — hidraw without a
    /// udev rule — still finds the adopted device. Making the decision again on
    /// every plug-in would be exactly the ceremony being removed.
    #[test]
    fn a_silent_enumeration_finds_the_adopted_device() {
        let mut settings = Settings::default();
        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Adopted);

        assert_eq!(settings.device_state(VID, PID, None), DeviceState::Adopted);

        // And the serial is not erased along the way, otherwise the second unit
        // would become indistinguishable from the first.
        settings.set_device_state(VID, PID, None, DeviceState::Adopted);
        assert_eq!(settings.devices[0].serial.as_deref(), Some("XY01"));
    }

    /// A decision made without a serial is completed as soon as the serial is
    /// learned, rather than leaving a broad entry next to a precise one.
    #[test]
    fn the_serial_completes_an_entry_that_had_none() {
        let mut settings = Settings::default();
        settings.set_device_state(VID, PID, None, DeviceState::Adopted);
        settings.set_device_state(VID, PID, Some("XY01"), DeviceState::Adopted);

        assert_eq!(settings.devices.len(), 1);
        assert_eq!(settings.devices[0].serial.as_deref(), Some("XY01"));
    }

    /// A file from an earlier version knows neither `devices` nor the current
    /// shape. It must reload without error, and rewriting it must not lose what
    /// was just decided.
    ///
    /// What **does not survive**, and it is the subject of issue #64: the three
    /// single-device fields. `activeEffect` and `device` were neither read nor
    /// written by anyone, and `brightness` at the root described a shared level
    /// two keyboards have no reason to have. Recovering them would have required
    /// choosing *which* device they designated — a question with no answer.
    #[test]
    fn an_older_file_reloads_and_keeps_its_settings() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("settings.json"),
            r#"{"activeEffect":"onde-radiale","brightness":90,"device":{"vid":5426,"pid":658}}"#,
        )
        .unwrap();

        let mut settings = store.read_settings().unwrap();
        assert!(settings.devices.is_empty());
        assert!(settings.active_effects.is_empty());
        settings.set_device_state(VID, PID, None, DeviceState::Adopted);
        store.write_settings(&settings).unwrap();

        let reloaded = store.read_settings().unwrap();
        assert_eq!(reloaded.device_state(VID, PID, None), DeviceState::Adopted);
        assert_eq!(reloaded.brightness(VID, PID, None), DEFAULT_BRIGHTNESS);
    }

    /// Fields go out in camelCase, like every DTO, and an entry without serial or
    /// brightness writes no empty key.
    #[test]
    fn devices_serialize_in_camel_case() {
        let mut settings = Settings::default();
        settings.set_device_state(VID, PID, None, DeviceState::Adopted);
        let json = serde_json::to_string(&settings).unwrap();

        assert!(json.contains(r#""devices":[{"vid":5426,"pid":658,"state":"adopted"}]"#));
        assert!(json.contains(r#""activeEffects":[]"#));
        assert!(!json.contains("serial"), "empty key written: {json}");
        assert!(!json.contains("brightness"), "default written: {json}");
    }

    // ------------------------------------------------- effect parameters

    /// A map of values, written the way the interface sends it.
    fn to_map(pairs: &[(&str, serde_json::Value)]) -> serde_json::Map<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    /// The heart of issue #28: switching effects then coming back loses nothing,
    /// and two devices do not step on each other.
    #[test]
    fn params_are_stored_per_device_and_per_effect() {
        let mut settings = Settings::default();
        let slow = to_map(&[("speed", serde_json::json!(0.5))]);
        let fast = to_map(&[("speed", serde_json::json!(9.0))]);

        settings.set_effect_params(VID, PID, "balayage", slow.clone());
        settings.set_effect_params(VID, PID, "respiration", fast.clone());
        // Same effect, another device: one more entry, not an overwrite.
        settings.set_effect_params(VID, PID + 1, "balayage", fast.clone());

        assert_eq!(settings.effect_params(VID, PID, "balayage"), Some(&slow));
        assert_eq!(settings.effect_params(VID, PID, "respiration"), Some(&fast));
        assert_eq!(
            settings.effect_params(VID, PID + 1, "balayage"),
            Some(&fast)
        );
        assert_eq!(settings.effect_params(VID, PID, "onde-radiale"), None);
    }

    /// Moving the same slider a hundred times does not write a hundred entries:
    /// that is exactly what a mouse drag produces.
    #[test]
    fn tuning_the_same_effect_twice_replaces_the_entry() {
        let mut settings = Settings::default();
        for i in 0..5 {
            settings.set_effect_params(VID, PID, "balayage", to_map(&[("speed", i.into())]));
        }

        assert_eq!(settings.effect_params.len(), 1);
        assert_eq!(
            settings.effect_params(VID, PID, "balayage"),
            Some(&to_map(&[("speed", serde_json::json!(4))]))
        );
    }

    /// Restoring the declared values **forgets**, instead of writing a copy of
    /// them: the effect starts again from its manifest, including when a later
    /// version changes its defaults.
    #[test]
    fn restoring_declared_values_removes_the_entry() {
        let mut settings = Settings::default();
        settings.set_effect_params(VID, PID, "balayage", to_map(&[("speed", 3.into())]));
        settings.set_effect_params(VID, PID, "balayage", serde_json::Map::new());

        assert!(settings.effect_params.is_empty());
        assert_eq!(settings.effect_params(VID, PID, "balayage"), None);

        // And forgetting what was never set does not create an empty entry.
        settings.set_effect_params(VID, PID, "onde-radiale", serde_json::Map::new());
        assert!(settings.effect_params.is_empty());
    }

    /// A file from an earlier version does not know `effectParams`. It reloads —
    /// that is what `#[serde(default)]` on the struct guarantees — and rewriting it
    /// loses neither the adoption nor the brightness.
    #[test]
    fn a_file_without_effect_params_reloads_and_accepts_them() {
        let (tmp, store) = temp_store();
        let config = tmp.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("settings.json"),
            r#"{"devices":[{"vid":5426,"pid":658,"state":"adopted","brightness":90}]}"#,
        )
        .unwrap();

        let mut settings = store.read_settings().unwrap();
        assert!(settings.effect_params.is_empty());

        let chosen = to_map(&[
            ("speed", serde_json::json!(0.5)),
            ("color", serde_json::json!({ "r": 0, "g": 180, "b": 255 })),
        ]);
        settings.set_effect_params(VID, PID, "balayage", chosen.clone());
        store.write_settings(&settings).unwrap();

        let reloaded = store.read_settings().unwrap();
        assert_eq!(reloaded.effect_params(VID, PID, "balayage"), Some(&chosen));
        assert_eq!(reloaded.brightness(VID, PID, None), 90);
        assert_eq!(reloaded.device_state(VID, PID, None), DeviceState::Adopted);
    }

    /// The four kinds of `ParamSpec` survive the disk as they are: the Rust side
    /// does not interpret them, and it must not damage them either. A color is a
    /// `{r,g,b}` object, not a string.
    #[test]
    fn the_four_kinds_of_values_round_trip() {
        let (_tmp, store) = temp_store();
        let chosen = to_map(&[
            ("speed", serde_json::json!(0.5)),
            ("bounce", serde_json::json!(true)),
            ("axis", serde_json::json!("vertical")),
            ("color", serde_json::json!({ "r": 255, "g": 96, "b": 0 })),
        ]);

        let mut settings = Settings::default();
        settings.set_effect_params(VID, PID, "balayage", chosen.clone());
        store.write_settings(&settings).unwrap();

        assert_eq!(
            store
                .read_settings()
                .unwrap()
                .effect_params(VID, PID, "balayage"),
            Some(&chosen)
        );
    }

    /// Deleting an effect takes its parameters away, on every device, and only
    /// its own. Otherwise `settings.json` would keep entries for an id nothing
    /// designates any more — and an effect reinstalled later under the same name
    /// would inherit the parameters of its namesake.
    #[test]
    fn forgetting_an_effect_removes_its_params_everywhere() {
        let mut settings = Settings::default();
        settings.set_effect_params(VID, PID, "balayage", to_map(&[("speed", 3.into())]));
        settings.set_effect_params(VID, PID + 1, "balayage", to_map(&[("speed", 9.into())]));
        settings.set_effect_params(VID, PID, "respiration", to_map(&[("period", 12.into())]));

        assert!(settings.forget_effect("balayage"));
        assert_eq!(settings.effect_params.len(), 1);
        assert_eq!(settings.effect_params(VID, PID, "balayage"), None);
        assert_eq!(settings.effect_params(VID, PID + 1, "balayage"), None);
        assert!(settings.effect_params(VID, PID, "respiration").is_some());

        // Nothing to remove: the file has no reason to be rewritten.
        assert!(!settings.forget_effect("balayage"));
        assert!(!settings.forget_effect("jamais-regle"));
    }

    /// **The trap of issue #48, settled.** Deleting the applied effect must purge
    /// its id, otherwise `settings.json` would name as applied an effect the
    /// library no longer knows — and the day the effect is resumed at startup, we
    /// would try to start an effect that is not there.
    #[test]
    fn forgetting_an_effect_also_purges_where_it_is_applied() {
        let mut settings = Settings::default();
        settings.set_active_effect(VID, PID, Some("a-supprimer"));
        settings.set_active_effect(VID, PID + 1, Some("a-supprimer"));
        settings.set_active_effect(VID, PID + 2, Some("epargne"));

        assert!(settings.forget_effect("a-supprimer"));
        assert_eq!(settings.active_effect(VID, PID), None);
        assert_eq!(settings.active_effect(VID, PID + 1), None);
        assert_eq!(
            settings.active_effect(VID, PID + 2),
            Some("epargne"),
            "the deletion took away another device's effect"
        );
    }

    /// Renaming moves an effect's references on every device, and only its own.
    #[test]
    fn renaming_an_effect_moves_its_settings_everywhere() {
        let mut settings = Settings::default();
        settings.set_active_effect(VID, PID, Some("Onde"));
        settings.set_active_effect(VID, PID + 1, Some("Autre"));
        settings.set_effect_params(VID, PID, "Onde", to_map(&[("speed", 3.into())]));

        assert!(settings.rename_effect("Onde", "Vague"));
        assert_eq!(settings.active_effect(VID, PID), Some("Vague"));
        assert_eq!(settings.active_effect(VID, PID + 1), Some("Autre"));
        assert!(settings.effect_params(VID, PID, "Vague").is_some());
        assert!(settings.effect_params(VID, PID, "Onde").is_none());

        // Nothing refers to it: the file has no reason to be rewritten.
        assert!(!settings.rename_effect("Absent", "Ailleurs"));
    }

    /// Effect parameters go out in camelCase like the rest of the DTOs.
    #[test]
    fn effect_params_serialize_in_camel_case() {
        let mut settings = Settings::default();
        settings.set_effect_params(VID, PID, "balayage", to_map(&[("speed", 3.into())]));
        let json = serde_json::to_string(&settings).unwrap();

        assert!(
            json.contains(
                r#""effectParams":[{"vid":5426,"pid":658,"effect":"balayage","values":{"speed":3}}]"#
            ),
            "serialization: {json}"
        );
    }

    // ------------------------------------------------- starting values

    /// A manifest declaring three parameters, one of them without `default`.
    fn declared_manifest() -> Manifest {
        Manifest {
            name: "Balayage".into(),
            description: empty_text(),
            params: serde_json::json!({
                "speed":  { "kind": "number",  "label": "Vitesse", "default": 120 },
                "bounce": { "kind": "boolean", "label": "Rebond",  "default": false },
                "muet":   { "kind": "number",  "label": "Sans défaut" }
            })
            .as_object()
            .unwrap()
            .clone(),
            api_version: EFFECTS_API_VERSION,
            reads_keys: false,
        }
    }

    /// **The nominal case of the tray icon**: starting an effect with no window
    /// must give the same lighting as starting it from the gallery — so the
    /// manifest defaults, overridden by what was kept.
    #[test]
    fn starting_values_come_from_the_manifest_and_are_overridden() {
        let stored = to_map(&[("speed", serde_json::json!(40))]);
        let starting = starting_params(&declared_manifest(), Some(&stored));

        assert_eq!(starting.get("speed"), Some(&serde_json::json!(40)));
        assert_eq!(starting.get("bounce"), Some(&serde_json::json!(false)));
    }

    /// With nothing kept, what the effect declares, and nothing more: a parameter
    /// without `default` is left to the effect rather than guessed.
    #[test]
    fn a_param_without_default_is_not_invented() {
        let starting = starting_params(&declared_manifest(), None);

        assert_eq!(starting.len(), 2, "starting values: {starting:?}");
        assert!(!starting.contains_key("muet"));
    }

    /// **Limited to declared parameters**, as on the window side: a value kept for
    /// a parameter the effect no longer has disappears on its own, instead of
    /// travelling to a loop that no longer reads it.
    #[test]
    fn an_orphan_setting_does_not_reach_the_loop() {
        let stored = to_map(&[
            ("speed", serde_json::json!(40)),
            ("disparu", serde_json::json!(7)),
        ]);
        let starting = starting_params(&declared_manifest(), Some(&stored));

        assert!(!starting.contains_key("disparu"));
        assert_eq!(starting.get("speed"), Some(&serde_json::json!(40)));
    }

    // ------------------------------------------------- TypeScript mirrors

    /// The mirror, read as is.
    const CANDEO_TS: &str = include_str!("../../src/api/candeo.ts");

    /// The field names of an interface in `candeo.ts`.
    ///
    /// A plain text scan, and that is enough: there is no attempt to understand
    /// TypeScript, only to pick the identifier at the start of the lines of an
    /// `export interface X { … }` block. A comment line carries none — the filter
    /// on identifier characters rules it out, including when the sentence contains
    /// a colon.
    fn ts_fields(nom: &str) -> BTreeSet<String> {
        let header = format!("export interface {nom} {{");
        let begin = CANDEO_TS
            .find(&header)
            .unwrap_or_else(|| panic!("\"{header}\" not found in src/api/candeo.ts"));
        let body = &CANDEO_TS[begin..];
        let end = body
            .find("\n}")
            .unwrap_or_else(|| panic!("interface \"{nom}\" is not closed"));
        body[..end]
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (field, _) = line.trim().split_once(':')?;
                let field = field.trim_end_matches('?');
                (!field.is_empty() && field.chars().all(|c| c.is_ascii_alphanumeric()))
                    .then(|| field.to_string())
            })
            .collect()
    }

    /// Compares the **serialized** fields of a struct with those `candeo.ts`
    /// declares for its mirror.
    ///
    /// Serialization rather than the declaration: it is what tells what actually
    /// lands in `settings.json`, `rename_all` and `skip_serializing` included.
    /// `legacy_log_level` is therefore rightly absent — it is read, never written
    /// back — and the mirror does not have to carry it.
    fn mirror(ts_name: &str, value: &impl Serialize) {
        let json = serde_json::to_value(value).expect("serialization");
        let rust: BTreeSet<String> = json
            .as_object()
            .unwrap_or_else(|| panic!("\"{ts_name}\" does not serialize to an object"))
            .keys()
            .cloned()
            .collect();
        let ts = ts_fields(ts_name);

        let missing_from_ts: Vec<&String> = rust.difference(&ts).collect();
        let missing_from_rust: Vec<&String> = ts.difference(&rust).collect();
        assert!(
            missing_from_ts.is_empty() && missing_from_rust.is_empty(),
            "\"{ts_name}\" diverged from its mirror:\n  \
             missing from src/api/candeo.ts: {missing_from_ts:?}\n  \
             missing from storage.rs: {missing_from_rust:?}"
        );
    }

    /// `Settings` is written on both sides of the IPC, and nothing ties the two
    /// together at compile time.
    ///
    /// A field added here and forgotten in `candeo.ts` does not show on read —
    /// `#[serde(default)]` fills it in — but the window that reads then rewrites
    /// the file **erases what its type does not name**. The field that nearly went
    /// that way is `logLevel`, precisely the one the log initialization depends
    /// on: the loss would have shown at the next startup, for whoever had just
    /// raised the level to understand a failure — the only moment this setting
    /// matters.
    ///
    /// Same guard as `STATE_CHANGED` and the window label: the word "mirror" is a
    /// promise, and this keeps it.
    #[test]
    fn settings_have_the_same_fields_on_both_sides() {
        // Every `Option` filled in: a field omitted by `skip_serializing_if`
        // would be missing from the comparison, and the test would let through
        // exactly what it watches for.
        let preferences = Preferences {
            log_level: Some(LogLevel::Debug),
            language: LanguageSetting::En,
            resume_effects: false,
            log_files_kept: 0,
            theme: Theme::Dark,
        };
        mirror("Preferences", &preferences);
        mirror(
            "Settings",
            &Settings {
                preferences,
                ..Settings::default()
            },
        );
        mirror(
            "DeviceRecord",
            &DeviceRecord {
                vid: 0x1532,
                pid: 0x0290,
                serial: Some("SN".into()),
                state: DeviceState::Adopted,
                brightness: Some(DEFAULT_BRIGHTNESS),
            },
        );
        mirror(
            "ActiveEffectRecord",
            &ActiveEffectRecord {
                vid: 0x1532,
                pid: 0x0290,
                effect: "onde".into(),
            },
        );
        mirror(
            "EffectParamsRecord",
            &EffectParamsRecord {
                vid: 0x1532,
                pid: 0x0290,
                effect: "onde".into(),
                values: serde_json::Map::new(),
            },
        );
    }

    /// A drift of `EFFECTS_API_VERSION` was only caught in **one** direction.
    ///
    /// A manifest announcing a version newer than the Rust side is refused at
    /// install, and that is what the comment in `candeo.ts` calls "not silent".
    /// But in the other direction nothing fires: if the Rust side moved to 2
    /// without the TypeScript, the editor would keep stamping
    /// `apiVersion: 1` on effects written against the new API, and the comparison
    /// would accept them all — 1 is indeed lower than 2. The effects would be
    /// installed under a version they do not follow, and the day that version was
    /// used to refuse something, it would refuse the wrong things.
    #[test]
    fn effects_api_version_is_the_same_on_both_sides() {
        let raw_version = CANDEO_TS
            .lines()
            .find_map(|l| l.trim().strip_prefix("export const EFFECTS_API_VERSION = "))
            .expect("\"export const EFFECTS_API_VERSION\" not found in src/api/candeo.ts")
            .trim()
            .trim_end_matches(';');
        let declared: u32 = raw_version
            .parse()
            .unwrap_or_else(|e| panic!("unreadable version \"{raw_version}\" in candeo.ts: {e}"));

        assert_eq!(
            declared, EFFECTS_API_VERSION,
            "src/api/candeo.ts announces version {declared}, storage.rs {EFFECTS_API_VERSION}"
        );
    }
}
