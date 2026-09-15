//! Bridge between the interface and the hardware.
//!
//! The types exposed to the front end are defined here rather than in the crates:
//! `candeo-protocol` and `candeo-device` thus stay free of any serde or Tauri
//! dependency, and so reusable and testable outside the application.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use candeo_device::{Inspection, Keyboard, Layout, Warning, DEATHSTALKER_V2_PRO};
use candeo_protocol::{Effect, Rgb};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use failure::Failure;
use storage::{DeviceState, Settings};

mod autostart;
mod failure;
mod hotplug;
mod i18n;
mod journal;
mod keys;
mod language;
mod paths;
mod runtime;
mod shipped;
mod single_instance;
/// Hardware probes, all `#[ignore]` — see the module.
#[cfg(test)]
mod sonde;
mod storage;
mod tray;

/// Known layouts. Only one for now.
///
/// Visible in the crate: the [`journal`] diagnostic lists the same devices as
/// [`list_devices`], and copying them over there would make a second list that
/// would diverge at the first added layout.
pub(crate) const LAYOUTS: &[&Layout] = &[&DEATHSTALKER_V2_PRO];

// ---------------------------------------------------------------- exposed types

// Fields go out in camelCase: that is the convention of the side that reads
// them. Letting Rust naming leak all the way into the interface would be a
// leaky abstraction, and it would only show at runtime.

/// Designates a device, and nothing else.
///
/// VID and PID, as adoption identifies them (issue #25): it is the key of the
/// open devices table, of the open failures table and of the render loops
/// table. The serial number tells two units of the same model apart in
/// `settings.json`, but it cannot serve as a key here — a silent enumeration
/// (hidraw without a udev rule) declares none, and the device would become
/// impossible to designate.
///
/// A type rather than two integers carried side by side: it appears as a
/// command argument **and** in the state the engine returns, and swapping them
/// would only show at runtime.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRef {
    pub vid: u16,
    pub pid: u16,
}

impl DeviceRef {
    pub(crate) fn of(layout: &Layout) -> Self {
        Self {
            vid: layout.vid,
            pid: layout.pid,
        }
    }
}

/// As it appears in a message meant to be read.
impl std::fmt::Display for DeviceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#06x}:{:#06x}", self.vid, self.pid)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub name: String,
    pub vid: u16,
    pub pid: u16,
    /// True if the device is actually plugged in.
    pub present: bool,
    /// Decision taken for this device, read back from `settings.json`.
    ///
    /// `present` says what the system sees, `state` what the user decided: the
    /// two are independent. An adopted device can be unplugged, a plugged-in
    /// device can be ignored.
    pub state: DeviceState,
    /// True if **this** device is the one open right now.
    pub open: bool,
    /// Last open failure **of this device**.
    ///
    /// Each carries its own: an opening that fails must neither stop the others
    /// from working, nor make them carry its message.
    pub error: Option<Failure>,
    /// Firmware the layout was surveyed against, `v1.5`.
    ///
    /// Known without opening anything: it is layout data. Showing it even with
    /// the device closed says what the code was established against, which is
    /// the first question when facing unexplained behavior.
    pub surveyed_firmware: String,
    /// Firmware **read** on opening.
    ///
    /// `None` when the device is not open — nothing was asked, and keeping the
    /// version from a previous opening would attribute it to the unit plugged
    /// in since — or when the read failed, which `warnings` then says.
    pub firmware: Option<String>,
    /// What the inspection on opening found that deserves to be seen.
    ///
    /// **Empty means "nothing to report", not "compatible".** The device status
    /// byte confirms that a command exists, never that its arguments are right.
    /// None of these warnings blocks anything. In the interface language.
    pub warnings: Vec<String>,
}

/// A key, as the simulator must draw it.
///
/// Two coordinate systems coexist, and they do not say the same thing:
///
/// - `row` / `col` locate the LED in the matrix, hence its rank in a frame;
/// - `x` / `y` / `w` / `h` give the physical rectangle, in keyboard pitch units
///   (1 u = one alphabetic key), origin at the top left.
///
/// The second does not follow from the first: the device declares no
/// dimension, the geometry is a manual transcription of the full-size ISO
/// layout. See `candeo_device::Key`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyInfo {
    pub index: u16,
    pub row: u8,
    pub col: u8,
    /// What the keyboard sends for this key; absent for Fn. See
    /// `candeo_device::Key::scancode`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scancode: Option<u16>,
    /// The key's name in the system's keyboard layout. See [`keys::label`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayoutInfo {
    pub name: String,
    pub rows: u8,
    pub cols: u8,
    /// Size of a frame: **all** cells, gaps included.
    pub frame_len: usize,
    /// Only the cells carrying an LED.
    pub keys: Vec<KeyInfo>,
}

impl From<&'static Layout> for LayoutInfo {
    fn from(l: &'static Layout) -> Self {
        // Walk the matrix row by row, so that `keys` comes out in index order.
        // The `candeo-device` table covers every lit position — an invariant
        // held by its `every_lit_position_has_a_key` test.
        let mut keys = Vec::with_capacity(l.lit_count());
        for row in 0..l.rows {
            for col in 0..l.cols {
                let Some(index) = l.at(row, col) else {
                    continue;
                };
                let Some(k) = l.key(index) else { continue };
                keys.push(KeyInfo {
                    index,
                    row,
                    col,
                    scancode: (k.scancode != candeo_device::NO_SCANCODE).then_some(k.scancode),
                    label: keys::label(k.scancode),
                    x: k.x,
                    y: k.y,
                    w: k.w,
                    h: k.h,
                });
            }
        }
        Self {
            name: l.name.to_string(),
            rows: l.rows,
            cols: l.cols,
            frame_len: l.led_count(),
            keys,
        }
    }
}

/// Effect, as named on the interface side.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EffectDto {
    Off,
    SpectrumCycle,
    Wave { direction: u8, speed: u8 },
    Custom,
}

impl From<EffectDto> for Effect {
    fn from(e: EffectDto) -> Self {
        match e {
            EffectDto::Off => Effect::Off,
            EffectDto::SpectrumCycle => Effect::SpectrumCycle,
            EffectDto::Wave { direction, speed } => Effect::Wave { direction, speed },
            EffectDto::Custom => Effect::Custom,
        }
    }
}

// ---------------------------------------------------------------- state

