//! The tray icon: controlling Candeo **without the window**.
//!
//! # This module does not deliver a shortcut, it delivers a promise
//!
//! The architecture has stated from the start that **an effect runs with the
//! window closed**: the engine lives in an independent thread
//! ([`crate::runtime`]), and that is the argument that ruled out running effects
//! in the WebView. The design was right; the implementation stopped short.
//! Nothing prevented `RunEvent::ExitRequested`, so on Windows as on Linux the
//! process ended with its last window — and with it the render thread. Closing
//! the window turned the effect off.
//!
//! **This module is what makes the sentence true**, and the two halves do not
//! come apart one without the other: an icon without interception would still
//! let the application die, an interception without an icon would leave a live
//! process that nothing controls any more — and that nothing can quit.
//! Hence [`installed`], and the two interceptions in [`crate::run`] that
//! consult it: **as long as there is no icon, the close button remains an exit**.
//!
//! # What the window's close button does
//!
//! It **hides** the window, it does not quit. That was the choice to make, and it
//! follows from everything above: closing to stop the effect would break the
//! promise at the very place where it has just been kept.
//!
//! The cost is real — the application no longer has an obvious exit — and it is
//! paid twice: "Quit Candeo" is **the only** clean exit, isolated at the bottom of
//! the menu by its own separator, and the first close after each launch says so
//! in a system notification ([`tell_still_running`]). An application you cannot
//! figure out how to quit is an application you uninstall.
//!
//! # What quitting does not do: turn the keyboard off
//!
//! See [`quit`]. It is a choice, and the reasoning is given there.
//!
//! # The menu is a view, never a source
//!
//! It is rebuilt from the **real** state — `settings.json`, the library,
//! [`crate::runtime::Engine::status`] — when the pointer hovers the icon, and
//! after every action. But nothing guarantees it is current at click time: a
//! keyboard can be unplugged while the menu is open, an effect deleted from the
//! window, a loop can stop on its own after thirty failures. **Every action
//! therefore re-reads the state when it runs** instead of trusting the item that
//! was just clicked; whatever fails goes to the log, the only visible place in
//! `release`.
//!
//! # What is not guaranteed on Linux
//!
//! `TrayIconEvent` is not emitted there at all — the icon shows and its menu
//! opens, but no hover is reported. The menu is therefore refreshed there by
//! actions alone. This is a limit of the GTK/AppIndicator stack, not an
//! oversight; the workaround would be to rebuild the menu on a timer, that is,
//! to enumerate USB and read the disk in a loop for a menu nobody is looking at.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use candeo_protocol::Effect;
use tauri::menu::{
    CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Runtime, Wry};
use tauri_plugin_notification::NotificationExt;

use candeo_device::{Inspection, Layout};

use crate::language::Language;
use crate::runtime::DeviceEngineStatus;
use crate::storage::{self, DeviceState, EffectEntry, EffectKind, EffectState};
use crate::{i18n, journal, single_instance, AppState, DeviceRef};

/// The tray's own failures are only logged: English sentences, never shown.
type Built<T> = Result<T, String>;

/// The icon's id, used to find it again and give it a new menu.
const ICON_ID: &str = "candeo";

/// What the window must learn when the state changed **without it**.
///
/// The window already polls the engine every second, but it re-reads neither
/// the device list nor `settings.json`: it read them once, on mount, because it
/// was until now the only one writing them. It no longer is, and it now
/// outlives its own closing — hidden, its snapshot can age for days.
///
/// Written here **and** in `src/api/candeo.ts`; the test at the end of the module
/// checks the two against each other, otherwise renaming the event would compile
/// without a word and yield a window that never resynchronizes again.
pub(crate) const STATE_CHANGED: &str = "candeo://etat-change";

/// True when the icon is actually in place.
///
/// **This is not a convenience: it is what keeps Candeo from becoming
/// impossible to quit.** If placing it fails — no system tray, no icon in the
/// bundle, a desktop environment without a tray area — only the window is left
/// to control the application. Preventing the exit then, or hiding the window on
/// its close button, would leave a process that no ordinary gesture terminates.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Last menu build failure, so that only the transition is logged.
///
/// The menu is rebuilt **on every hover of the icon**. An unreadable
/// `settings.json` or a broken USB enumeration would produce one line per mouse
/// pass: the same flood as per-frame logging, at a different rate, and the same
/// rule shuts it. See [`journal::transition`].
static LAST_FAILURE: Mutex<Option<String>> = Mutex::new(None);

/// True if the icon is there, hence if the application outlives its windows.
pub(crate) fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// Set once [`tell_still_running`] has spoken in this launch.
static TOLD: AtomicBool = AtomicBool::new(false);

