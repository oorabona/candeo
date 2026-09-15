//! One instance of Candeo at a time.
//!
//! Two processes share neither the keyboard nor the settings, and both failures
//! are hard to read:
//!
//! - **The hardware.** Each would open its own HID handle and run its own loops
//!   at 30 fps on the same interface. The keyboard would flicker between two
//!   effects without either window showing anything unusual: each one displays
//!   *its own* render in the simulator, and that render is correct.
//! - **The settings.** [`crate::storage::Store::write_settings`] writes to a
//!   temporary file with a **fixed** name, then renames it. The name can stay
//!   fixed because synchronous Tauri commands run on the main thread — but that
//!   reasoning holds *within* one process. With two, one would rename what the
//!   other is still writing: the rename would stay atomic, what it publishes
//!   would not.
//!
//! **The single instance is therefore what makes the fixed temporary name
//! safe.** The two decisions hold together, and do not come apart one without
//! the other: making Candeo multi-instance would force revisiting that name,
//! and revisiting that name without it would buy nothing.
//!
//! # What this does not protect against
//!
//! The manufacturer's runtime — or any other lighting software — writes to the
//! same keyboard, and nothing here can prevent it. That is a separate problem,
//! better handled by a note in the documentation than by a feature.

use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Manager, Runtime, WebviewWindow, WebviewWindowBuilder};

/// Label of the main window.
///
/// It is written in three places that nothing links at compile time: here, in
/// `tauri.conf.json` — a window without a `label` gets "main", Tauri's default —
/// and in `capabilities/default.json`, which grants its permissions to that
/// window only. The test at the end of the module checks the three against each
/// other.
pub(crate) const MAIN_WINDOW: &str = "main";

/// The single-instance plugin, **to be registered before all the others**.
///
/// It is during the plugin's initialization that the second process finds out
/// it is one too many, notifies the running instance and exits. Plugins are
/// initialized in registration order, and all of them before the windows are
/// created and before the application's `setup`: putting it first means dying
/// before opening a single HID handle. Further down the list, the extra process
/// would touch the keyboard in the time it takes to find out it is extra —
/// exactly what this is meant to prevent.
pub(crate) fn init<R: Runtime>() -> TauriPlugin<R> {
    tauri_plugin_single_instance::init(|app, _args, _cwd| {
        // The second launch's arguments and working directory are ignored:
        // Candeo has no command line. The day it has one — opening an effect,
        // for instance — this is where it would be relayed to the running
        // instance.
        if let Err(e) = reveal(app) {
            // A log line, and nothing more. The extra process is already gone,
            // there is nobody left to refuse anything to; panicking would take
            // down the surviving instance and the effects it runs, over a window
            // that did not come to the foreground.
            //
            // `warn`: degraded but working — the exclusion worked, only raising
            // the window failed. That is the nuance the former `eprintln!` could
            // not carry, since it could not be read in `release`: the binary is
            // built without a console.
            tracing::warn!("single instance: {e}");
        }
    })
}

/// Brings the main window back in front of whoever asks for it.
///
/// Without this, the second launch would vanish silently, and a launch with no
/// visible effect reads as a refusal to start.
///
/// Two callers now, and deliberately the same path: the second launch, and
/// "Ouvrir la fenêtre" (Open window) in the system tray ([`crate::tray`]). Both
/// ask for exactly the same thing — a hidden window to show, or a destroyed
/// window to reopen from its declaration — and writing two versions would make
/// them diverge at the first fix.
pub(crate) fn reveal<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let window = match app.get_webview_window(MAIN_WINDOW) {
        Some(window) => window,
        None => reopen(app)?,
    };

    // The window is coming back: its snapshot may date from the last time it
    // was hidden, that is, from days ago. Notifying before showing it rather
    // than after makes no difference — the event is asynchronous — but doing it
    // here covers both callers at once. A window that was just **reopened** will
    // not hear it: it has not loaded its JavaScript yet, and it does not need
    // to — it reads everything on mount.
    crate::tray::notify_state_changed(app);

    // All three, because none implies the others: a hidden window that is only
    // brought to the foreground stays invisible, a minimized window that is
    // shown stays minimized, and a visible window behind another one stays
    // there until it is given focus. There is no attempt to find out which of
    // the three applied: asking for all of them costs less than reading the
    // window's state to deduce the same thing.
    window
        .show()
        .and_then(|()| window.unminimize())
        .and_then(|()| window.set_focus())
        .map_err(|e| format!("window “{MAIN_WINDOW}” not brought to the foreground: {e}"))
}

/// Reopens the main window **from its declaration**.
///
/// The window is built nowhere in this code: it is declared in
/// `tauri.conf.json`, and Tauri builds it at startup. Reopening it therefore
/// means re-reading that declaration — not copying a size and a title here,
/// which would make a second source of truth and diverge as soon as the window
/// is resized in the configuration. Along the way it gets its label back, and
/// with it the permissions the capability grants to that window only.
///
/// This branch was a safety net for as long as the process ended with its last
/// window; it became the nominal path the day [`crate::tray`] separated the
/// two — window closed, effects still running in their threads, which do not
/// depend on it.
///
/// Today the close button **hides** the window instead of destroying it, so
/// once built, [`reveal`] finds it and never gets here. The window is declared
/// `create: false`: this is how it is built at startup, and, launched at login
/// ([`crate::autostart`]), the first time someone asks to see it.
fn reopen<R: Runtime>(app: &AppHandle<R>) -> Result<WebviewWindow<R>, String> {
    let config = app
        .config()
        .app
        .windows
        .iter()
        // On the label alone, and above all not on the `create` flag Tauri
        // reads at startup: a window declared `create: false` — which is how
        // Candeo would start hidden in the system tray — is still exactly the
        // one to open when someone launches Candeo again.
        .find(|w| w.label == MAIN_WINDOW)
        .cloned()
        .ok_or_else(|| format!("no “{MAIN_WINDOW}” window declared in tauri.conf.json"))?;

    WebviewWindowBuilder::from_config(app, &config)
        .and_then(|builder| builder.build())
        .map_err(|e| format!("window “{MAIN_WINDOW}” not reopened: {e}"))
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// The three spellings of the label must stay in agreement.
    ///
    /// Nothing links them at compile time: renaming the window in
    /// `tauri.conf.json` would compile without a word and yield a silent
    /// failure — [`reveal`] would never find the existing window again, would
    /// open a new one on every relaunch, and that one would have none of the
    /// permissions the capability reserves for "main".
    #[test]
    fn the_window_label_is_the_same_everywhere() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        let declared: Vec<&str> = config["app"]["windows"]
            .as_array()
            .expect("no window declared")
            .iter()
            // A window without a `label` gets "main": the other two spellings
            // depend on that Tauri default, so it is replayed here rather than
            // assumed absent.
            .map(|w| w["label"].as_str().unwrap_or(MAIN_WINDOW))
            .collect();
        assert!(
            declared.contains(&MAIN_WINDOW),
            "declared windows: {declared:?}"
        );

        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json"))
                .expect("capabilities/default.json");
        let targeted = capability["windows"]
            .as_array()
            .expect("the capability targets no window");
        assert!(
            targeted.iter().any(|w| w.as_str() == Some(MAIN_WINDOW)),
            "the capability does not target \"{MAIN_WINDOW}\": {targeted:?}"
        );
    }
}