/// Everything the application holds open, **device by device**.
///
/// # Lock ordering
///
/// A deadlock has already been caught here: `list_devices` took the keyboard
/// lock then the failures lock, `ignore_device` the reverse. With a device
/// table and one render loop per device, the rule is therefore explicit and
/// held:
///
/// > **No code holds two of these locks at the same time.** A table is locked
/// > for as long as it takes to read or put an `Arc` in it, never for an HID
/// > write, a loop start or a wait for completion.
///
/// This is what [`AppState::handle`] and [`AppState::open_devices`] do: they
/// clone under the table lock and release it before querying anything. If two
/// locks became unavoidable, the order is that of the declaration below —
/// `devices`, then `engine`, then `failures`, then a device's handle, then a
/// loop's shared state. The render thread only knows the last two: it has no
/// way to take an application lock, hence no way to block a command with one.
#[derive(Default)]
pub struct AppState {
    /// The devices, **one per handle**.
    ///
    /// An entry appears as soon as a device is targeted and is never removed;
    /// its content says whether it is open. Closing sets the `Option` to `None`
    /// without touching the `Arc`: the loop holding a copy notices at the next
    /// frame and stops writing, instead of carrying on with a handle nobody
    /// looks at anymore.
    pub(crate) devices: Mutex<HashMap<DeviceRef, runtime::Handle>>,
    /// The render loops, one per device. See [`runtime`].
    pub(crate) engine: runtime::Engine,
    /// Last open failure, **per device**.
    ///
    /// A table rather than a single field: that is what keeps a failing device
    /// from dragging down any other. A global message would force choosing
    /// which one to show, and the next one would erase the previous one.
    pub(crate) failures: Mutex<HashMap<DeviceRef, Failure>>,
}

impl AppState {
    /// This device's handle, created closed if it did not exist.
    ///
    /// The table lock is held only for the clone: whatever is done with the
    /// handle next — an HID write, a loop starting — must hold back no command
    /// targeting another device.
    pub(crate) fn handle(&self, device: DeviceRef) -> runtime::Handle {
        Arc::clone(self.devices.lock().unwrap().entry(device).or_default())
    }

    /// This device's handle, **without creating one**.
    ///
    /// Commands that require an open device go through here: targeting an
    /// unknown device must be a refusal, not one more row in the table.
    fn opened(&self, device: DeviceRef) -> Option<runtime::Handle> {
        self.devices.lock().unwrap().get(&device).map(Arc::clone)
    }

    /// What this device said about itself on opening — **if it is open**.
    ///
    /// A copy, taken under the handle lock alone and released at once: the
    /// inspection was done once, on opening, and reading it back here costs no
    /// USB exchange. That is what lets the device list and the diagnostic show
    /// it without ever touching the render loop.
    pub(crate) fn inspection(&self, device: DeviceRef) -> Option<Inspection> {
        let handle = self.opened(device)?;
        let guard = handle.lock().unwrap();
        guard.as_ref().map(|kb| kb.inspection().clone())
    }

    /// Opens — or closes, with `None` — this device.
    fn set_open(&self, device: DeviceRef, keyboard: Option<Keyboard>) {
        *self.handle(device).lock().unwrap() = keyboard;
    }

    /// The devices actually open right now.
    ///
    /// Two steps, and never both locks together: copy the handles under the
    /// table lock, release it, then query them. Querying them in place would
    /// block the whole table during a loop's HID write.
    pub(crate) fn open_devices(&self) -> HashSet<DeviceRef> {
        let handles: Vec<(DeviceRef, runtime::Handle)> = self
            .devices
            .lock()
            .unwrap()
            .iter()
            .map(|(d, h)| (*d, Arc::clone(h)))
            .collect();

        handles
            .into_iter()
            .filter(|(_, h)| h.lock().unwrap().is_some())
            .map(|(d, _)| d)
            .collect()
    }
}

/// Layout used when no device is connected.
///
/// Serves the engine and the simulator: writing an effect must not require
/// owning the keyboard.
pub(crate) fn default_layout() -> &'static Layout {
    LAYOUTS[0]
}

/// Errors go up to the front end as a code, which it translates: see
/// [`Failure`].
type CmdResult<T> = Result<T, Failure>;

pub(crate) fn hid() -> CmdResult<hidapi::HidApi> {
    hidapi::HidApi::new().map_err(|e| Failure::unexpected(format!("HID not initialised: {e}")))
}

fn find_layout(device: DeviceRef) -> CmdResult<&'static Layout> {
    LAYOUTS
        .iter()
        .copied()
        .find(|l| DeviceRef::of(l) == device)
        .ok_or_else(|| Failure::new("noLayout").with("device", device))
}

/// Serial number of the plugged-in unit — if one is plugged in.
///
/// The two levels say two different things and must not be confused: `None`
/// means **unplugged**, `Some(None)` **plugged in with no declared serial**.
/// The second is not a degenerate case — it is what hidraw returns on Linux
/// when the udev rule does not grant reading the attributes.
///
/// ⚠️ **The DeathStalker USB descriptor carries none**, on any interface. The
/// serial that truly matches comes from the protocol, on opening: see
/// [`known_serial`].
pub(crate) fn plugged(api: &hidapi::HidApi, layout: &Layout) -> Option<Option<String>> {
    api.device_list()
        .find(|d| layout.is_lighting_interface(d.vendor_id(), d.product_id(), d.interface_number()))
        .map(|d| {
            d.serial_number()
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
        })
}

/// The unit's serial: **the protocol one first**, the USB descriptor one
/// otherwise.
///
/// The protocol (`0x00`/`0x82`) is the only source that gives one on this
/// hardware, but it can only be read with the device open — and an ignored or
/// detected device is not opened just to ask it. The descriptor therefore
/// stays the fallback, and [`storage::DeviceRecord::matches`] tolerates it
/// being silent.
pub(crate) fn known_serial(inspection: Option<&Inspection>, usb: Option<String>) -> Option<String> {
    inspection.and_then(|i| i.serial.clone().ok()).or(usb)
}

/// Logs an opening, and what the inspection found in it.
///
/// One `info` line for the opening itself — the firmware is in it, it is the
/// first field anyone will look for — then one `warn` line per warning. **On
/// opening and only then**: it is a transition, and the inspection is never
/// redone.
///
/// The serial's fingerprint, never the serial: it identifies a specific unit,
/// and a log ends up pasted into a bug report.
fn log_opening(device: DeviceRef, keyboard: &Keyboard, usb: Option<String>, what: &str) {
    let inspection = keyboard.inspection();
    let serial = known_serial(Some(inspection), usb);
    tracing::info!(
        device = %device,
        serial = journal::fingerprint_of(serial.as_deref()),
        firmware = %inspection
            .firmware
            .as_ref()
            .map_or_else(|e| format!("not read ({e})"), ToString::to_string),
        "{what}"
    );
    for warning in inspection.warnings(keyboard.layout()) {
        tracing::warn!(device = %device, "{warning}");
    }
}

// ---------------------------------------------------------------- adoption

/// Asked about the serial a device reads on opening: `Some(reason)` refuses the
/// unit. See [`open_adopted`].
type Judge<'a> = &'a dyn Fn(Option<&str>) -> Option<Failure>;

/// What the attempt to open **one** device produced.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OpenOutcome {
    pub device: DeviceRef,
    pub error: Option<Failure>,
}