/// Says, on the first close after each launch, that Candeo still runs and how to
/// quit (#110), as applications that keep running do.
///
/// Once per launch: whoever knows does not need it repeated at every close. A
/// notification denied or with no daemon to show it changes nothing: the plugin
/// shows it in the background and drops its failure.
pub(crate) fn tell_still_running(app: &AppHandle) {
    if TOLD.swap(true, Ordering::Relaxed) {
        return;
    }
    let language = crate::language::current(app);
    let quit = i18n::text(language, "tray.quit");
    let shown = app
        .notification()
        .builder()
        .title(i18n::text(language, "tray.stillRunningTitle"))
        .body(i18n::t(
            language,
            "tray.stillRunningBody",
            &BTreeMap::from([("quit", quit)]),
        ))
        .show();
    if let Err(e) = shown {
        tracing::warn!("notification not shown: {e}");
    }
}

/// Tells the window that the state changed without it.
///
/// Generic because [`single_instance`] is: it is what brings the window back,
/// and a window returning after being hidden is exactly the case where its
/// snapshot is oldest.
///
/// Failure is swallowed: nobody listens when no window is open, and that is the
/// nominal case for this module.
pub(crate) fn notify_state_changed<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit(STATE_CHANGED, ());
}

// ---------------------------------------------------------------- items

/// What a menu item triggers.
///
/// A type, and not a string compared by hand in the handler: muda carries only a
/// text identifier, and this is the one place in the code where a typo would
/// show neither at compile time nor at run time — the item would simply do
/// nothing. Going through [`Action::to_id`] and [`Action::from_id`] brings
/// that binding back under the compiler, and the round-trip test checks it
/// without any clicking.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    /// Bring the window back into view.
    OpenWindow,
    /// **The only clean exit.**
    QuitApp,
    /// Start this effect on this device.
    ///
    /// `effet` keeps its name: [`Action::to_id`] interpolates it by name.
    Start { device: DeviceRef, effet: String },
    /// Toggle this device's keyboard output.
    ToggleOutput { device: DeviceRef },
    /// The firmware's `Effect::Off` on this device.
    TurnOff { device: DeviceRef },
}

// Each constant is named after the value it holds, and three of them are
// interpolated by name into the identifiers `Action::to_id` writes: renaming
// them would change those strings.
const OUVRIR: &str = "ouvrir";
const QUITTER: &str = "quitter";
const EFFET: &str = "effet";
const SORTIE: &str = "sortie";
const ETEINDRE: &str = "eteindre";

/// The separator between the fields of an identifier.
///
/// `:` can stay unescaped: an effect identifier is a built-in id or an effect
/// name, and [`storage::validate_name`] refuses `:` in names, as Windows does in
/// file names — so it can never contain one. The test
/// `the_effect_alphabet_excludes_the_separator` holds that dependency; without
/// it, widening the identifier alphabet one day would silently break the menu.
const SEP: char = ':';

/// The device, written to be read back — four hex digits per field.
///
/// Not the `Display` of [`DeviceRef`]: that one is made for a human reading a
/// log (`0x1532:0x0292`), and what is written here must simply parse back
/// without ambiguity.
fn hex_id(device: DeviceRef) -> String {
    format!("{:04x}{SEP}{:04x}", device.vid, device.pid)
}

/// The device named by the two fields [`hex_id`] wrote.
fn device(vid: &str, pid: &str) -> Option<DeviceRef> {
    Some(DeviceRef {
        vid: u16::from_str_radix(vid, 16).ok()?,
        pid: u16::from_str_radix(pid, 16).ok()?,
    })
}

impl Action {
    fn to_id(&self) -> String {
        match self {
            Self::OpenWindow => OUVRIR.to_owned(),
            Self::QuitApp => QUITTER.to_owned(),
            Self::Start { device, effet } => format!("{EFFET}{SEP}{}{SEP}{effet}", hex_id(*device)),
            Self::ToggleOutput { device } => format!("{SORTIE}{SEP}{}", hex_id(*device)),
            Self::TurnOff { device } => format!("{ETEINDRE}{SEP}{}", hex_id(*device)),
        }
    }

    /// The action this identifier designates, if it designates one.
    ///
    /// `None` rather than a panic: the handler is **global** — it receives the
    /// events of every menu in the application — and an item from elsewhere is
    /// not a programming error.
    fn from_id(id: &str) -> Option<Self> {
        match id {
            OUVRIR => return Some(Self::OpenWindow),
            QUITTER => return Some(Self::QuitApp),
            _ => {}
        }

        let (verb, rest) = id.split_once(SEP)?;
        let (vid, rest) = rest.split_once(SEP)?;
        match verb {
            EFFET => {
                let (pid, effet) = rest.split_once(SEP)?;
                Some(Self::Start {
                    device: device(vid, pid)?,
                    effet: effet.to_owned(),
                })
            }
            SORTIE => Some(Self::ToggleOutput {
                device: device(vid, rest)?,
            }),
            ETEINDRE => Some(Self::TurnOff {
                device: device(vid, rest)?,
            }),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------- the menu

/// A controlled device, as the menu must present it.
struct ControlledDevice {
    layout: &'static Layout,
    device: DeviceRef,
    availability: Availability,
}

/// What the tray can do with a controlled device **right now**.
///
/// Distinct from "controlled", as everywhere else: an adopted device keeps its
/// place in the menu when it is away. Removing it would hide a keyboard the
/// user decided to control, only because it is momentarily elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Availability {
    /// Open: its actions can succeed.
    Open,
    /// Plugged in but **not open**: another unit of the same model released at
    /// startup, an adopted unit whose opening failed, or a device closed by its
    /// render loop after its writes kept failing. Every action would fail with
    /// "no device open": the menu must not offer them (#72).
    NotOpen,
    /// Unplugged.
    Unplugged,
}

/// The controlled devices, plugged in or not.
///
/// USB enumeration can fail — that is the case when HID is unavailable — and
/// that is no reason to empty the menu: it then falls back to "no known serial",
/// which [`storage::DeviceRecord::matches`] tolerates precisely for this
/// situation.
fn controlled_devices(settings: &storage::Settings, state: &AppState) -> Vec<ControlledDevice> {
    let api = crate::hid().ok();
    crate::LAYOUTS
        .iter()
        .copied()
        .filter_map(|layout| {
            let plugged = api.as_ref().and_then(|api| crate::plugged(api, layout));
            let inspection = state.inspection(DeviceRef::of(layout));
            let device = controlled_device(layout, settings, plugged, inspection.as_ref());
            if let Some(d) = &device {
                tracing::debug!(device = %d.device, availability = ?d.availability, "tray menu device state");
            }
            device
        })
        .collect()
}

/// One controlled device, decided from the same facts as `list_devices`.
///
/// **The serial comes from the open handle first.** The USB descriptor of this
/// keyboard carries none, so looking the adoption up with it alone matched any
/// unit of the model, and "plugged in" was taken for "ready". The window never
/// had that problem because it reads [`crate::known_serial`] and reports `open`
/// separately; the tray now asks the same questions.
///
/// Pure, so the #72 scenario is testable without a second keyboard.
fn controlled_device(
    layout: &'static Layout,
    settings: &storage::Settings,
    plugged: Option<Option<String>>,
    inspection: Option<&Inspection>,
) -> Option<ControlledDevice> {
    let present = plugged.is_some();
    let serial = crate::known_serial(inspection, plugged.flatten());
    if settings.device_state(layout.vid, layout.pid, serial.as_deref()) != DeviceState::Adopted {
        return None;
    }
    // Unplugged wins over a handle still open: until the render loop closes
    // it, that handle only leads to failures.
    let availability = match (present, inspection.is_some()) {
        (false, _) => Availability::Unplugged,
        (true, true) => Availability::Open,
        (true, false) => Availability::NotOpen,
    };
    Some(ControlledDevice {
        layout,
        device: DeviceRef::of(layout),
        availability,
    })
}

/// What a device submenu shows and allows, derived from its availability.
#[derive(Debug, PartialEq, Eq)]
struct Presentation {
    title: String,
    /// A line explaining why nothing can be done, when that is the case.
    reason: Option<String>,
    effects: bool,
    output: bool,
    turn_off: bool,
}

/// Only an open device offers actions; the others say why they do not.
///
/// The actions still re-read the state when clicked (see the module header):
/// greying items is honesty in the menu, not the protection.
fn presentation(device: &ControlledDevice, loop_running: bool, language: Language) -> Presentation {
    let name = BTreeMap::from([("name", device.layout.name.to_owned())]);
    match device.availability {
        Availability::Open => Presentation {
            title: device.layout.name.to_owned(),
            reason: None,
            effects: true,
            // The toggle lives in the loop: without a loop there is nothing to toggle.
            output: loop_running,
            turn_off: true,
        },
        Availability::NotOpen => Presentation {
            title: i18n::t(language, "tray.notOpenTitle", &name),
            reason: Some(i18n::text(language, "tray.notOpenReason")),
            effects: false,
            output: false,
            turn_off: false,
        },
        Availability::Unplugged => Presentation {
            title: i18n::t(language, "tray.unpluggedTitle", &name),
            reason: None,
            effects: false,
            output: false,
            turn_off: false,
        },
    }
}

/// The submenus of the controlled devices, built on the **current** state.
///
/// Kept apart from the rest of the menu because it is the only part that
/// depends on the disk and on USB: see [`menu`], which treats its failure as a
/// degradation and not as a refusal.
fn device_submenus(app: &AppHandle, language: Language) -> Built<Vec<Submenu<Wry>>> {
    let store = storage::store(app)?;
    let settings = store.read_settings()?;
    let library = store.list_effects()?;
    // The real engine state, not a memory: it is the same source as the
    // `engine_status` command the window reads.
    //
    // **Devices only, never the preview.** This menu describes what the
    // keyboards are doing; ticking here an effect that is only being looked at
    // in the window would be the lie that issue #63 rejects. Nothing to filter —
    // `device_status` cannot return the preview.
    let engine_status = app.state::<AppState>().engine.device_status();

    controlled_devices(&settings, &app.state::<AppState>())
        .iter()
        .map(|controlled| device_submenu(app, controlled, &library, &engine_status, language))
        .collect()
}

/// The menu, and what was missing to build it fully.
///
/// The second member is `Some` when the menu is **degraded**: the icon is there,
/// "Open window" and "Quit Candeo" too, but the device
/// list is missing. That is deliberately a degradation and not an error — the
/// two remaining items are the ones that depend on nothing, and they are the
/// ones most needed when something is wrong. Refusing to place the icon over an
/// unreadable `settings.json` would, on top of that, make the close button fatal
/// to effects again, for an entire run of the application.
///
/// And saying so **in the menu** is not a stopgap: in `release` the binary is
/// built without a console, and the icon is precisely the place where a failure
/// can be seen without opening one.
fn menu(app: &AppHandle) -> Built<(Menu<Wry>, Option<String>)> {
    let language = crate::language::current(app);
    let mut items: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();

    let degraded = match device_submenus(app, language) {
        Ok(submenus) => {
            if submenus.is_empty() {
                // An empty section would read as a broken icon. Naming the
                // absence costs one line; Open window sits just below.
                items.push(Box::new(inert_item(
                    app,
                    &i18n::text(language, "tray.noDevice"),
                )?));
            }
            for device in submenus {
                items.push(Box::new(device));
            }
            None
        }
        Err(e) => {
            // Never the raw error in the menu: it can hold a local file path,
            // and [`log_menu_failure`] already logs it.
            items.push(Box::new(inert_item(
                app,
                &i18n::text(language, "tray.devicesUnavailable"),
            )?));
            Some(e)
        }
    };

    items.push(Box::new(separator_item(app)?));
    items.push(Box::new(item(
        app,
        &Action::OpenWindow,
        &i18n::text(language, "tray.openWindow"),
        true,
    )?));
    // Its own separator, and it is not decorative: closing the window no
    // longer quits, so this item is the application's only exit. Burying it in
    // the list above would amount to hiding it.
    items.push(Box::new(separator_item(app)?));
    items.push(Box::new(item(
        app,
        &Action::QuitApp,
        &i18n::text(language, "tray.quit"),
        true,
    )?));

    let refs: Vec<&dyn IsMenuItem<Wry>> = items.iter().map(AsRef::as_ref).collect();
    let assembled = Menu::with_items(app, &refs).map_err(|e| format!("menu not assembled: {e}"))?;
    Ok((assembled, degraded))
}

/// A device's submenu: its effect, its output, turning it off.
fn device_submenu(
    app: &AppHandle,
    controlled: &ControlledDevice,
    library: &[EffectEntry],
    engine_status: &[DeviceEngineStatus],
    language: Language,
) -> Built<Submenu<Wry>> {
    let current = engine_status
        .iter()
        .find(|s| s.device == controlled.device)
        .map(|s| &s.status);

    // `effect_id` survives the stop of a loop that shut itself down after
    // thirty failures: without the filter, the menu would tick an effect that
    // nothing runs any more.
    let running_effect = current
        .filter(|s| s.running)
        .and_then(|s| s.effect_id.as_deref());

    let view = presentation(controlled, running_effect.is_some(), language);

    let mut items: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();
    if let Some(reason) = &view.reason {
        items.push(Box::new(inert_item(app, reason)?));
        items.push(Box::new(separator_item(app)?));
    }
    // Only what can start: a file not compiled yet, or that does not load, would
    // be a menu item whose click fails with a message the tray cannot show.
    let ready: Vec<&EffectEntry> = library
        .iter()
        .filter(|e| e.state == EffectState::Ready)
        .collect();
    // Grouped like the gallery, under a heading when both groups have effects:
    // a built-in and one of the user's may share a name.
    let both = ready.iter().any(|e| e.kind == EffectKind::Builtin)
        && ready.iter().any(|e| e.kind == EffectKind::User);
    for (kind, heading) in [
        (EffectKind::Builtin, "tray.builtinEffects"),
        (EffectKind::User, "tray.userEffects"),
    ] {
        if both {
            if kind == EffectKind::User {
                items.push(Box::new(separator_item(app)?));
            }
            items.push(Box::new(inert_item(app, &i18n::text(language, heading))?));
        }
        for entry in ready.iter().filter(|e| e.kind == kind) {
            items.push(Box::new(check_item(
                app,
                &Action::Start {
                    device: controlled.device,
                    effet: entry.id.clone(),
                },
                &entry.manifest.name,
                view.effects,
                running_effect == Some(entry.id.as_str()),
            )?));
        }
    }

    items.push(Box::new(separator_item(app)?));
    items.push(Box::new(check_item(
        app,
        &Action::ToggleOutput {
            device: controlled.device,
        },
        &i18n::text(language, "tray.sendToKeyboard"),
        view.output,
        current.is_some_and(|s| s.to_keyboard),
    )?));
    items.push(Box::new(item(
        app,
        &Action::TurnOff {
            device: controlled.device,
        },
        &i18n::text(language, "tray.turnOff"),
        // Greyed out is display comfort only: what really protects is that
        // [`turn_off_device`] re-reads the state when clicked. See the module
        // header.
        view.turn_off,
    )?));

    let refs: Vec<&dyn IsMenuItem<Wry>> = items.iter().map(AsRef::as_ref).collect();
    Submenu::with_items(app, view.title, true, &refs)
        .map_err(|e| format!("submenu of {} not assembled: {e}", controlled.device))
}

fn item(app: &AppHandle, action: &Action, text: &str, enabled: bool) -> Built<MenuItem<Wry>> {
    MenuItem::with_id(app, action.to_id(), text, enabled, None::<&str>)
        .map_err(|e| format!("item “{text}” not created: {e}"))
}

fn check_item(
    app: &AppHandle,
    action: &Action,
    text: &str,
    enabled: bool,
    is_checked: bool,
) -> Built<CheckMenuItem<Wry>> {
    CheckMenuItem::with_id(app, action.to_id(), text, enabled, is_checked, None::<&str>)
        .map_err(|e| format!("check item “{text}” not created: {e}"))
}

/// An item that does nothing: it informs, and it is greyed out to say so.
///
/// No identifier, hence no [`Action`]: clicking it is impossible, and giving it
/// one would suggest otherwise.
fn inert_item(app: &AppHandle, text: &str) -> Built<MenuItem<Wry>> {
    MenuItem::new(app, text, false, None::<&str>)
        .map_err(|e| format!("item “{text}” not created: {e}"))
}

fn separator_item(app: &AppHandle) -> Built<PredefinedMenuItem<Wry>> {
    PredefinedMenuItem::separator(app).map_err(|e| format!("separator not created: {e}"))
}

// ---------------------------------------------------------------- the actions

/// Performs what the item asks for, re-reading the state along the way.
fn perform(app: &AppHandle, action: Action) {
    match action {
        // These two do not touch the engine, and so do not go through
        // [`report_change`]: [`open_window`] already notifies the returning
        // window, and [`quit`] takes the process down — rebuilding a menu or
        // notifying a window that is being destroyed would only add a failure
        // line on every exit. `app.exit` **returns**: what follows a call to
        // [`quit`] does run.
        Action::OpenWindow => open_window(app),
        Action::QuitApp => quit(app),
        Action::Start { device, effet } => {
            start(app, device, &effet);
            report_change(app);
        }
        Action::ToggleOutput { device } => {
            toggle_output(app, device);
            report_change(app);
        }
        Action::TurnOff { device } => {
            turn_off_device(app, device);
            report_change(app);
        }
    }
}

/// What an action has just invalidated: the menu, and the window.
///
/// Rebuilding the menu **now** is what keeps "the menu reflects the real state"
/// true where no hover is reported, that is, on Linux.
fn report_change(app: &AppHandle) {
    refresh(app);
    notify_state_changed(app);
}

fn open_window(app: &AppHandle) {
    // The single-instance path, not a second one: it can show a hidden window
    // as well as reopen one from its declaration, and it searches by label
    // alone — a `create: false` window is therefore still the one to open.
    // Writing another one here would make two to keep in agreement.
    if let Err(e) = single_instance::reveal(app) {
        tracing::warn!("window not brought back from the system tray: {e}");
    }
}

/// **The only clean exit.**
///
/// # The lighting is left as is, and that is a choice
///
/// Quitting does not turn the keyboard off. A firmware effect outlives the
/// software shutting down anyway — the firmware runs it — and a keyboard that
/// went dark on quit would surprise more than a keyboard that stays as it was
/// left. "Éteindre" (Turn off) is in the menu, one click away, for anyone who
/// wants darkness.
///
/// The consequence is accepted: a host-loop effect leaves the keyboard on its
/// **last frame**, frozen, and a frozen frame looks like an effect still
/// running. That is the price of the opposite choice from
/// [`crate::release_devices`], which turns the keyboard off because that path,
/// for its part, starts over from a known state.
///
/// The loops are still stopped, and **awaited**: a HID write cut off
/// mid-transfer by the end of the process would leave the device on a partial
/// report. The lighting, for its part, does not change — stopping a loop writes
/// nothing more.
fn quit(app: &AppHandle) {
    app.state::<AppState>().engine.stop_all();
    tracing::info!("Candeo exiting, requested from the system tray");
    // A code, hence `ExitRequested { code: Some(_) }`: that is what tells this
    // exit apart from the one caused by closing the last window, and therefore
    // what lets it through. See [`crate::run`].
    app.exit(0);
}

fn start(app: &AppHandle, device: DeviceRef, effect: &str) {
    // The window's command, not a copy: starting an effect from the menu must
    // do exactly what the gallery does — same library lookup, same layout, same
    // handle shared with the loop, settings re-read now.
    if let Err(e) = crate::runtime::start_saved(app, device, effect) {
        // The engine may already have named the cause under its own *span*;
        // what would be missing without this line is **where the request came
        // from** — and the most likely failure here, an effect deleted since
        // the menu was built, is refused before the engine knows anything about
        // it. One line per click floods nobody: the transitions rule targets
        // per-frame logging, not a human gesture.
        tracing::error!(device = %device, effect, "effect not started from the system tray: {e}");
    }
}

/// Toggles the keyboard output, **based on the engine state**.
///
/// Not based on the menu checkbox: muda flips it by itself on click, and it
/// dated from the last build. Trusting it would re-enable an output that had
/// just been cut from the window.
fn toggle_output(app: &AppHandle, device: DeviceRef) {
    let state = app.state::<AppState>();
    let Some(current) = state
        .engine
        .device_status()
        .into_iter()
        .find(|s| s.device == device)
    else {
        tracing::warn!(device = %device, "output not toggled: no loop on this device");
        return;
    };

    crate::runtime::set_output_to_keyboard(app.state(), device, !current.status.to_keyboard);
}

/// The firmware's `Effect::Off`: zero cost, and it survives closing.
///
/// The order is that of [`crate::release_devices`], and it matters: the loop
/// stops **and its stop is awaited**, otherwise the next frame would light up
/// again what was just turned off.
fn turn_off_device(app: &AppHandle, device: DeviceRef) {
    let state = app.state::<AppState>();
    state.engine.stop(device);
    // Nothing runs on this device any more, and the file must say so: leaving
    // the identifier in place would make `settings.json` describe an effect
    // nobody asks for any more. It is the same step `stop_effect` takes from
    // the window.
    crate::runtime::remember_active_effect(app, device, None);

    if let Err(e) = crate::with_keyboard(&state, device, |kb| Ok(kb.set_effect(Effect::Off)?)) {
        // The expected case: the device was unplugged — or ignored from the
        // window — while the menu was open. The item was enabled when the menu
        // was built; the device was gone by the time of the click.
        tracing::warn!(device = %device, "turn-off refused from the system tray: {e}");
    }
}

// ---------------------------------------------------------------- installation

/// Places the icon. **Cannot fail**, in the sense that nothing propagates.
///
/// A failure here must not prevent the application from starting: it brings it
/// back to what it was before this issue — a window, and the close button to
/// quit it. [`installed`] carries that switch, and [`crate::run`] reads it.
///
/// That leaves the failures that are not really failures: an unreadable
/// `settings.json` or a broken USB enumeration still place the icon, with a
/// menu that says so. See [`menu`].
pub(crate) fn install(app: &AppHandle) {
    match place_icon(app) {
        Ok(()) => {
            INSTALLED.store(true, Ordering::Relaxed);
            tracing::info!("tray icon placed, the window is no longer the only control");
        }
        // `error`: without an icon, closing the window stops the effects — that
        // is exactly the failure this issue closes, and it becomes silent again
        // if nobody reports it.
        Err(e) => tracing::error!("no tray icon, closing the window will stop the effects: {e}"),
    }
}

fn place_icon(app: &AppHandle) -> Built<()> {
    // The application icon, not a second image to keep up to date: it is the
    // one the bundle already ships, and the one the user recognizes.
    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| "no application icon in the bundle".to_string())?;

    // The initial menu, degradation included; what was missing is recorded
    // through the same path as later rebuilds, so that only the onset is kept.
    let (initial_menu, degraded) = menu(app)?;
    log_menu_failure(degraded.as_deref());

    TrayIconBuilder::with_id(ICON_ID)
        .icon(icon)
        .tooltip("Candeo")
        .menu(&initial_menu)
        // Left click opens the window, right click opens the menu: that is the
        // system tray convention, and it puts "Ouvrir la fenêtre" one click
        // away. On Linux no click is reported, only the right-click menu
        // responds — hence the item, which remains the safe path.
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|icon, event| match event {
            // Hover precedes the right click: it is the last moment the menu
            // can be rebuilt before it is shown.
            TrayIconEvent::Enter { .. } => {
                tracing::debug!("tray icon hovered, rebuilding the menu");
                refresh(icon.app_handle());
            }
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => open_window(icon.app_handle()),
            _ => {}
        })
        .on_menu_event(|app, event: MenuEvent| {
            // The handler is global: it sees the items of every menu go by.
            // What does not concern us is ignored, not refused.
            if let Some(action) = Action::from_id(event.id.as_ref()) {
                perform(app, action);
            }
        })
        .build(app)
        // The handle is dropped: the application's manager keeps one, that is
        // what keeps the icon alive, and [`refresh`] finds it again by its
        // id.
        .map(|_| ())
        .map_err(|e| format!("icon not placed: {e}"))
}