/// Opens **all** controlled and present devices, one by one.
///
/// This is the startup loop, written without Tauri or HID — presence and
/// opening come in as arguments — so that its invariant can be checked by an
/// ordinary test: *one device's failure drags down no other*. Each attempt
/// produces its own report line, and an error does not interrupt the loop.
///
/// All of them, and no longer a single one: `AppState` now carries a table of
/// open devices (issue #26). The second controlled device is therefore no
/// longer left closed for lack of room.
///
/// # The serial is checked on opening, before anything is written
///
/// The USB descriptor carries no serial on this hardware: before opening, the
/// decision can only match on VID and PID, and any unit of the model passes.
/// `open` therefore receives a judge: the device reads its serial over the
/// protocol and asks it, **before** the inspection rewrites anything. A unit the
/// decision does not cover is closed having received reads only, and the
/// attempt reports why (#74). Otherwise plugging in a colleague's keyboard —
/// same model — would have it controlled, or at least written to, on behalf of
/// a decision taken for another.
fn open_adopted<K>(
    layouts: &[&'static Layout],
    settings: &Settings,
    present: impl Fn(&Layout) -> Option<Option<String>>,
    mut open: impl FnMut(&'static Layout, Judge<'_>) -> Result<K, Failure>,
) -> (Vec<(&'static Layout, K)>, Vec<OpenOutcome>) {
    let mut opened = Vec::new();
    let mut outcomes = Vec::new();

    for layout in layouts {
        // Unplugged: nothing to open, and nothing to report either.
        let Some(serial) = present(layout) else {
            continue;
        };
        if settings.device_state(layout.vid, layout.pid, serial.as_deref()) != DeviceState::Adopted
        {
            continue;
        }
        let judge = |serial: Option<&str>| wrong_unit(settings, layout, serial);
        let error = match open(layout, &judge) {
            Ok(k) => {
                opened.push((*layout, k));
                None
            }
            Err(e) => Some(e),
        };
        outcomes.push(OpenOutcome {
            device: DeviceRef::of(layout),
            error,
        });
    }

    (opened, outcomes)
}

/// `Some(reason)` if the opened unit is not the one the decision designates.
///
/// Without a read serial, nothing is concluded: a device that does not answer
/// reads stays matched as before, on its VID and PID. Refusing over a question
/// left unanswered would turn off the lighting of someone who has only one
/// unit.
fn wrong_unit(settings: &Settings, layout: &Layout, serial: Option<&str>) -> Option<Failure> {
    let serial = serial?;
    (settings.device_state(layout.vid, layout.pid, Some(serial)) != DeviceState::Adopted)
        .then(|| Failure::new("wrongUnit").with("fingerprint", journal::fingerprint(serial)))
}

/// Brings the effect library up to date at startup — see
/// [`storage::Store::migrate`] — and copies the shipped effects.
///
/// A failure is logged and the startup goes on: the effects it did not reach are
/// only missing from the library until the next launch, and the window is what
/// lets someone look.
fn migrate_effects(app: &AppHandle) -> BTreeMap<String, String> {
    match storage::store(app).and_then(|store| store.migrate(&shipped::ALL)) {
        Ok((renames, seeding)) => {
            if !renames.is_empty() {
                tracing::info!(effects = renames.len(), "effect references moved to keys");
            }
            if seeding != storage::Seeding::default() {
                tracing::info!(
                    copied = seeding.copied,
                    updated = seeding.updated,
                    "shipped effects copied"
                );
            }
            renames
        }
        Err(e) => {
            tracing::warn!("effects not migrated: {e}");
            BTreeMap::new()
        }
    }
}

/// What [`apply_adoptions`] did.
#[derive(Default)]
struct Adoptions {
    opened: Vec<DeviceRef>,
    /// An adopted device is plugged in but did not open, for a reason that may
    /// pass: not ready yet, not permitted yet. Another unit of the model is not
    /// one.
    retry: bool,
}

/// Applies the stored decisions: at application startup, and when a device is
/// plugged in (#81). Devices in `already_open` are left alone.
///
/// Cannot fail: unreadable settings or a missing HID must not stop the window
/// from opening — it is what would let the situation be fixed.
fn apply_adoptions(
    app: &AppHandle,
    state: &AppState,
    already_open: &HashSet<DeviceRef>,
) -> Adoptions {
    let (store, settings) = match storage::store(app).and_then(|s| {
        let settings = s.read_settings()?;
        Ok((s, settings))
    }) {
        Ok(loaded) => loaded,
        Err(e) => {
            tracing::error!("adoption abandoned, no device opened: {e}");
            return Adoptions::default();
        }
    };
    let api = match hid() {
        Ok(api) => api,
        Err(e) => {
            tracing::error!("adoption abandoned, no device opened: {e}");
            return Adoptions::default();
        }
    };

    let layouts: Vec<&'static Layout> = LAYOUTS
        .iter()
        .copied()
        .filter(|l| !already_open.contains(&DeviceRef::of(l)))
        .collect();
    let (newly_opened, outcomes) = open_adopted(
        &layouts,
        &settings,
        |l| plugged(&api, l),
        |l, judge| {
            let mut refusal = None;
            let opened = Keyboard::open_if(&api, l, |serial| {
                refusal = judge(serial);
                refusal.is_none()
            })?;
            opened.ok_or_else(|| refusal.expect("a refused unit has a reason"))
        },
    );

    // What the openings learn, to be stored once the loop is done.
    let mut learned = settings.clone();
    let mut done = Adoptions::default();

    // Handles first, failures next: two tables, never locked together.
    for (layout, keyboard) in newly_opened {
        let device = DeviceRef::of(layout);
        done.opened.push(device);
        // The second `plugged` enumerates nothing again — it reads back the
        // list `api` already holds — and it avoids making [`OpenOutcome`]
        // carry the serial, which reports an attempt and has no business
        // describing the device.
        let usb = plugged(&api, layout).flatten();
        let serial = known_serial(Some(keyboard.inspection()), usb.clone());
        log_opening(device, &keyboard, usb, "adopted device opened");
        // A decision taken without a serial **is completed** as soon as it is
        // known: that is the rule of [`storage::Settings::set_device_state`].
        // Without it, an adoption older than protocol reads would stay matched
        // on the model alone, and the first unit to come along would always
        // pass.
        learned.set_device_state(
            layout.vid,
            layout.pid,
            serial.as_deref(),
            DeviceState::Adopted,
        );
        // **Before** handing over the handle: brightness is reapplied on the
        // keyboard just opened, while we still hold it.
        reapply_brightness(&keyboard, &settings, layout, serial.as_deref());
        state.set_open(device, Some(keyboard));
    }

    // Only if something was learned: rewriting an unchanged file at every
    // startup would only multiply the chances of truncating it.
    if learned != settings {
        match store.write_settings(&learned) {
            Ok(()) => tracing::info!("serial read over the protocol, stored for matching"),
            // `warn`: the device is open and works. Only the distinction
            // between two units is lost, until the next startup.
            Err(e) => tracing::warn!("serial read but not stored: {e}"),
        }
    }

    let mut failures = state.failures.lock().unwrap();
    for outcome in outcomes {
        match outcome.error {
            Some(e) => {
                // `error` and not `warn`: an adopted device that does not open
                // means the user's lighting will not come on.
                tracing::error!(device = %outcome.device, "adopted device not opened: {e}");
                done.retry |= e.code != "wrongUnit";
                failures.insert(outcome.device, e);
            }
            None => {
                failures.remove(&outcome.device);
            }
        }
    }
    done
}

/// Brings the open devices in line with what is plugged in (#81): a device that
/// left is closed, an adopted one that came back is opened — its serial checked,
/// its brightness and applied effect given back — and the window and the tray
/// are told.
///
/// A loop still running on a device that left keeps running, writing nowhere:
/// the handle it shares is filled again on replug, so the effect comes back
/// without restarting.
///
/// Returns true when an adopted device is plugged in but did not open for a
/// reason that may pass: [`hotplug`] tries again.
pub(crate) fn reconcile_devices(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let api = match hid() {
        Ok(api) => api,
        Err(e) => {
            tracing::warn!("devices not reconciled: {e}");
            return false;
        }
    };
    let open = state.open_devices();
    let mut changed = false;
    for layout in LAYOUTS {
        let device = DeviceRef::of(layout);
        if open.contains(&device) && plugged(&api, layout).is_none() {
            state.set_open(device, None);
            state.failures.lock().unwrap().remove(&device);
            tracing::info!(device = %device, "device unplugged, closed");
            changed = true;
        }
    }

    let adoptions = apply_adoptions(app, &state, &state.open_devices());
    for device in &adoptions.opened {
        runtime::resume_applied(app, *device);
    }
    // A failed open changes what Devices shows too.
    if changed || !adoptions.opened.is_empty() || adoptions.retry {
        tray::refresh(app);
        tray::notify_state_changed(app);
    }
    adoptions.retry
}

/// Puts back on the keyboard the brightness stored for it.
///
/// **A stored level that is not reapplied on plug-in is useless**: it is the
/// whole reason for storing it. The surveyed protocol can write brightness, not
/// read it back — without this step, a replugged keyboard restarts at whatever
/// its firmware kept, and the setting in `settings.json` describes a state
/// that nothing produces.
///
/// Nothing is rewritten when it is the default: the keyboard is already there,
/// and one more HID write at each device's startup would buy nothing.
///
/// Nothing goes up: a refused write must not stop the adoption from
/// completing — the device is open, the effect can run, and that is what
/// matters. It is logged, because a keyboard dimmer than requested without a
/// word anywhere is exactly the kind of gap that takes hours to track down.
fn reapply_brightness(
    keyboard: &Keyboard,
    settings: &Settings,
    layout: &Layout,
    serial: Option<&str>,
) {
    let level = settings.brightness(layout.vid, layout.pid, serial);
    if level == storage::DEFAULT_BRIGHTNESS {
        return;
    }
    match keyboard.set_brightness(level) {
        Ok(()) => tracing::info!(
            device = %DeviceRef::of(layout),
            level,
            "stored brightness reapplied"
        ),
        Err(e) => tracing::warn!(
            device = %DeviceRef::of(layout),
            level,
            "stored brightness not reapplied: {e}"
        ),
    }
}

/// An inspection warning in the interface language: the device crate knows no
/// catalog.
fn warning_text(language: language::Language, warning: &Warning) -> String {
    let command = |name: &str| i18n::text(language, &format!("devices.warnings.commands.{name}"));
    let (key, params) = match warning {
        Warning::FirmwareDiffers { read, surveyed } => (
            "devices.warnings.firmwareDiffers",
            BTreeMap::from([
                ("read", read.to_string()),
                ("surveyed", surveyed.to_string()),
            ]),
        ),
        Warning::FirmwareNotRead { reason, surveyed } => (
            "devices.warnings.firmwareNotRead",
            BTreeMap::from([
                ("reason", reason.clone()),
                ("surveyed", surveyed.to_string()),
            ]),
        ),
        Warning::Unsupported { name, command: id } => (
            "devices.warnings.unsupported",
            BTreeMap::from([("name", command(name)), ("command", id.to_string())]),
        ),
        Warning::ReadBackDiffers {
            name,
            command: id,
            wrote,
            read,
        } => (
            "devices.warnings.readBackDiffers",
            BTreeMap::from([
                ("name", command(name)),
                ("command", id.to_string()),
                ("wrote", wrote.clone()),
                ("read", read.clone()),
            ]),
        ),
    };
    i18n::t(language, key, &params)
}

// ---------------------------------------------------------------- commands

/// Lists the known layouts: plugged in or not, and above all **in which state**.
#[tauri::command]
fn list_devices(app: AppHandle, state: State<'_, AppState>) -> CmdResult<Vec<DeviceInfo>> {
    let settings = storage::store(&app)?.read_settings()?;
    let api = hid()?;
    let language = language::current(&app);
    // The readings are taken one after another, each releasing its lock before
    // the next. That is the rule documented on [`AppState`], and it comes from a
    // real deadlock: this command took the keyboard then the failures,
    // `ignore_device` the reverse, and the two blocked each other.
    let failures = state.failures.lock().unwrap().clone();

    Ok(LAYOUTS
        .iter()
        .map(|l| {
            let device = DeviceRef::of(l);
            let plugged_in = plugged(&api, l);
            let present = plugged_in.is_some();
            // Read back from the handle, with no USB exchange: `None` means
            // closed.
            let inspection = state.inspection(device);
            let serial = known_serial(inspection.as_ref(), plugged_in.flatten());
            DeviceInfo {
                name: l.name.to_string(),
                vid: l.vid,
                pid: l.pid,
                present,
                state: settings.device_state(l.vid, l.pid, serial.as_deref()),
                open: inspection.is_some(),
                error: failures.get(&device).cloned(),
                surveyed_firmware: l.surveyed_firmware.to_string(),
                firmware: inspection
                    .as_ref()
                    .and_then(|i| i.firmware.as_ref().ok())
                    .map(ToString::to_string),
                warnings: inspection
                    .map(|i| {
                        i.warnings(l)
                            .iter()
                            .map(|w| warning_text(language, w))
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
        .collect())
}

/// Stores "piloté" (controlled) for this device, and opens it if it is there.
///
/// The decision is written **whatever the outcome of the opening**: it is a
/// decision, not the report of an attempt. The next startup will replay it,
/// which is precisely what we want from a keyboard that a hub has not finished
/// enumerating yet.
///
/// # Opening comes before writing, and it is for the serial
///
/// The USB descriptor carries none: only opening reads it, over the protocol.
/// Writing the decision first would store it without a serial — hence for
/// **every** unit of the model — and adopting a second identical keyboard
/// would land on the first one's entry instead of creating one. If the write
/// then fails, the handle is dropped with the function: nothing is left open
/// without a decision to justify it.
///
/// Returns the layout when the device was opened, `None` when it is adopted but
/// unplugged — that is not an error, it will be opened the next time it is
/// plugged in.
#[tauri::command]
fn adopt_device(
    app: AppHandle,
    state: State<'_, AppState>,
    vid: u16,
    pid: u16,
) -> CmdResult<Option<LayoutInfo>> {
    let device = DeviceRef { vid, pid };
    let layout = find_layout(device)?;

    // Already open: its inspection says which unit it is. Opening it again would
    // inspect it on a second handle while its loop writes on the first, and the
    // replies could cross (#74).
    if let Some(inspection) = state.inspection(device) {
        let store = storage::store(&app)?;
        let mut settings = store.read_settings()?;
        let serial = known_serial(Some(&inspection), None);
        settings.set_device_state(vid, pid, serial.as_deref(), DeviceState::Adopted);
        store.write_settings(&settings)?;
        return Ok(Some(LayoutInfo::from(layout)));
    }

    let api = hid()?;
    let plugged_in = plugged(&api, layout);
    let opening = plugged_in.is_some().then(|| Keyboard::open(&api, layout));
    let usb = plugged_in.flatten();
    let serial = known_serial(
        opening
            .as_ref()
            .and_then(|o| o.as_ref().ok())
            .map(Keyboard::inspection),
        usb.clone(),
    );

    let store = storage::store(&app)?;
    let mut settings = store.read_settings()?;
    settings.set_device_state(vid, pid, serial.as_deref(), DeviceState::Adopted);
    store.write_settings(&settings)?;

    let Some(opening) = opening else {
        return Ok(None);
    };
    match opening {
        Ok(kb) => {
            log_opening(device, &kb, usb, "device controlled");
            // As at startup: what was stored for this keyboard takes effect the
            // moment it is opened, not at the next launch.
            reapply_brightness(&kb, &settings, layout, serial.as_deref());
            // The other open devices stay open: adopting this one is not a
            // choice made in place of the others.
            state.set_open(device, Some(kb));
            state.failures.lock().unwrap().remove(&device);
            runtime::resume_applied(&app, device);
            Ok(Some(LayoutInfo::from(layout)))
        }
        Err(e) => {
            let failure = Failure::from(e);
            tracing::error!(device = %device, "device not opened: {failure}");
            state
                .failures
                .lock()
                .unwrap()
                .insert(device, failure.clone());
            Err(failure)
        }
    }
}

/// Stores "ignoré" (ignored), and closes the device if it was open.
///
/// Does not go through HID: ignoring a device must stay possible when HID
/// access is precisely what is failing. The serial is therefore only picked up
/// if it is available — [`storage::DeviceRecord::matches`] finds the entry
/// without it.
///
/// The protocol one, when the device is open, is read back **before** closing
/// it: it is the only one that tells the unit apart, and it goes away with the
/// handle.
#[tauri::command]
fn ignore_device(app: AppHandle, state: State<'_, AppState>, vid: u16, pid: u16) -> CmdResult<()> {
    let device = DeviceRef { vid, pid };
    let layout = find_layout(device)?;
    let serial = known_serial(
        state.inspection(device).as_ref(),
        hid().ok().and_then(|api| plugged(&api, layout)).flatten(),
    );

    let store = storage::store(&app)?;
    let mut settings = store.read_settings()?;
    settings.set_device_state(vid, pid, serial.as_deref(), DeviceState::Ignored);
    store.write_settings(&settings)?;

    tracing::info!(device = %device, "device ignored and closed");

    // "Left alone": we do not keep open what we commit to no longer touching.
    // The handle is emptied, not removed: the loop that fed it holds a copy,
    // notices at the next frame and stops writing — without the other devices
    // being touched.
    state.set_open(device, None);
    state.failures.lock().unwrap().remove(&device);
    Ok(())
}

/// Brings **all** devices back to a known state: nothing running, nothing lit,
/// nothing open.
///
/// The counterpart of [`apply_adoptions`], and what lets
/// [`storage::reset_settings`] keep its promise: after it, no loop or handle
/// depends any longer on decisions the file no longer carries.
///
/// Three steps, and the order matters:
///
/// 1. the loops stop, and the stop is **awaited** — the next frame would light
///    up again what we are about to turn off;
/// 2. the backlight turns off. Stopping a loop leaves the keyboard on its last
///    frame, and a frozen frame looks like an effect still running;
///    `Effect::Off` leaves the device in a readable state, at no cost — the
///    firmware runs it;
/// 3. the handles are emptied, as [`ignore_device`] does: we do not keep open a
///    device that no decision designates anymore.
///
/// Open failures go too: they reported attempts made for adoptions that no
/// longer exist, and keeping them would show a red error on a device nobody has
/// asked anything of anymore.
///
/// Nothing goes up, and that is deliberate: a keyboard that refuses to turn
/// off — unplugged in the meantime, access lost — must not stop the reset.
/// Turning off is a nicety, not the point.
pub(crate) fn release_devices(state: &AppState) {
    state.engine.stop_all();

    for device in state.open_devices() {
        let _ = with_keyboard(state, device, |kb| {
            kb.set_effect(Effect::Off).map_err(Failure::from)
        });
        state.set_open(device, None);
    }

    state.failures.lock().unwrap().clear();
}

/// One-off opening, deciding nothing.
///
/// Distinct from [`adopt_device`]: it does not touch `settings.json`, so it
/// does not survive a restart. That is what we want to try a device without
/// committing.
///
/// No caller from any screen, like [`disconnect`] and for the same reason: the
/// interface today only offers adopting or ignoring, that is, the two actions
/// that **decide**. The one that commits to nothing does not have its button
/// yet — and it is the pair that will have to be wired together, not one of
/// the two.
#[tauri::command]
fn connect(state: State<'_, AppState>, vid: u16, pid: u16) -> CmdResult<LayoutInfo> {
    let device = DeviceRef { vid, pid };
    let layout = find_layout(device)?;
    // Already open: not a second handle beside its loop, see [`adopt_device`].
    if state.inspection(device).is_some() {
        return Ok(LayoutInfo::from(layout));
    }

    let api = hid()?;
    let kb = Keyboard::open(&api, layout)?;
    log_opening(
        device,
        &kb,
        plugged(&api, layout).flatten(),
        "device opened, no decision saved",
    );
    state.set_open(device, Some(kb));
    state.failures.lock().unwrap().remove(&device);
    Ok(LayoutInfo::from(layout))
}

/// Closes **one** device. The others are not touched.
///
/// No screen calls it today — `useDevice` wraps it, nobody destructures the
/// wrapper. It stays because it is the only action that **closes without
/// deciding**: `ignore_device` writes to `settings.json` and holds for the
/// next times, this one releases the handle and nothing else. What is missing
/// is a button, not a command.
#[tauri::command]
fn disconnect(state: State<'_, AppState>, device: DeviceRef) {
    state.set_open(device, None);
}

/// Layout used as a fallback when nothing is connected.
///
/// `get_layout` refuses when disconnected, and that is exactly the case to
/// serve: we draw the keyboard and write an effect **before** plugging
/// anything in, or without owning the keyboard.
///
/// Without this command, the interface would have no choice but to copy the
/// geometry — a second source of truth that would silently diverge.
#[tauri::command]
fn get_default_layout() -> LayoutInfo {
    LayoutInfo::from(default_layout())
}

/// Acts on a device's handle, **if** it is open.
///
/// A single lock is held, the handle's, and never a table's: an HID write
/// takes a few milliseconds and must hold back no command targeting another
/// device.
fn with_keyboard<T>(
    state: &AppState,
    device: DeviceRef,
    f: impl FnOnce(&Keyboard) -> CmdResult<T>,
) -> CmdResult<T> {
    let not_open = || Failure::new("deviceNotOpen").with("device", device);
    let handle = state.opened(device).ok_or_else(not_open)?;
    let guard = handle.lock().unwrap();
    let kb = guard.as_ref().ok_or_else(not_open)?;
    f(kb)
}

/// Layout of an open device.
#[tauri::command]
fn get_layout(state: State<'_, AppState>, device: DeviceRef) -> CmdResult<LayoutInfo> {
    with_keyboard(&state, device, |kb| Ok(LayoutInfo::from(kb.layout())))
}

/// Writes the brightness **to the keyboard**, and nothing else.
///
/// Does not touch `settings.json`: that is [`remember_brightness`]. The two are
/// separate just as `set_effect_params` and `remember_effect_params` are, and
/// for the same reason — they have neither the same rate nor the same
/// destination. A slider being dragged produces dozens of HID writes per
/// second, and a single disk write, when it stops.
#[tauri::command]
fn set_brightness(state: State<'_, AppState>, device: DeviceRef, level: u8) -> CmdResult<()> {
    with_keyboard(&state, device, |kb| Ok(kb.set_brightness(level)?))
}

/// Stores this device's brightness, without touching the keyboard.
///
/// The disk counterpart of [`set_brightness`]. As with
/// `remember_effect_params`, reading, modifying and writing happen here in one
/// go: sending the whole file back from the window would overwrite an adoption
/// decided in the meantime.
///
/// The serial is picked up **if it is available**, as [`ignore_device`] does:
/// it lands the level on the right unit's entry when there are two of the same
/// model, and its absence blocks nothing —
/// [`storage::DeviceRecord::matches`] finds the entry without it.
///
/// Bringing the slider back to the maximum **removes** the entry rather than
/// writing 255: see [`storage::Settings::set_brightness`].
#[tauri::command]
fn remember_brightness(
    app: AppHandle,
    state: State<'_, AppState>,
    device: DeviceRef,
    level: u8,
) -> CmdResult<()> {
    let layout = find_layout(device)?;
    let serial = known_serial(
        state.inspection(device).as_ref(),
        hid().ok().and_then(|api| plugged(&api, layout)).flatten(),
    );

    let store = storage::store(&app)?;
    let mut settings = store.read_settings()?;
    // Nothing new: the file is not rewritten. A slider moved and then brought
    // back comes through here.
    if !settings.set_brightness(device.vid, device.pid, serial.as_deref(), level) {
        return Ok(());
    }
    store.write_settings(&settings)
}

#[tauri::command]
fn set_effect(state: State<'_, AppState>, device: DeviceRef, effect: EffectDto) -> CmdResult<()> {
    with_keyboard(&state, device, |kb| Ok(kb.set_effect(effect.into())?))
}

/// Pushes a complete frame.
///
/// `frame` is a flat sequence of RGB triplets and must cover **all** the
/// matrix cells. Sending fewer leaves the last rows frozen on their previous
/// value — that is the classic trap of this hardware.
///
/// # Exposed without a caller, and deliberately
///
/// The window does not send frames: the [`crate::runtime`] loop produces and
/// writes them, and the preview **receives** them over a channel instead of
/// pushing them. No screen will therefore call this one as long as the
/// architecture stays that way.
///
/// It remains the only path that puts a precise frame on the keyboard
/// **without the effects engine** — which served to establish that the frame
/// must cover 132 cells and not 106, and will serve again the day a device
/// answers differently. Same reason as the `pub` items of `candeo-protocol`:
/// what made the survey possible stays able to redo it.
#[tauri::command]
fn present(state: State<'_, AppState>, device: DeviceRef, frame: Vec<u8>) -> CmdResult<()> {
    with_keyboard(&state, device, |kb| {
        let expected = kb.layout().led_count();
        if frame.len() != expected * 3 {
            return Err(Failure::unexpected(format!(
                "frame of {} bytes, {} expected ({expected} cells × 3)",
                frame.len(),
                expected * 3
            )));
        }
        let colors: Vec<Rgb> = frame
            .chunks_exact(3)
            .map(|c| Rgb::new(c[0], c[1], c[2]))
            .collect();
        Ok(kb.present(&colors)?)
    })
}

/// Writes a row segment, without touching the rest.
///
/// Also without a caller, and kept for a named reason: partial writing is
/// **the** lead if the frame rate must go above 30 — sending only the rows
/// that change, instead of all six on every frame. The `runtime::FPS` comment
/// names it as such. This is where that lead is checked on the hardware before
/// being written into the loop; removing it would mean removing the measuring
/// tool before the measurement.
#[tauri::command]
fn write_row(
    state: State<'_, AppState>,
    device: DeviceRef,
    row: u8,
    col_start: u8,
    colors: Vec<u8>,
) -> CmdResult<()> {
    with_keyboard(&state, device, |kb| {
        if colors.len() % 3 != 0 || colors.is_empty() {
            return Err(Failure::unexpected(
                "colors must be a non-empty list of RGB triplets",
            ));
        }
        let c: Vec<Rgb> = colors
            .chunks_exact(3)
            .map(|x| Rgb::new(x[0], x[1], x[2]))
            .collect();
        Ok(kb.write_row(row, col_start, &c)?)
    })
}

// ---------------------------------------------------------------- entry point

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // First, and the order is not decorative: this is where the second
        // process stops, and it must do so before touching the keyboard. See
        // [`single_instance`].
        .plugin(single_instance::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // **First of all**, and before the store is resolved: a failure to
            // resolve the configuration folder is exactly what we want to see,
            // and it would otherwise happen before there is anywhere to write
            // it. It is also the first moment `app_log_dir()` exists —
            // everything before it, plugin registration, does not log. The
            // stored level is read back right after: start at the default then
            // adjust, never the reverse.
            journal::init(app.handle());
            journal::reload_level_setting(app.handle());

            // Before anything reads an effect from the library or from
            // `settings.json`: the tray lists them, and the window reads both.
            app.manage(storage::MigratedEffects(Mutex::new(migrate_effects(
                app.handle(),
            ))));

            let state = AppState::default();
            // Before `manage`: the state is afterwards only reachable through
            // the manager, and adoption needs nothing but the state.
            apply_adoptions(app.handle(), &state, &HashSet::new());
            app.manage(state);

            // After `manage`, since starting goes through the command path, which
            // reads the state from the manager; before the tray, so its first menu
            // ticks the effects running.
            for device in app.state::<AppState>().open_devices() {
                runtime::resume_applied(app.handle(), device);
            }

            // **After `manage`**, and the order is binding: the menu is built
            // on the engine's real state, which it fetches through the manager.
            // After adoption too, so that the first menu shows the devices
            // already open rather than an empty list.
            tray::install(app.handle());
            // Last: a replug reconciles through everything above.
            hotplug::watch(app.handle());

            // The window is declared `create: false`, and built here once the
            // state it reads is managed. Launched at login, no window is built
            // until someone opens it from the tray (#103); without a tray icon,
            // the window is the only way in, so it opens anyway.
            if !(autostart::launched_hidden() && tray::installed()) {
                if let Err(e) = single_instance::reveal(app.handle()) {
                    tracing::error!("window not opened: {e}");
                }
            }
            Ok(())
        })
        // The close button **hides**, it does not quit — as long as there is a
        // tray icon to keep the application reachable. See [`tray`] for the
        // why, and for what happens when the icon is missing.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Only the main window, even though there is only one: the day
                // a second one appears — a dialog, a detached screen —, closing
                // it would hide it instead of destroying it, and it would
                // reappear as is the next time. The fault would be looked for
                // far from here.
                if window.label() == single_instance::MAIN_WINDOW && tray::installed() {
                    api.prevent_close();
                    // **The preview stops here, and the applied effect does
                    // not.** That is the whole difference between the two: one
                    // is what the keyboard does — it outlives the window, that
                    // is the promise of [`tray`] —, the other is what we watch,
                    // and there is nobody left to watch.
                    //
                    // Here rather than in the window: hiding does not destroy
                    // the web view, so neither `onBeforeUnmount` nor `pagehide`
                    // fire. A QuickJS context and a thread would be kept alive
                    // for a hidden screen, indefinitely.
                    window.state::<AppState>().engine.stop_preview();
                    // Hide, not destroy: the web view keeps its state, and
                    // reopening is instant. That is also what lets the window
                    // hear `candeo://etat-change` while it is hidden, and so
                    // come back already up to date.
                    if let Err(e) = window.hide() {
                        tracing::warn!("window not hidden: {e}");
                    }
                    tray::tell_still_running(window.app_handle());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_devices,
            adopt_device,
            ignore_device,
            connect,
            disconnect,
            get_layout,
            get_default_layout,
            set_brightness,
            remember_brightness,
            set_effect,
            present,
            write_row,
            runtime::start_effect,
            runtime::stop_effect,
            runtime::set_effect_params,
            runtime::set_output_to_keyboard,
            runtime::subscribe_frames,
            runtime::unsubscribe_frames,
            runtime::start_preview,
            runtime::stop_preview,
            runtime::set_preview_params,
            runtime::subscribe_preview_frames,
            runtime::unsubscribe_preview_frames,
            runtime::engine_status,
            storage::list_effects,
            storage::save_effect_source,
            storage::cache_effect,
            storage::rename_effect,
            storage::delete_effect,
            storage::read_effect_source,
            storage::legacy_effect_ids,
            storage::duplicate_effect,
            storage::missing_builtins,
            storage::restore_builtin,
            storage::open_effects_dir,
            storage::forget_effect_settings,
            storage::get_settings,
            storage::set_settings,
            storage::set_resume_effects,
            autostart::get_launch_at_login,
            autostart::set_launch_at_login,
            storage::reset_settings,
            storage::remember_effect_params,
            journal::get_journal,
            journal::set_log_level,
            journal::set_log_files_kept,
            language::get_language,
            language::set_language,
            storage::set_theme,
            journal::open_log_dir,
            journal::diagnostic,
            journal::log_from_webview,
        ])
        .build(tauri::generate_context!())
        .expect("error while running the application")
        // `build` then `run`, not `run` alone: it is the only way to get the
        // application event handler — hence to prevent `ExitRequested`.
        //
        // **This is where "an effect runs with the window closed" becomes
        // true.** Without this handler, nothing prevents the exit: on Windows as
        // on Linux the process stops with its last window, and the render thread
        // goes with it. Three documents claimed the opposite, and it was the
        // argument that had ruled out running effects in the WebView — the
        // design was right, the implementation stopped short of the end.
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                // `None` designates an exit requested by the user — the last
                // window closing — and `Some` an exit requested by the code,
                // that is, our own "Quitter" (Quit). The distinction is the
                // whole mechanism: preventing without making it would make
                // Candeo impossible to quit, even through its only menu item
                // meant for that.
                //
                // And only as long as there is an icon: without it, nothing
                // would control the application or terminate it anymore. See
                // [`tray::installed`].
                if code.is_none() && tray::installed() {
                    api.prevent_exit();
                }
            }
        });
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_warning_is_said_in_the_interface_language() {
        let warning = Warning::Unsupported {
            name: "effect",
            command: candeo_protocol::SET_EFFECT,
        };
        let fr = warning_text(language::Language::Fr, &warning);
        assert!(
            fr.starts_with("Ce micrologiciel") && fr.contains("« effet »"),
            "{fr}"
        );
        let en = warning_text(language::Language::En, &warning);
        assert!(en.contains("“effect”"), "{en}");
    }

    /// Two made-up layouts: the only real layout is unique, and the invariant
    /// to check — one device drags down no other — only makes sense from two
    /// onwards. They only serve to be identified, hence the minimal matrix.
    static FIRST: Layout = Layout {
        name: "Premier",
        vid: 0x1532,
        pid: 0x1111,
        interface: 3,
        surveyed_firmware: candeo_protocol::Firmware { major: 1, minor: 0 },
        rows: 1,
        cols: 1,
        matrix: &[0],
        keys: &[],
    };
    static SECOND: Layout = Layout {
        name: "Second",
        vid: 0x1532,
        pid: 0x2222,
        interface: 3,
        surveyed_firmware: candeo_protocol::Firmware { major: 1, minor: 0 },
        rows: 1,
        cols: 1,
        matrix: &[0],
        keys: &[],
    };

    /// A serial specific to each layout, as in a real enumeration.
    fn plugged_with_serial(l: &Layout) -> Option<Option<String>> {
        Some(Some(format!("S{:04x}", l.pid)))
    }

    /// A device that reads `serial` over the protocol — `None` when it does not
    /// answer reads — and asks the judge before any write, as `Keyboard::open_if`
    /// does.
    fn unit(
        serial: Option<&'static str>,
    ) -> impl FnMut(&'static Layout, Judge<'_>) -> Result<&'static str, Failure> {
        move |l, judge| judge(serial).map_or(Ok(l.name), Err)
    }

    fn both_controlled() -> Settings {
        let mut settings = Settings::default();
        for l in [&FIRST, &SECOND] {
            settings.set_device_state(
                l.vid,
                l.pid,
                plugged_with_serial(l).flatten().as_deref(),
                DeviceState::Adopted,
            );
        }
        settings
    }

    /// The layouts actually opened, by name — the value returned by the tests'
    /// fake "open".
    fn newly_opened(opened: &[(&'static Layout, &'static str)]) -> Vec<&'static str> {
        opened.iter().map(|(_, name)| *name).collect()
    }

    /// **The heart of adoption.** The first device refuses to open; the second
    /// must open anyway, and the failure message must stay attached to the one
    /// that failed.
    #[test]
    fn a_failing_device_blocks_no_other() {
        let mut attempts = Vec::new();

        let (opened, outcomes) = open_adopted(
            &[&FIRST, &SECOND],
            &both_controlled(),
            plugged_with_serial,
            |l, _| {
                attempts.push(l.pid);
                if l.pid == FIRST.pid {
                    Err(Failure::new("deviceAccess").with("detail", "access denied"))
                } else {
                    Ok(l.name)
                }
            },
        );

        assert_eq!(
            attempts,
            vec![FIRST.pid, SECOND.pid],
            "the loop stopped at the first failure"
        );
        assert_eq!(newly_opened(&opened), vec!["Second"]);

        assert_eq!(outcomes.len(), 2);
        assert_eq!(
            outcomes[0],
            OpenOutcome {
                device: DeviceRef::of(&FIRST),
                error: Some(Failure::new("deviceAccess").with("detail", "access denied")),
            }
        );
        assert_eq!(
            outcomes[1],
            OpenOutcome {
                device: DeviceRef::of(&SECOND),
                error: None,
            },
            "the first one's failure spilled over onto the second"
        );
    }

    /// Neither "detected" nor "ignored" may produce the slightest opening: that
    /// is the whole point of the three states.
    #[test]
    fn only_a_controlled_device_is_opened() {
        let mut settings = Settings::default();
        settings.set_device_state(
            SECOND.vid,
            SECOND.pid,
            plugged_with_serial(&SECOND).flatten().as_deref(),
            DeviceState::Ignored,
        );

        let mut attempts = 0;
        let (opened, outcomes) = open_adopted(
            &[&FIRST, &SECOND], // FIRST was never seen: detected
            &settings,
            plugged_with_serial,
            |l, _| {
                attempts += 1;
                Ok(l.name)
            },
        );

        assert_eq!(attempts, 0, "a device not controlled was opened");
        assert!(opened.is_empty());
        assert!(outcomes.is_empty());
    }

    /// Adopted but unplugged: nothing to open, and above all nothing to
    /// report — it is not a failure, the device is simply elsewhere.
    #[test]
    fn a_controlled_but_unplugged_device_produces_no_error() {
        let (opened, outcomes) =
            open_adopted(&[&FIRST, &SECOND], &both_controlled(), |_| None, unit(None));

        assert!(opened.is_empty());
        assert!(outcomes.is_empty());
    }

    /// What issue #26 changes: `AppState` carries a **table** of devices, so
    /// the second controlled one is no longer left closed for lack of room.
    /// Each will have its handle, its loop and its effect.
    #[test]
    fn all_controlled_devices_are_opened() {
        let mut attempts = Vec::new();
        let (opened, outcomes) = open_adopted(
            &[&FIRST, &SECOND],
            &both_controlled(),
            plugged_with_serial,
            |l, _| {
                attempts.push(l.pid);
                Ok(l.name)
            },
        );

        assert_eq!(attempts, vec![FIRST.pid, SECOND.pid]);
        assert_eq!(newly_opened(&opened), vec!["Premier", "Second"]);
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|c| c.error.is_none()));
    }

    // -------------------------------------------------------- serial over protocol

    /// The DeathStalker enumeration: plugged in, **without a serial**.
    fn plugged_without_serial(_: &Layout) -> Option<Option<String>> {
        Some(None)
    }

    /// A decision taken for one unit — or for the whole model, without a serial.
    fn controlled_for(serial: Option<&str>) -> Settings {
        let mut settings = Settings::default();
        settings.set_device_state(FIRST.vid, FIRST.pid, serial, DeviceState::Adopted);
        settings
    }

    /// **What the protocol serial changes.** The silent USB descriptor lets any
    /// unit of the model through; the serial read on opening says it is not the
    /// right one, and the handle is released.
    #[test]
    fn another_unit_of_the_model_is_not_controlled() {
        let (opened, outcomes) = open_adopted(
            &[&FIRST],
            &controlled_for(Some("XY01")),
            plugged_without_serial,
            unit(Some("XY02")),
        );

        assert!(opened.is_empty(), "the neighboring unit was controlled");
        assert_eq!(outcomes.len(), 1);
        let reason = outcomes[0].error.as_ref().expect("no reason given");
        assert_eq!(reason.code, "wrongUnit");
        // The reason is shown and goes to the log: the serial is not in it, its
        // fingerprint is.
        let text = reason.to_string();
        assert!(!text.contains("XY02"), "the serial leaked: {text}");
        assert!(text.contains(&journal::fingerprint("XY02")), "{text}");
    }

    #[test]
    fn the_adopted_unit_is_recognized_by_its_serial() {
        let (opened, outcomes) = open_adopted(
            &[&FIRST],
            &controlled_for(Some("XY01")),
            plugged_without_serial,
            unit(Some("XY01")),
        );

        assert_eq!(newly_opened(&opened), vec!["Premier"]);
        assert!(outcomes[0].error.is_none());
    }

    /// An adoption older than protocol reads carries no serial: it must not
    /// refuse the unit at hand — it will learn it.
    #[test]
    fn an_adoption_without_serial_accepts_the_plugged_in_unit() {
        let (opened, _) = open_adopted(
            &[&FIRST],
            &controlled_for(None),
            plugged_without_serial,
            unit(Some("XY02")),
        );
        assert_eq!(newly_opened(&opened), vec!["Premier"]);
    }

    /// A serial that could not be read concludes nothing: refusing over an
    /// unanswered question would turn off the lighting of someone with only
    /// one keyboard.
    #[test]
    fn without_a_read_serial_nothing_is_concluded() {
        let (opened, _) = open_adopted(
            &[&FIRST],
            &controlled_for(Some("XY01")),
            plugged_without_serial,
            unit(None),
        );
        assert_eq!(newly_opened(&opened), vec!["Premier"]);
    }

    /// The protocol wins over the descriptor, and the descriptor stays the
    /// fallback of a closed device.
    #[test]
    fn the_protocol_serial_comes_before_the_descriptor_one() {
        let inspection = Inspection {
            firmware: Err("non lue".into()),
            serial: Ok("XY01".into()),
            checks: Vec::new(),
        };
        assert_eq!(
            known_serial(Some(&inspection), Some("USB".into())).as_deref(),
            Some("XY01")
        );
        assert_eq!(
            known_serial(None, Some("USB".into())).as_deref(),
            Some("USB")
        );
        let unreadable = Inspection {
            serial: Err("illisible".into()),
            ..inspection
        };
        assert_eq!(known_serial(Some(&unreadable), None), None);
    }

    /// The table is keyed on VID/PID: two devices from the same vendor must
    /// not be confused, otherwise opening the second would close the first.
    #[test]
    fn two_devices_from_the_same_vendor_have_distinct_keys() {
        let state = AppState::default();
        let first = DeviceRef::of(&FIRST);
        let second = DeviceRef::of(&SECOND);

        assert_ne!(first, second);
        assert!(state.open_devices().is_empty());

        // Without hardware we cannot put a `Keyboard` in: we check what depends
        // only on the table — a device's handle is indeed its own, and
        // targeting a device never opened creates none.
        assert!(state.opened(first).is_none());
        let handle = state.handle(first);
        assert!(state.opened(first).is_some());
        assert!(
            state.opened(second).is_none(),
            "targeting one device opened another"
        );
        assert!(Arc::ptr_eq(&handle, &state.handle(first)));
    }
}