/// Rebuilds the menu from the current state.
///
/// The cost is that of an ordinary command: one read of `settings.json`, one
/// read of the library, and one USB enumeration. That is exactly what
/// `list_devices` costs each time the devices screen opens, and it is paid here
/// on the same terms — on a user gesture, never on a timer. A menu rebuilt in
/// the background would enumerate USB forever for a menu nobody is looking at.
pub(crate) fn refresh(app: &AppHandle) {
    let Some(icon) = app.tray_by_id(ICON_ID) else {
        return;
    };

    let failure = match menu(app) {
        // A degraded menu is indeed set: it carries a way to open the window
        // and a way to quit, and it **says** what was missing. What the log
        // keeps of it is the start of the failure, not a line per hover.
        Ok((menu, degraded)) => icon
            .set_menu(Some(menu))
            .map_err(|e| format!("menu not replaced, the previous one stays: {e}"))
            .err()
            .or(degraded),
        Err(e) => Some(e),
    };
    log_menu_failure(failure.as_deref());
}

/// Logs the **start** of a menu failure, and its recovery. Nothing
/// else — see [`LAST_FAILURE`].
fn log_menu_failure(failure: Option<&str>) {
    let mut last = LAST_FAILURE.lock().unwrap();
    let transition = journal::transition(last.as_deref(), failure);
    *last = failure.map(str::to_owned);
    drop(last);

    match transition {
        journal::Transition::Started => tracing::error!(
            "tray menu could not be built or replaced: {}",
            failure.unwrap_or_default()
        ),
        journal::Transition::Recovered => tracing::info!("tray menu recovered"),
        journal::Transition::Unchanged => {}
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: DeviceRef = DeviceRef {
        vid: 0x1532,
        pid: 0x0292,
    };
    /// Two devices, because anything "per device" only makes sense from two
    /// on — and mixing up two identifiers would make the menu act on the wrong
    /// keyboard.
    const OTHER: DeviceRef = DeviceRef {
        vid: 0x1532,
        pid: 0x0001,
    };

    fn all_actions() -> Vec<Action> {
        vec![
            Action::OpenWindow,
            Action::QuitApp,
            Action::Start {
                device: DEVICE,
                effet: "onde-circulaire".into(),
            },
            Action::Start {
                device: OTHER,
                effet: "a".into(),
            },
            Action::ToggleOutput { device: DEVICE },
            Action::TurnOff { device: OTHER },
        ]
    }

    /// **The only link between an item and what it does is a string.** muda
    /// carries nothing else: a write and a read that diverged would yield an
    /// item that does nothing, with no error at compile time or at run time.
    #[test]
    fn every_action_reads_back_as_written() {
        for action in all_actions() {
            let id = action.to_id();
            assert_eq!(
                Action::from_id(&id).as_ref(),
                Some(&action),
                "identifier \"{id}\""
            );
        }
    }

    /// Two devices must not be confused: the menu would act on the wrong
    /// keyboard, and nothing would report it.
    #[test]
    fn two_devices_give_two_identifiers() {
        assert_ne!(
            Action::TurnOff { device: DEVICE }.to_id(),
            Action::TurnOff { device: OTHER }.to_id()
        );
        assert_ne!(hex_id(DEVICE), hex_id(OTHER));
    }

    /// The handler is global: it receives the items of every menu in the
    /// application. What does not come from here must be ignored, not
    /// misinterpreted.
    #[test]
    fn a_foreign_identifier_triggers_nothing() {
        for id in [
            "",
            "ouvrir-vraiment",
            "effet",
            "effet:1532",
            "effet:1532:0292",
            "effet:zzzz:0292:onde",
            "sortie:1532",
            "sortie:1532:0292:en-trop",
            "eteindre:1532:029x",
            "3", // an identifier muda numbered itself
        ] {
            assert_eq!(Action::from_id(id), None, "\"{id}\" was interpreted");
        }
    }

    /// **What makes `:` usable without escaping.** The day the effect identifier
    /// alphabet widened, the menu would start targeting the wrong effect — or
    /// none — without anything saying so.
    #[test]
    fn the_effect_alphabet_excludes_the_separator() {
        assert!(storage::validate_name(&format!("a{SEP}b")).is_err());

        let effet = format!("Onde (copie) é{}", "a".repeat(50));
        let action = Action::Start {
            device: DEVICE,
            effet: effet.clone(),
        };
        storage::validate_name(&effet).expect("name refused");
        assert_eq!(Action::from_id(&action.to_id()), Some(action));
    }

    fn inspection_with_serial(serial: &str) -> Inspection {
        Inspection {
            firmware: Err("not read".into()),
            serial: Ok(serial.into()),
            checks: Vec::new(),
        }
    }

    fn adopted(serial: &str) -> storage::Settings {
        let mut settings = storage::Settings::default();
        let layout = &candeo_device::DEATHSTALKER_V2_PRO;
        settings.set_device_state(layout.vid, layout.pid, Some(serial), DeviceState::Adopted);
        settings
    }

    /// #72: the adopted unit is plugged in but not open — another unit of the
    /// model released at startup, or a device its render loop closed. The USB
    /// descriptor gives no serial, so the adoption still matches: the menu must
    /// say "not open" and offer nothing, instead of looking ready.
    #[test]
    fn a_plugged_but_unopened_device_offers_no_action() {
        let settings = adopted("XY01");
        let layout = &candeo_device::DEATHSTALKER_V2_PRO;

        let device =
            controlled_device(layout, &settings, Some(None), None).expect("still controlled");
        assert_eq!(device.availability, Availability::NotOpen);

        let view = presentation(&device, false, Language::En);
        assert!(!view.effects && !view.output && !view.turn_off, "{view:?}");
        assert!(view.reason.is_some());
        assert_ne!(view.title, layout.name, "the title must not look ready");
        assert_eq!(view.title, format!("{} — not open", layout.name));
        assert_eq!(
            presentation(&device, false, Language::Fr).title,
            format!("{} — non ouvert", layout.name)
        );
    }

    #[test]
    fn an_open_device_offers_its_actions() {
        let settings = adopted("XY01");
        let layout = &candeo_device::DEATHSTALKER_V2_PRO;
        let open = inspection_with_serial("XY01");

        let device =
            controlled_device(layout, &settings, Some(None), Some(&open)).expect("controlled");
        assert_eq!(device.availability, Availability::Open);

        let view = presentation(&device, false, Language::En);
        assert!(
            view.effects && view.turn_off && view.reason.is_none(),
            "{view:?}"
        );
        assert!(!view.output, "no loop, nothing to toggle");
        assert_eq!(view.title, layout.name);
        assert!(presentation(&device, true, Language::En).output);
    }

    /// A handle can outlive the unplugging until the render loop closes it: the
    /// enumeration wins, since that handle only leads to failures.
    #[test]
    fn unplugged_wins_over_a_stale_handle() {
        let settings = adopted("XY01");
        let layout = &candeo_device::DEATHSTALKER_V2_PRO;
        let stale = inspection_with_serial("XY01");

        let device =
            controlled_device(layout, &settings, None, Some(&stale)).expect("still controlled");
        assert_eq!(device.availability, Availability::Unplugged);
        assert!(!presentation(&device, true, Language::En).effects);
    }

    /// The open unit's serial decides, not the model: an open unit that is not
    /// the adopted one is not presented as controlled.
    #[test]
    fn the_open_units_serial_decides_adoption() {
        let mut settings = adopted("XY01");
        let layout = &candeo_device::DEATHSTALKER_V2_PRO;
        settings.set_device_state(layout.vid, layout.pid, Some("XY02"), DeviceState::Ignored);
        let other = inspection_with_serial("XY02");

        assert!(controlled_device(layout, &settings, Some(None), Some(&other)).is_none());
    }

    /// The event name is written on both sides of the IPC, and nothing links the
    /// two at compile time: renaming it on one side only would yield a window
    /// that no longer resynchronizes, without a single error anywhere. Same
    /// guard as for the window label, see [`single_instance`].
    #[test]
    fn the_event_has_the_same_name_on_both_sides() {
        let ts = include_str!("../../src/api/candeo.ts");
        assert!(
            ts.contains(STATE_CHANGED),
            "\"{STATE_CHANGED}\" not found in src/api/candeo.ts"
        );
    }
}
