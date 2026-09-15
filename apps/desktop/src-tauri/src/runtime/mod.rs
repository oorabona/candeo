//! Effects engine: **one render thread per device**, independent of the window.
//!
//! This is the only place where effect code runs. The front end never runs any:
//! it sends the source and receives the frames. The simulator preview is
//! therefore the production output, by construction — not a likeness obtained
//! by running the same code in a second JavaScript engine.
//!
//! # One device, one effect
//!
//! Each device carries its own loop, hence its own frame rate, parameters, error
//! state and output. Nothing is shared between two devices: that is what keeps a
//! failing device from affecting any other — the adoption invariant (issue #25),
//! held this time at the engine level.
//!
//! A loop receives **a layout** and **an output**, never "a keyboard". The day a
//! layout spans several devices, [`DeviceOut`] will split the frame, and the
//! effect code will not change by a single line.
//!
//! # And one preview loop, which belongs to no device
//!
//! **Previewing must never interrupt the effect running on the keyboard.**
//! With the engine at one effect per device, previewing Y on a keyboard running
//! X would stop X: browsing the gallery would turn off the current lighting
//! (issue #63). The preview is a **separate** loop, a single one, whose hardware
//! output is [`NoOutput`] — `DeviceOut::present` returning `None` already means
//! "no device open, this is not a failure".
//!
//! It **borrows the layout** of the selected device, to look like what will be
//! obtained, without taking anything else from it: not its loop, not its handle,
//! not its status line.
//!
//! That is why [`EngineReport`] keeps the two in **two separate fields** rather
//! than in a list to filter. See [`PreviewStatus`].
//!
//! # What an effect cannot drag on
//!
//! A `while (true)` in `render` would freeze its thread for good: the `stop`
//! flag is read *between* two frames, so it would never be read again — and
//! since an effect runs with the window closed, closing it would not help. An
//! endless allocation, for its part, would take down the whole process rather
//! than the effect alone.
//!
//! Neither is a matter of malice: they are two ordinary programming errors, and
//! two limits are enough to treat them as such — a compute time per frame
//! ([`FRAME_BUDGET`], and [`LOAD_BUDGET`] for the module body) and a memory per
//! effect ([`MEMORY_BUDGET`]). Exceeding one opens **no new path**: it takes the
//! exception path, which [`MAX_CONSECUTIVE_ERRORS`] turns into a clean stop.
//! What the engine adds is the name of the cause: see [`name_the_cause`].
//!
//! # Lock order
//!
//! Three locks, and the order is the declaration order: **loop table → a
//! device's thread → a loop's shared state**.
//!
//! The table is locked only long enough to read or put an `Arc` in it, never
//! during a start or while waiting for a thread to end. The next two are taken
//! together, in this order, by [`DeviceLoop::start`] and [`DeviceLoop::stop`] —
//! that is what serializes starting and stopping a device, and the lock waited
//! on is **its own**: waiting for one to end holds back no command aimed at the
//! others.
//!
//! The render thread takes only the last one: it knows only its [`Shared`] and
//! its output, never the engine. It therefore has no way to hold back a command,
//! and it never holds a device handle and an engine lock at the same time. See
//! `crate::AppState`.
//!
//! See `docs/design/effects-runtime.md`.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use candeo_device::{Keyboard, Layout};
use candeo_protocol::{Effect, Rgb};
use rquickjs::loader::{BuiltinLoader, BuiltinResolver};
use rquickjs::runtime::InterruptHandler;
use rquickjs::{CatchResultExt, Context, Function, Module, Runtime};
use serde::Serialize;
use tauri::ipc::{Channel, InvokeResponseBody};

use crate::journal;

pub mod presses;
pub mod swatch;

use presses::Presses;

/// The module the host provides, and that the editor describes through its
/// `.d.ts`.
const API_JS: &str = include_str!("api.js");

/// The glue that imports the effect and installs the render function.
const BOOTSTRAP_JS: &str = include_str!("bootstrap.js");

/// 30 frames per second — **measured, not assumed**.
///
/// The frame rate was 60, by analogy with a screen. Timing on the hardware says
/// otherwise: a full update costs **7 control transfers** — 6 rows then the
/// switch to custom mode, see `Keyboard::present` — and the 120 measured give
/// **13.1 ms on average, 14.4 ms at worst**, without a single refused write. The
/// device therefore accepts up to ~76 fps.
///
/// 60 fps therefore *barely* held: the write alone ate **78 % of the 16.7 ms
/// period**, leaving ~3.6 ms to the effect — less than the compute budget it is
/// granted. In other words an effect **well within its limits** already made the
/// deadline slip, and the loop silently fell back to a frame rate it announced
/// nowhere.
///
/// At 33.3 ms, the write drops to 39 % and ~20 ms remain for the effect. What 60
/// promised without keeping, 30 keeps.
///
/// ⚠️ **The bottleneck is the bus, not the JavaScript.** Two leads if the frame
/// rate ever had to go back up: rewrite only the rows that change — partial
/// writes are verified on hardware — and stop re-sending the custom mode report
/// when already in it, which alone accounts for ~1.9 ms of the 13.
const FPS: u32 = 30;

/// Beyond this, stop. An effect that throws on every frame will not recover on
/// its own, and carrying on would only fill the log silently.
const MAX_CONSECUTIVE_ERRORS: u32 = 30;

/// Consecutive failed writes after which the loop **closes** its device.
///
/// An unplugged keyboard leaves a dead handle: every write fails, and plugging
/// it back in creates a new device instance that this handle will never reach.
/// Kept open, it made the window and the tray report the device as open while
/// nothing got through (#72). Closed, every view tells the truth, and
/// reconnecting goes through the existing commands.
///
/// One second at [`FPS`]: a transient failure does not close anything, and a
/// device that is gone is not presented as open for long.
const MAX_DEVICE_WRITE_ERRORS: u32 = FPS;

/// Time granted to compute **one** frame.
///
/// ## What this figure really measures
///
/// Not a performance allowance: **a freeze detector**, with headroom for machine
/// hiccups. An ordinary effect costs **0.23 ms** and a field of five thousand
/// particles with a one-second trail **1.1 ms** — measured in `release` on this
/// engine, for a 132-LED keyboard. No realistic effect lives between 1 and
/// 10 ms.
///
/// What these milliseconds buy is therefore **tolerated preemption**: the
/// deadline is measured in wall-clock time, not compute time, and a thread the
/// scheduler suspends in the middle of a frame consumes its budget without
/// executing anything. At 10 ms, an ordinary effect can be suspended for nearly
/// 10 ms without being accused of freezing.
///
/// ## Where the ceiling is
///
/// The HID write of a full frame costs **13.1 ms on average, 14.4 ms at worst**
/// (§5 of the survey), and it lives in the same 33.3 ms period:
///
/// ```text
///   budget 10 ms + write 14.4 ms = 24.4 ms   →  9 ms of headroom
/// ```
///
/// The threshold where the deadline would start to **slip silently** is around
/// **19 ms** of budget. We are far from it, and that is what makes 10 ms safe
/// where the original figure — half of a 16.7 ms period — was just a proportion,
/// and no longer a measurement.
///
/// ## And far below a freeze
///
/// An effect that does not return is stopped after [`MAX_CONSECUTIVE_ERRORS`]
/// frames, that is **0.3 s** — whereas a budget of one second per frame would
/// have waited half a minute before saying what is wrong.
///
/// Exceeding it once stops nothing: the consecutive error counter goes back to
/// zero at the first rendered frame. It takes thirty in a row.
#[cfg(not(debug_assertions))]
const FRAME_BUDGET: Duration = Duration::from_millis(10);

/// The same budget, at the speed of the engine actually compiled.
///
/// QuickJS is C, compiled at the profile's optimization level. Unoptimized, the
/// **same** frame of the **same** simple effect went from 0.23 ms to 6.3 ms of
/// JavaScript: twenty-five times slower, measured. It was not the effect that
/// changed, it was the interpreter.
///
/// A single budget would therefore have had to pick a side: at 10 ms it would
/// cut flawless effects as soon as the app runs in development; at 200 ms it
/// would allow a six-second freeze in production.
///
/// ⚠️ **This factor of 25 was measured before `[profile.dev.package."*"]` moved
/// dependencies to `opt-level = 2`.** QuickJS comes in through a dependency: it
/// is therefore optimized in debug builds too now, and the gap should have
/// melted. This value remains **a ceiling, not a target** — keeping it wide
/// costs nothing as long as nobody takes it for an up-to-date measurement. To be
/// redone if someone wants to unify the two budgets.
#[cfg(debug_assertions)]
const FRAME_BUDGET: Duration = Duration::from_millis(200);

/// Time granted to **load** an effect, module body included.
///
/// The module body runs once, before the first frame: it therefore escapes the
/// frame budget. Without a limit here, a loop written outside `render` would
/// block [`DeviceLoop::start`] forever — the command waits for the load verdict
/// while holding the device lock, and that keyboard would never start or stop
/// anything again.
///
/// Parsing and evaluating a module takes milliseconds; two seconds is three
/// orders of magnitude above, and that price is paid only once.
const LOAD_BUDGET: Duration = Duration::from_secs(2);

/// Memory granted to an effect's JavaScript engine — **one per device**.
///
/// The frame buffer weighs nothing: `bootstrap.js` reuses it from one frame to
/// the next. But an effect is allowed to keep state, and that is what has to
/// fit. Measured, QuickJS context and loaded modules included: 0.17 MB for a
/// stateless effect, 2.7 MB for two thousand particles keeping one second of
/// frames, 6 MB for five thousand. Thirty-two megabytes therefore leave five
/// times the most outlandish effect we know how to write for 132 LEDs, and
/// nearly two hundred times the ordinary effect — while staying negligible next
/// to the application, even with one effect per keyboard.
///
/// Unlike time, memory does not depend on the build profile: the same objects
/// take the same bytes.
///
/// What is bounded is not an effect's appetite: it is an array that grows on
/// every frame taking down the whole process instead of itself.
const MEMORY_BUDGET: usize = 32 * 1024 * 1024;

/// What the loop shares with the rest of the application.
///
/// Everything is behind `Arc`: the render thread outlives the window, so it
/// cannot borrow anything from a command's state.
struct Shared {
    stop: AtomicBool,
    /// Effect parameters, as JSON. Re-read on every frame: setting them does
    /// not restart the loop.
    params: Mutex<String>,
    /// Keyboard output. Separate from the simulator output — one must be able
    /// to write an effect without owning the keyboard, and to leave it running
    /// without watching the screen.
    to_keyboard: AtomicBool,
    /// Simulator output. `None` while nobody listens: nothing is serialized
    /// then.
    frames: Mutex<Option<Channel<InvokeResponseBody>>>,
    /// The effect's last error. Readable even after the window is closed and
    /// reopened, which a one-off event would not allow.
    error: Mutex<Option<String>>,
    /// Last failed write to the keyboard.
    ///
    /// Distinct from the effect error above: these two failures have neither
    /// the same cause nor the same remedy, and mixing them up would send
    /// someone looking in the wrong place. A flawless effect may well reach no
    /// LED.
    device_error: Mutex<Option<Failure>>,
    /// True if the last frame was actually written to a device.
    ///
    /// Without it, starting an effect with no keyboard connected produced **no
    /// sign at all**: the simulator animated, the "envoyer" (send) box stayed
    /// checked, and the keyboard kept its previous frame. A silence that reads
    /// as an engine failure.
    reaching: AtomicBool,
    /// Consecutive failed writes, reset by any successful write. See
    /// [`MAX_DEVICE_WRITE_ERRORS`].
    device_failures: AtomicU32,
    /// Name of the running effect, so that the interface knows what to
    /// highlight after the window restarts.
    effect_id: Mutex<Option<String>>,
    /// The effect reads key presses: its error text stays out of the log and
    /// the diagnostic. See [`loggable`].
    reads_keys: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            stop: AtomicBool::new(false),
            params: Mutex::new("{}".to_string()),
            to_keyboard: AtomicBool::new(true),
            frames: Mutex::new(None),
            error: Mutex::new(None),
            device_error: Mutex::new(None),
            reaching: AtomicBool::new(false),
            device_failures: AtomicU32::new(0),
            effect_id: Mutex::new(None),
            reads_keys: AtomicBool::new(false),
        }
    }
}

/// What the log and the diagnostic may say of an effect's error.
///
/// An effect that reads key presses chooses its error text, and could write the
/// presses into it; nothing about key presses reaches the log or the diagnostic
/// (`docs/design/key-input.md` §3, #44). Its text is left out there; the window,
/// which is local, still shows it.
pub(crate) fn loggable(error: &str, reads_keys: bool) -> &str {
    if reads_keys {
        "(text not logged: the effect reads key presses)"
    } else {
        error
    }
}

/// Engine state for **one** device, as the interface reads it.
#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub running: bool,
    pub effect_id: Option<String>,
    /// Error coming from the effect code, already readable: shown as is.
    pub error: Option<String>,
    /// Failed write to the keyboard — nothing to do with the effect code.
    pub device_error: Option<Failure>,
    /// True if the frames actually reach a keyboard.
    pub reaching_keyboard: bool,
    pub to_keyboard: bool,
    /// For the diagnostic: see [`loggable`].
    #[serde(skip)]
    pub reads_keys: bool,
}

/// A device's state, and which device it belongs to.
///
/// `engine_status()` returns one per targeted device: a global message would
/// force a choice of which one to show, and the next would erase the previous —
/// exactly what the table of open failures already avoids on the adoption side.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEngineStatus {
    pub device: DeviceRef,
    #[serde(flatten)]
    pub status: EngineStatus,
}

/// What the window **watches**, and which reaches no keyboard.
///
/// # A type of its own, not one more line in the device list
///
/// This is the fourth time in this project that a lying state has cost a round
/// of debugging — the unadopted keyboard, the "accepted" write, the frame frozen
/// after an automatic stop, and now the preview. A flag to filter gets filtered
/// badly: a single caller that forgets it — the tray icon, the log, the gallery
/// — is enough to announce as running on the keyboard an effect that is only
/// being watched. Here there is **nothing to filter**: the preview is not in the
/// list, and a caller cannot find it there by mistake.
///
/// # What it does not carry is deliberate too
///
/// No `toKeyboard`, no `reachingKeyboard`, no `deviceError`. A preview loop has
/// **no** hardware output; those three fields set to false would not describe a
/// preview, they would describe an effect failing to write — that is, a
/// failure, where there is only a choice.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewStatus {
    /// The device whose layout the preview **borrows**.
    ///
    /// It is not controlled, it is not even necessarily plugged in: it is a
    /// geometry, not a destination. Naming it lets the interface say "what it
    /// will look like on that keyboard".
    pub layout_of: DeviceRef,
    pub running: bool,
    pub effect_id: Option<String>,
    /// Error coming from the effect code, already readable: shown as is.
    pub error: Option<String>,
    /// For the diagnostic: see [`loggable`].
    #[serde(skip)]
    pub reads_keys: bool,
}

/// Everything the engine knows, **arranged so that nothing gets confused**.
///
/// Two fields, not a list: `devices` describes what runs on the hardware,
/// `preview` what the window watches. The tray, the log and the gallery read
/// only the first — see [`PreviewStatus`] for why.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineReport {
    pub devices: Vec<DeviceEngineStatus>,
    /// `None` when nothing is being previewed — which is the case as soon as
    /// the window is closed, see [`Engine::stop_preview`].
    pub preview: Option<PreviewStatus>,
}

// ---------------------------------------------------------------- output

/// A loop's hardware output.
///
/// A trait rather than the [`Keyboard`] itself, for two reasons that matter
/// equally:
///
/// 1. **it is the only place where a loop touches hardware.** The day a layout
///    spans several devices, this is where the frame will be split; neither
///    the loop nor the effect code will have to change;
/// 2. **a test can make a device fail.** Without this seam, "a failing device
///    affects no other" could only be checked with two keyboards plugged in,
///    so never.
pub(crate) trait DeviceOut: Send {
    /// Writes a frame.
    ///
    /// `None` when no device is open. That is not a failure: one writes an
    /// effect without owning the keyboard, and the interface must be able to
    /// say so other than as an error.
    ///
    /// `abandon`: if this write fails too, **drop the device** in the same
    /// critical section as the write. Deciding outside it would race with a
    /// reconnection: the loop could close the keyboard that was just reopened.
    fn present(&self, colors: &[Rgb], abandon: bool) -> Option<Result<(), String>>;

    /// Turns the backlight off, when an effect stopped on its own: its last frame,
    /// frozen, would look like an effect still running (#48). Nothing to do
    /// without a device.
    fn turn_off(&self) {}
}

/// A device handle, shared between the commands and its loop.
///
/// `Arc` because the loop outlives the window: it cannot borrow anything from a
/// command's state. `Option` because closing a device — ignored, unplugged —
/// must not stop the loop that fed it: the loop notices on the next frame and
/// reports it through `reachingKeyboard`.
pub(crate) type Handle = Arc<Mutex<Option<Keyboard>>>;

impl DeviceOut for Handle {
    fn present(&self, colors: &[Rgb], abandon: bool) -> Option<Result<(), String>> {
        // The handle lock is released **before** the result is recorded: a
        // loop never holds the handle and an engine lock at the same time.
        let mut guard = self.lock().unwrap();
        let result = guard.as_ref()?.present(colors).map_err(|e| e.to_string());
        if result.is_err() && abandon {
            // Only the keyboard that just failed can be dropped here: a
            // reconnection would have replaced it before this lock was taken.
            *guard = None;
        }
        Some(result)
    }

    fn turn_off(&self) {
        let guard = self.lock().unwrap();
        if let Some(Err(e)) = guard.as_ref().map(|kb| kb.set_effect(Effect::Off)) {
            tracing::warn!("backlight not turned off after the effect stopped: {e}");
        }
    }
}

/// The preview's output: **none**.
///
/// Nothing to invent here — `None` already means "no device open, this is not
/// a failure", and that is exactly what a preview is. The loop therefore feeds
/// its frame channel and nothing else, `reachingKeyboard` stays false, and not
/// a single byte goes to a keyboard.
struct NoOutput;

impl DeviceOut for NoOutput {
    fn present(&self, _colors: &[Rgb], _abandon: bool) -> Option<Result<(), String>> {
        None
    }
}

/// Who a render loop belongs to.
///
/// A type, rather than a `bool` next to the [`DeviceRef`]: the two cases are
/// logged neither at the same level nor under the same word, and "the preview's
/// device" does not exist — there is only a borrowed layout. The compiler holds
/// here a distinction that two arguments side by side would let slip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// A device's loop: it writes to the hardware.
    Device(DeviceRef),
    /// The preview loop, which borrows this device's layout without
    /// controlling it.
    Preview(DeviceRef),
}

// ---------------------------------------------------------------- engine

/// The running loops, one per device.
///
/// The table holds only `Arc`s: it is locked for the length of a lookup, never
/// for a start or a wait. Stopping one device's loop therefore holds back no
/// command aimed at the others — otherwise a HID write stuck on one would
/// freeze the other, and the adoption invariant would not survive the engine.
#[derive(Default)]
pub struct Engine {
    loops: Mutex<HashMap<DeviceRef, Arc<DeviceLoop>>>,
    /// The preview loop: **a single one**, with no hardware output.
    ///
    /// One per window, and there is only one — the promise "an effect runs with
    /// the window closed" holds only for devices, and a preview that nobody
    /// watches is a QuickJS context kept alive for nothing.
    ///
    /// It lives **next to** the table, never inside: a table entry would be
    /// found by `all()`, hence stopped by `stop_everywhere`, counted by
    /// `status()`, and it would have had to be excluded every time. Keeping it
    /// out of the table makes the type hold what would otherwise be held by
    /// vigilance.
    preview: Arc<DeviceLoop>,
    /// The layout the preview borrows, written and cleared with the loop.
    preview_layout: Mutex<Option<DeviceRef>>,
    /// Key presses, read only while a loop runs an effect declaring them.
    presses: Arc<Presses>,
}

/// The loop of **one** device.
///
/// Two locks, and the order between them is fixed: `thread` then `shared`.
/// `thread` serializes start and stop; `shared` is taken only long enough to
/// clone or replace an `Arc`, never during a wait. That is what allows reading
/// a device's state while another one starts — and even while this one starts.
#[derive(Default)]
struct DeviceLoop {
    thread: Mutex<Option<JoinHandle<()>>>,
    shared: Mutex<Option<Arc<Shared>>>,
}

impl DeviceLoop {
    fn current(&self) -> Option<Arc<Shared>> {
        self.shared.lock().unwrap().clone()
    }

    /// True if **this** effect is the one the loop is running.
    ///
    /// The id is the one requested from `start`, not a property of the loaded
    /// code: the JavaScript is read once at start and then lives in memory, so
    /// there is nothing to re-read to know it — and that is precisely why
    /// deleting an effect does not get noticed on its own.
    fn runs(&self, effect: &str) -> bool {
        self.current()
            .is_some_and(|s| s.effect_id.lock().unwrap().as_deref() == Some(effect))
    }

    fn status(&self) -> EngineStatus {
        match self.current() {
            None => EngineStatus::default(),
            Some(s) => EngineStatus {
                running: !s.stop.load(Ordering::Relaxed),
                effect_id: s.effect_id.lock().unwrap().clone(),
                error: s.error.lock().unwrap().clone(),
                device_error: s.device_error.lock().unwrap().clone(),
                reaching_keyboard: s.reaching.load(Ordering::Relaxed),
                to_keyboard: s.to_keyboard.load(Ordering::Relaxed),
                reads_keys: s.reads_keys.load(Ordering::Relaxed),
            },
        }
    }

    /// Stops the loop and **waits** for it to end.
    ///
    /// The wait is not a detail: without it, starting an effect right after
    /// stopping one would let two loops write to the same device until the
    /// first one notices it must stop. The reasoning holds per device, and so
    /// does the lock waited on.
    ///
    /// `target` is only for the log: a loop does not know its device — it
    /// receives a layout and an output — and "effect stopped" without saying
    /// which one would be worthless with two keyboards plugged in.
    fn stop(&self, target: Target) {
        let mut thread = self.thread.lock().unwrap();
        let was_running = self.shared.lock().unwrap().take().inspect(|s| {
            s.stop.store(true, Ordering::Relaxed);
        });
        if let Some(h) = thread.take() {
            let _ = h.join();
        }
        // Only if something was running: stopping an idle device is the most
        // common action of all — every effect deletion goes through it — and
        // tells nobody anything.
        if let Some(s) = was_running {
            let effect = s.effect_id.lock().unwrap().clone();
            match target {
                Target::Device(d) => tracing::info!(device = %d, effect, "effect stopped"),
                // `debug`: see [`DeviceLoop::start`]. The "lifecycle" level
                // describes what the keyboard does, and a preview does not touch it.
                Target::Preview(d) => tracing::debug!(layout = %d, effect, "preview stopped"),
            }
        }
    }

    /// Starts an effect on this target. Replaces the one that was running.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &self,
        target: Target,
        effect_id: String,
        js: String,
        params: String,
        layout: &'static Layout,
        out: Box<dyn DeviceOut>,
        presses: Arc<Presses>,
    ) -> Result<(), String> {
        // Held from start to finish: this lock is what forbids two loops from
        // overlapping on this device. It is taken only here and in
        // [`Self::stop`], and the render thread does not know it.
        let mut thread = self.thread.lock().unwrap();
        if let Some(s) = self.shared.lock().unwrap().take() {
            s.stop.store(true, Ordering::Relaxed);
        }
        if let Some(h) = thread.take() {
            let _ = h.join();
        }

        let shared = Arc::new(Shared::default());
        *shared.params.lock().unwrap() = params;
        *shared.effect_id.lock().unwrap() = Some(effect_id.clone());

        // The JavaScript context is built **inside** the thread and never
        // leaves it: QuickJS types do not cross threads, and confining them
        // here is safer than making them shareable.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let s = Arc::clone(&shared);

        let thread_effect_id = effect_id.clone();
        let handle = std::thread::Builder::new()
            .name("candeo-effect".into())
            .spawn(move || {
                render_loop(
                    target,
                    thread_effect_id,
                    s,
                    js,
                    layout,
                    out,
                    presses,
                    ready_tx,
                )
            })
            .map_err(|e| format!("render thread not started: {e}"))?;

        // Wait for the load verdict: a syntax error must surface to the call,
        // not be discovered in a status later.
        match ready_rx.recv() {
            Ok(Ok(())) => {
                match target {
                    Target::Device(d) => {
                        tracing::info!(device = %d, effect = %effect_id, "effect started")
                    }
                    // `debug` and not `info`, and this is not shyness:
                    // "lifecycle" is the level that describes **what the
                    // keyboard does**, and a preview does not touch it. Browsing
                    // the gallery would otherwise fill the log with lines that
                    // match nothing lit.
                    Target::Preview(d) => {
                        tracing::debug!(layout = %d, effect = %effect_id, "preview started")
                    }
                }
                *self.shared.lock().unwrap() = Some(shared);
                *thread = Some(handle);
                Ok(())
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                match target {
                    // `error`: the requested effect will not run, so the
                    // lighting is not the one requested. The caller gets the
                    // same message — the log is for whoever reads it afterwards,
                    // and for whoever does not have the window in front of them.
                    Target::Device(d) => {
                        tracing::error!(device = %d, effect = %effect_id, "effect not started: {e}")
                    }
                    // `warn`: nothing is broken on the hardware, but the screen
                    // will not show what was requested — and that is precisely
                    // the first place where a freshly written effect breaks.
                    Target::Preview(d) => {
                        tracing::warn!(layout = %d, effect = %effect_id, "preview not started: {e}")
                    }
                }
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                let e = "the render thread stopped before loading the effect".to_string();
                tracing::error!(target = ?target, effect = %effect_id, "{e}");
                Err(e)
            }
        }
    }
}

impl Engine {
    /// This device's loop, created stopped if it did not exist.
    ///
    /// Only a start creates one. The entry is then never removed: a device that
    /// has carried an effect keeps its line in `engine_status()`, stopped rather
    /// than absent. "This device is doing nothing" and "I know nothing about
    /// this device" are not the same statement.
    fn device_loop(&self, device: DeviceRef) -> Arc<DeviceLoop> {
        Arc::clone(self.loops.lock().unwrap().entry(device).or_default())
    }

    /// This device's loop, **without creating one**.
    ///
    /// Setting or stopping a device that never started anything does nothing,
    /// and above all must not invent a status line for it.
    fn existing(&self, device: DeviceRef) -> Option<Arc<DeviceLoop>> {
        self.loops.lock().unwrap().get(&device).map(Arc::clone)
    }

    /// All loops, with the table unlocked.
    ///
    /// The copy is not a detail: acting on a loop means waiting for a thread to
    /// end, which we refuse to do while holding the table lock.
    fn all(&self) -> Vec<(DeviceRef, Arc<DeviceLoop>)> {
        let mut all: Vec<_> = self
            .loops
            .lock()
            .unwrap()
            .iter()
            .map(|(d, l)| (*d, Arc::clone(l)))
            .collect();
        // A hash table orders nothing, and a list that reorders itself on every
        // query is unreadable in the interface.
        all.sort_by_key(|(d, _)| (d.vid, d.pid));
        all
    }

    /// State of every device targeted since the application started.
    ///
    /// **The preview is not in it, and cannot be**: it does not live in the
    /// table. This is what the tray icon and the diagnostic read — the two
    /// places that describe the hardware.
    pub fn device_status(&self) -> Vec<DeviceEngineStatus> {
        self.all()
            .into_iter()
            .map(|(device, l)| DeviceEngineStatus {
                device,
                status: l.status(),
            })
            .collect()
    }

    /// The current preview, if there is one.
    ///
    /// `None` as soon as the loop is stopped by [`Self::stop_preview`]: a
    /// preview is transient, and "the last effect you watched" is information
    /// for nobody. A preview that cut out **on its own** — thirty failed frames
    /// — does stay visible, with `running` false and the error along with it:
    /// that is the only way to know why the screen froze.
    pub fn preview_status(&self) -> Option<PreviewStatus> {
        let layout_of = (*self.preview_layout.lock().unwrap())?;
        let s = self.preview.current()?;
        let status = PreviewStatus {
            layout_of,
            running: !s.stop.load(Ordering::Relaxed),
            effect_id: s.effect_id.lock().unwrap().clone(),
            error: s.error.lock().unwrap().clone(),
            reads_keys: s.reads_keys.load(Ordering::Relaxed),
        };
        Some(status)
    }

    /// Everything the engine knows, devices and preview **kept apart**.
    pub fn report(&self) -> EngineReport {
        EngineReport {
            devices: self.device_status(),
            preview: self.preview_status(),
        }
    }

    /// The shared state of the loop running on this device, if there is one.
    fn shared(&self, device: DeviceRef) -> Option<Arc<Shared>> {
        self.existing(device).and_then(|l| l.current())
    }

    pub fn stop(&self, device: DeviceRef) {
        if let Some(l) = self.existing(device) {
            l.stop(Target::Device(device));
        }
    }

    /// Stops **all** loops, preview included, and waits for them to end.
    ///
    /// Used at process exit as well as when resetting the configuration: in
    /// both cases we start again from a known state, and leaving running loops
    /// that nothing refers to any more would be exactly the opposite. The
    /// preview is part of it — it writes to no keyboard, but it keeps a QuickJS
    /// context and a thread alive.
    pub fn stop_all(&self) {
        self.stop_preview();
        for (device, l) in self.all() {
            l.stop(Target::Device(device));
        }
    }

    // ------------------------------------------------------------ preview

    /// Starts — or replaces — the preview, **without touching any device**.
    ///
    /// `layout_of` names the device whose layout is borrowed; it is neither
    /// open, nor controlled, nor even necessarily plugged in. The output is
    /// [`NoOutput`]: not a single byte goes to a keyboard, whatever happens.
    ///
    /// Replacing costs one QuickJS context destroyed and another one built.
    /// That is not free, and that is why the rate is bounded **on the gesture
    /// side** — the window waits for the selection to settle before calling.
    /// Bounding it here would have forced a choice between making the last
    /// selection wait and losing it, and the window would have had to reconcile
    /// what it believed it had requested with what is running.
    pub fn start_preview(
        &self,
        layout_of: DeviceRef,
        effect_id: String,
        js: String,
        params: String,
        layout: &'static Layout,
    ) -> Result<(), String> {
        // Written **before** the start: if the start fails, the loop is empty
        // and `preview_status` returns `None` anyway — whereas a layout set
        // afterwards would be missing for the whole load.
        *self.preview_layout.lock().unwrap() = Some(layout_of);
        self.preview.start(
            Target::Preview(layout_of),
            effect_id,
            js,
            params,
            layout,
            Box::new(NoOutput),
            Arc::clone(&self.presses),
        )
    }

    /// Stops the preview. No device effect is touched.
    pub fn stop_preview(&self) {
        // The layout is taken before the stop, so that the log line names the
        // one that was borrowed rather than nothing.
        let borrowed = self.preview_layout.lock().unwrap().take();
        if let Some(device) = borrowed {
            self.preview.stop(Target::Preview(device));
        }
    }

    pub fn set_preview_params(&self, params: String) {
        if let Some(s) = self.preview.current() {
            *s.params.lock().unwrap() = params;
        }
    }

    pub fn set_preview_channel(&self, channel: Option<Channel<InvokeResponseBody>>) {
        if let Some(s) = self.preview.current() {
            *s.frames.lock().unwrap() = channel;
        }
    }

    /// Stops this effect **wherever it runs**, and returns the devices affected.
    ///
    /// Called before an effect is deleted: the loop runs an `effect.js` loaded
    /// into memory at start, so it would carry on without the slightest error
    /// while its folder no longer exists — a device controlled by an effect
    /// missing from the library.
    ///
    /// All devices, not just the one being looked at: the same effect can be
    /// started on as many keyboards as one likes, and missing one would leave it
    /// in that invisible state.
    ///
    /// The device's status line does not disappear, but it stops naming the
    /// effect — that is what [`DeviceLoop::stop`] does, and it is exactly what
    /// is wanted here: the id no longer refers to anything.
    pub fn stop_everywhere(&self, effect: &str) -> Vec<DeviceRef> {
        // **The preview too**, and for exactly the same reason: it runs the
        // same `effect.js` loaded into memory, and leaving it running would give
        // a screen animating an effect missing from the library. It is not in
        // the returned list — no device was touched.
        if self.preview.runs(effect) {
            self.stop_preview();
        }

        let mut stopped = Vec::new();
        for (device, l) in self.all() {
            // The table lock is already released — `all` copied the pointers.
            // A stop waits for a thread to end, and a command aimed at another
            // device is never made to wait.
            if l.runs(effect) {
                l.stop(Target::Device(device));
                stopped.push(device);
            }
        }
        stopped
    }

    /// Renames this effect wherever it runs, preview included.
    ///
    /// The loops keep the JavaScript they loaded at start: only the id they
    /// report changes. That is what lets renaming an effect leave the lighting as
    /// it is, while every status line names the effect the way the library does.
    pub fn rename_everywhere(&self, from: &str, to: &str) {
        let loops = self
            .all()
            .into_iter()
            .map(|(_, l)| l)
            .chain(std::iter::once(Arc::clone(&self.preview)));
        for l in loops {
            if let Some(s) = l.current() {
                let mut id = s.effect_id.lock().unwrap();
                if id.as_deref() == Some(from) {
                    *id = Some(to.to_owned());
                }
            }
        }
    }

    pub fn set_params(&self, device: DeviceRef, params: String) {
        if let Some(s) = self.shared(device) {
            *s.params.lock().unwrap() = params;
        }
    }

    pub fn set_to_keyboard(&self, device: DeviceRef, on: bool) {
        if let Some(s) = self.shared(device) {
            s.to_keyboard.store(on, Ordering::Relaxed);
        }
    }

    pub fn set_channel(&self, device: DeviceRef, channel: Option<Channel<InvokeResponseBody>>) {
        if let Some(s) = self.shared(device) {
            *s.frames.lock().unwrap() = channel;
        }
    }

    /// Starts an effect on a device. Replaces the one running there.
    ///
    /// The other devices are not touched — not their loop, not their frame
    /// rate, not their error state.
    pub fn start(
        &self,
        device: DeviceRef,
        effect_id: String,
        js: String,
        params: String,
        layout: &'static Layout,
        out: Box<dyn DeviceOut>,
    ) -> Result<(), String> {
        self.device_loop(device).start(
            Target::Device(device),
            effect_id,
            js,
            params,
            layout,
            out,
            Arc::clone(&self.presses),
        )
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// Prepares the QuickJS context, then runs until stopped.
#[allow(clippy::too_many_arguments)]
fn render_loop(
    target: Target,
    effect_id: String,
    shared: Arc<Shared>,
    js: String,
    layout: &'static Layout,
    out: Box<dyn DeviceOut>,
    presses: Arc<Presses>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    // **The span, and it is the reason `tracing` was chosen.** There is one
    // loop per device: "write refused" is useless without knowing which one.
    // Opened here, it carries the device and the effect until the thread ends,
    // and everything logged below — including in [`emit`] — carries them too,
    // without a single call having to pass them.
    //
    // Two names, not a field to read: in a log read afterwards, "render" and
    // "preview" must be told apart at a glance — an
    // effect error in one did not turn off the keyboard, in the other it did.
    let span = match target {
        Target::Device(d) => tracing::info_span!("render", device = %d, effect = %effect_id),
        Target::Preview(d) => tracing::info_span!("preview", layout = %d, effect = %effect_id),
    };
    let _entered = span.enter();

    let frame_len = layout.led_count();

    // The budget is born here and never leaves the thread: the interrupt
    // handler crosses no boundary, and the deadline is written only by this
    // loop, just before each run of effect code.
    let budget = Rc::new(Budget::default());

    // Loading gets its own: the module body runs once, before the first frame,
    // hence outside any frame budget.
    budget.grant(LOAD_BUDGET);

    // `_rt` must live as long as the context: it carries the module resolver.
    // Dropping it here would make every `import` unresolvable on the first
    // frame.
    let (_rt, ctx) = match prepare_budgeted(&js, layout, &budget) {
        Ok(c) => {
            let _ = ready.send(Ok(()));
            c
        }
        Err(e) => {
            let _ = ready.send(Err(name_the_cause(
                e,
                &budget,
                "loading the effect",
                &format!("{} s", LOAD_BUDGET.as_secs()),
            )));
            return;
        }
    };

    // Presses are read only for an effect that declares them, and for as long as
    // this loop runs it: the guard ends with the thread
    // (`docs/design/key-input.md` §3). A device's loop reads its own keyboard; the
    // preview draws a layout, not a device, and reads them all.
    //
    // `started` first: a press that arrives once the guard exists is never older
    // than the effect's clock.
    let started = Instant::now();
    let reads_keys = ctx
        .with(|ctx| ctx.globals().get::<_, bool>("__candeo_reads_keys"))
        .unwrap_or(false);
    shared.reads_keys.store(reads_keys, Ordering::Relaxed);
    let keys = reads_keys.then(|| (presses.read(), presses::positions(layout)));
    let keyboard = match target {
        Target::Device(d) => Some((d.vid, d.pid)),
        Target::Preview(_) => None,
    };

    let period = Duration::from_nanos(1_000_000_000 / u64::from(FPS));
    let mut deadline = Instant::now();
    let mut frame_index: u32 = 0;
    let mut consecutive_errors: u32 = 0;

    while !shared.stop.load(Ordering::Relaxed) {
        let params = shared.params.lock().unwrap().clone();
        let now = Instant::now();
        let time = now.duration_since(started).as_secs_f64();
        let pressed = match &keys {
            Some((reading, positions)) => {
                positions.to_json(&reading.since(started, now, keyboard), now, time)
            }
            None => String::new(),
        };

        // The deadline is renewed before **every** frame: that is the whole
        // point of the shared cell. The handler, for its part, was set once and
        // for all on the `Runtime`.
        budget.grant(FRAME_BUDGET);

        match render_with_presses(&ctx, time, frame_index, &params, &pressed, frame_len) {
            Ok(bytes) => {
                consecutive_errors = 0;
                // The effect recovered: clear the error, otherwise the
                // interface would show a stale one indefinitely.
                let before = shared.error.lock().unwrap().take();
                if journal::transition(before.as_deref(), None) == journal::Transition::Recovered {
                    tracing::info!("effect recovered");
                }
                emit(&shared, out.as_ref(), &bytes);
            }
            Err(e) => {
                // Exceeding a limit opens no new path: it is a frame error like
                // any other, which the counter below eventually turns into a
                // clean stop.
                let e = name_the_cause(
                    e,
                    &budget,
                    "the effect",
                    &format!("{} ms per frame", FRAME_BUDGET.as_millis()),
                );
                consecutive_errors += 1;
                // **One line when the failure starts, not one per frame.** At 30
                // frames per second, logging every failure would produce thirty
                // lines per second and bury the one that names the cause.
                // [`journal::transition`] holds the rule.
                let before = shared.error.lock().unwrap().replace(e.clone());
                if journal::transition(before.as_deref(), Some(&e)) == journal::Transition::Started
                {
                    tracing::warn!("effect started failing: {}", loggable(&e, reads_keys));
                }
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    tracing::error!(
                        failures = MAX_CONSECUTIVE_ERRORS,
                        "effect stopped after {MAX_CONSECUTIVE_ERRORS} consecutive failures, \
                         backlight turned off: {}",
                        loggable(&e, reads_keys)
                    );
                    shared.stop.store(true, Ordering::Relaxed);
                    // A frozen last frame reads as an effect still running; off
                    // says nothing runs, and the gallery says why (#48).
                    out.turn_off();
                    break;
                }
            }
        }

        frame_index = frame_index.wrapping_add(1);

        // Absolute deadline rather than `sleep(period)`: a slow frame must not
        // shift all the following ones. If we fell behind, start again from now
        // instead of trying to catch up at high speed.
        deadline += period;
        let now = Instant::now();
        if deadline > now {
            std::thread::sleep(deadline - now);
        } else {
            deadline = now;
        }
    }
}

/// The deadline the interrupt handler checks — and what it did about it.
///
/// A shared cell, not a captured deadline: the handler is set on the `Runtime`
/// **once**, whereas the deadline changes on **every frame**.
/// [`prepare_bounded`], which grants only one for a whole installation, can
/// make do with capturing it; the render loop cannot.
///
/// The flag is there to **name the cause**. QuickJS throws the same
/// "InternalError: interrupted" whatever the reason for an interruption, and it
/// is the handler — and only the handler — that knows the deadline made it
/// throw.
#[derive(Default)]
struct Budget {
    deadline: Cell<Option<Instant>>,
    exceeded: Cell<bool>,
}

impl Budget {
    /// Grants `duration` to the next run, with the flag lowered.
    fn grant(&self, duration: Duration) {
        self.deadline.set(Some(Instant::now() + duration));
        self.exceeded.set(false);
    }

    /// The handler itself: `true` cuts the current run.
    ///
    /// QuickJS calls it every 10,000 instructions — an `Instant::now()` at that
    /// frequency is not measurable.
    fn expire(&self) -> bool {
        match self.deadline.get() {
            Some(end) if Instant::now() >= end => {
                self.exceeded.set(true);
                true
            }
            _ => false,
        }
    }
}

/// Loads an effect once and returns the manifest its module declares, as JSON.
///
/// Bounded like swatch sampling: this is effect code being run so the library
/// can list it, and a module that loops at the top level must fail here with an
/// error, not freeze the command that records it.
pub(crate) fn declared_manifest(js: &str) -> Result<String, String> {
    let deadline = Instant::now() + swatch::BUDGET;
    let (_rt, ctx) = prepare_bounded(js, crate::default_layout(), Some(deadline))?;
    ctx.with(|ctx| ctx.globals().get::<_, String>("__candeo_manifest"))
        .map_err(|e| format!("unreadable manifest: {e}"))
}

/// JavaScript context ready to render, **with no time limit**.
///
/// Reserved for tests. Since the render loop bounds every frame and swatch
/// sampling bounds its installation, no production caller prepares a context
/// that a `while (true)` could freeze any more — that is the whole point of the
/// two budgets. What remains are the tests that are not about the limits, to
/// which a deadline would only add machine-dependent randomness.
///
/// The `Runtime` is returned with the context, not kept here: it carries the
/// module resolver, so it must live just as long.
#[cfg(test)]
fn prepare(js: &str, layout: &'static Layout) -> Result<(Runtime, Context), String> {
    prepare_bounded(js, layout, None)
}

/// Like [`prepare_with`], with a **single** deadline, captured by value.
///
/// It covers everything the caller will do with the context, from loading to
/// the last frame. That is what swatch sampling needs — a few frames, a single
/// limit, on a command's thread where a `while (true)` would prevent an
/// installation from completing. The render loop, for its part, changes it on
/// every frame: see [`Budget`].
///
/// The deadline is set **before** any evaluation, to cover the module body as
/// well.
fn prepare_bounded(
    js: &str,
    layout: &'static Layout,
    deadline: Option<Instant>,
) -> Result<(Runtime, Context), String> {
    let interrupt =
        deadline.map(|end| -> InterruptHandler { Box::new(move || Instant::now() >= end) });
    prepare_with(js, layout, interrupt)
}

/// Like [`prepare_with`], but bounded **call by call**: the handler reads a
/// budget that the caller renews before each run.
fn prepare_budgeted(
    js: &str,
    layout: &'static Layout,
    budget: &Rc<Budget>,
) -> Result<(Runtime, Context), String> {
    let budget = Rc::clone(budget);
    prepare_with(js, layout, Some(Box::new(move || budget.expire())))
}

/// The common trunk, for a layout from this repository: it serializes, then
/// delegates.
fn prepare_with(
    js: &str,
    layout: &'static Layout,
    interrupt: Option<InterruptHandler>,
) -> Result<(Runtime, Context), String> {
    prepare_with_layout(js, layout.led_count(), layout_json(layout), interrupt)
}

/// The trunk: limits set, modules resolved, `__candeo_render` installed, for an
/// **already serialized** layout.
///
/// Both limits are set **before** any evaluation — the module body is effect
/// code like any other.
///
/// The layout arrives as JSON rather than as a [`Layout`] for a reason that only
/// shows in the tests: `candeo_device::Key` carries a **mandatory** rectangle,
/// so no layout in this repository can describe a device without a surveyed
/// geometry — and yet that is the case an effect measuring physical distances
/// must refuse, saying so. Writing it by hand is the only way to check that
/// refusal until #34 makes the rectangle optional.
fn prepare_with_layout(
    js: &str,
    frame_len: usize,
    layout_json: String,
    interrupt: Option<InterruptHandler>,
) -> Result<(Runtime, Context), String> {
    let rt = Runtime::new().map_err(|e| format!("QuickJS : {e}"))?;

    rt.set_interrupt_handler(interrupt);

    // The memory limit, for its part, is not renewed and applies to every
    // caller: an endless allocation takes down the whole process, whether it
    // happens in a render loop or during an effect installation.
    rt.set_memory_limit(MEMORY_BUDGET);

    // `@candeo/effects-api` is **built in**. That is what allows writing a
    // normal `import` without a bundler, without path resolution and without
    // `node_modules`.
    let resolver = BuiltinResolver::default()
        .with_module("@candeo/effects-api")
        .with_module("effect");
    let loader = BuiltinLoader::default()
        .with_module("@candeo/effects-api", API_JS)
        .with_module("effect", js);
    rt.set_loader(resolver, loader);

    let ctx = Context::full(&rt).map_err(|e| format!("QuickJS : {e}"))?;

    ctx.with(|ctx| -> Result<(), String> {
        let g = ctx.globals();
        g.set("__candeo_frame_len", frame_len as u32)
            .map_err(js_error)?;
        g.set("__candeo_layout", layout_json).map_err(js_error)?;

        Module::evaluate(ctx.clone(), "bootstrap", BOOTSTRAP_JS)
            .catch(&ctx)
            .map_err(|e| format!("loading the effect: {e}"))?
            .finish::<()>()
            .catch(&ctx)
            .map_err(|e| format!("loading the effect: {e}"))?;
        Ok(())
    })?;

    Ok((rt, ctx))
}

/// One frame with no key presses: swatches, tests, and effects that read no keys.
fn render_once(
    ctx: &Context,
    time: f64,
    frame_index: u32,
    params: &str,
    frame_len: usize,
) -> Result<Vec<u8>, String> {
    render_with_presses(ctx, time, frame_index, params, "", frame_len)
}

/// One frame, with `presses` as `presses::to_json` writes them (empty: none).
fn render_with_presses(
    ctx: &Context,
    time: f64,
    frame_index: u32,
    params: &str,
    presses: &str,
    frame_len: usize,
) -> Result<Vec<u8>, String> {
    ctx.with(|ctx| {
        let render: Function = ctx
            .globals()
            .get("__candeo_render")
            .map_err(|_| "the render function is gone from the context".to_string())?;

        let out: Vec<u8> = render
            .call((time, frame_index, params, presses))
            .catch(&ctx)
            .map_err(|e| format!("{e}"))?;

        if out.len() != frame_len * 3 {
            return Err(format!(
                "the effect rendered {} bytes, {} expected",
                out.len(),
                frame_len * 3
            ));
        }
        Ok(out)
    })
}

/// The text QuickJS throws when the memory limit is reached, and the only sign
/// it gives of it (`JS_ThrowOutOfMemory`).
const QUICKJS_OOM: &str = "out of memory";

/// Turns a failure of a bounded run into a cause the effect author can relate
/// to **their** code.
///
/// QuickJS names neither limit: an interruption surfaces as
/// "InternalError: interrupted", which points at nothing, and a memory overrun
/// as "out of memory", which says neither whose nor by how much. Both limits
/// are set here; so it is here, and nowhere else, that they can be explained.
///
/// Time is read from the budget flag, never from the message: the handler did
/// the cutting, and it alone knows so for sure. Memory has only QuickJS's text
/// — hence the comparison, and the fallback to the raw error when it says
/// nothing: misnaming a cause would be worse than not naming it.
///
/// `subject` tells the two bounded places apart — a frame, a load. The loop that
/// never ends is not at the same place in the file, and the time `granted` is not
/// the same.
///
/// English, like the TypeScript diagnostics next to it: the reader is the effect's
/// author.
fn name_the_cause(error: String, budget: &Budget, subject: &str, granted: &str) -> String {
    if budget.exceeded.get() {
        return format!(
            "{subject} exceeded its computing time ({granted}): \
             a loop that never ends, or a computation too heavy."
        );
    }
    if error.contains(QUICKJS_OOM) {
        return format!(
            "{subject} exceeded the memory granted to it ({} MB): \
             a state that grows on every frame, or an oversized allocation.",
            MEMORY_BUDGET / (1024 * 1024)
        );
    }
    error
}

/// The two outputs, independent: either one may be absent.
fn emit(shared: &Shared, out: &dyn DeviceOut, bytes: &[u8]) {
    if !shared.to_keyboard.load(Ordering::Relaxed) {
        // Output deliberately turned off: this is not a fault, but the frames
        // reach no keyboard and the interface must be able to say so.
        shared.reaching.store(false, Ordering::Relaxed);
    } else {
        let colors: Vec<Rgb> = bytes
            .chunks_exact(3)
            .map(|c| Rgb::new(c[0], c[1], c[2]))
            .collect();
        // A keyboard unplugged midway does not stop the effect: the simulator
        // carries on, and reconnecting goes through the existing commands. But
        // the failure is **recorded**, not swallowed — hiding it gave a loop
        // that claims to be healthy while not a single byte reaches the device.
        // And this device's failure says nothing about the others: each loop
        // writes into its own state.
        let failures = shared.device_failures.load(Ordering::Relaxed);
        let abandon = failures + 1 >= MAX_DEVICE_WRITE_ERRORS;
        match out.present(&colors, abandon) {
            // **No device open.** Without this report, starting an effect with
            // no keyboard connected produced no sign at all: the simulator
            // animated, the "envoyer" (send) box stayed checked, and the
            // keyboard kept its previous frame. It read as "only the first frame
            // got through".
            // Nothing to log: writing an effect **without owning the keyboard**
            // is an intended use, not a failure. Saying so on every frame would
            // drown the log, and saying it once would pass off as an incident
            // what `reachingKeyboard` already makes visible on screen.
            None => {
                shared.device_failures.store(0, Ordering::Relaxed);
                shared.reaching.store(false, Ordering::Relaxed);
            }
            Some(Ok(())) => {
                shared.device_failures.store(0, Ordering::Relaxed);
                let before = shared.device_error.lock().unwrap().take();
                if journal::transition(before.as_ref(), None) == journal::Transition::Recovered {
                    tracing::info!("writing to the device recovered");
                }
                shared.reaching.store(true, Ordering::Relaxed);
            }
            Some(Err(e)) if abandon => {
                // The device was dropped under its own lock (see
                // [`DeviceOut::present`]). The message kept for the window says
                // what to do, a closed device is reopened by the existing
                // commands; the technical cause goes to the log only.
                shared.device_failures.store(0, Ordering::Relaxed);
                *shared.device_error.lock().unwrap() = Some(Failure::new("deviceClosed"));
                tracing::warn!(
                    "device closed after {MAX_DEVICE_WRITE_ERRORS} consecutive failed writes: {e}"
                );
                shared.reaching.store(false, Ordering::Relaxed);
            }
            Some(Err(e)) => {
                // Same rule as for the effect error: the start of the failure,
                // and nothing else. A keyboard unplugged midway would fail on
                // every frame until it is plugged back in.
                shared
                    .device_failures
                    .store(failures + 1, Ordering::Relaxed);
                let failure = Failure::new("deviceWrite").with("detail", &e);
                let before = shared.device_error.lock().unwrap().replace(failure.clone());
                if journal::transition(before.as_ref(), Some(&failure))
                    == journal::Transition::Started
                {
                    tracing::warn!("writing to the device started failing: {e}");
                }
                shared.reaching.store(false, Ordering::Relaxed);
            }
        }
    }

    // Raw binary: serializing as a JSON array of integers would take a 396-byte
    // frame to more than 1.5 KB, thirty times per second.
    if let Some(ch) = shared.frames.lock().unwrap().as_ref() {
        let _ = ch.send(InvokeResponseBody::Raw(bytes.to_vec()));
    }
}

/// The layout as the effect sees it.
///
/// Serialized by hand: `candeo-device` has no serde, and that is deliberate —
/// its tests run without any system dependency.
///
/// # Two spaces, and both travel
///
/// `row`/`col` place the LED in the matrix; `x`/`y`/`w`/`h` give the keycap
/// rectangle, in keyboard pitch units — the same unit as
/// [`candeo_device::Key`], with no conversion on the way. An effect that talks
/// about distance cannot be right without the second: a matrix cell is one
/// cell, whether the key is 1 u or 6.25 u wide.
///
/// The geometry is **always** emitted here, because `candeo_device::Key` always
/// carries it. The day the rectangle becomes optional (#34), these four fields
/// will disappear for layouts that have not been drawn, and the effects reading
/// them will fail saying so — see `bounds` and `center` in `api.js`.
fn layout_json(l: &'static Layout) -> String {
    let mut keys = String::new();
    for (row, col, k) in keys_in_order(l) {
        if !keys.is_empty() {
            keys.push(',');
        }
        let mut fields = format!(r#""index":{},"row":{row},"col":{col}"#, k.index);
        if k.scancode != candeo_device::NO_SCANCODE {
            fields.push_str(&format!(r#","scancode":{}"#, k.scancode));
        }
        if let Some(label) = crate::keys::label(k.scancode) {
            fields.push_str(&format!(r#","label":{}"#, json_string(&label)));
        }
        keys.push_str(&format!(
            r#"{{{fields},"x":{},"y":{},"w":{},"h":{}}}"#,
            k.x, k.y, k.w, k.h
        ));
    }
    format!(
        r#"{{"name":{},"rows":{},"cols":{},"keys":[{keys}]}}"#,
        json_string(l.name),
        l.rows,
        l.cols
    )
}

/// A layout's keys, row by row: the order of `layout.keys` for effects. The
/// positions presses are given as ([`presses::positions`]) depend on it.
fn keys_in_order(
    l: &'static Layout,
) -> impl Iterator<Item = (u8, u8, &'static candeo_device::Key)> {
    (0..l.rows)
        .flat_map(move |row| (0..l.cols).map(move |col| (row, col)))
        .filter_map(move |(row, col)| Some((row, col, l.key(l.at(row, col)?)?)))
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn js_error(e: rquickjs::Error) -> String {
    format!("QuickJS : {e}")
}

// ---------------------------------------------------------------- commands

use crate::{AppState, CmdResult, DeviceRef, Failure};
use tauri::{AppHandle, Manager, State};

/// Starts an effect, built-in or installed, **on a device**.
///
/// The `id → JavaScript` resolution is the library's, so built-ins come first:
/// see [`crate::storage`]. The engine itself makes no difference — a shipped
/// effect is a module loaded exactly like the one just written.
///
/// The layout comes from **the targeted device**, open or not. That is
/// deliberate: one must be able to write and preview an effect **without owning
/// the keyboard**, and a device's layout does not depend on its presence.
/// Targeting an unplugged device therefore starts the effect, feeds the
/// simulator, and `reachingKeyboard` stays false until the device is opened.
///
/// # This is the gesture that **commits** the keyboard
///
/// Previewing is the other path, and it does not go through here:
/// [`start_preview`] opens no hardware output and writes nothing to disk.
/// "Appliquer" (Apply) does both — it sends to the keyboard, and it
/// **remembers** the effect for this device.
#[tauri::command]
pub fn start_effect(
    app: AppHandle,
    state: State<'_, AppState>,
    device: DeviceRef,
    id: String,
    params: serde_json::Value,
) -> CmdResult<()> {
    let layout = crate::find_layout(device)?;
    let js = crate::storage::store(&app)?.effect_js(&crate::storage::EffectKey::parse(&id)?)?;

    let params = serialised(&params)?;

    // The handle is shared with the loop, not copied: closing the device later
    // — ignored, unplugged — shows on the next frame.
    let out = Box::new(state.handle(device));
    state
        .engine
        .start(device, id.clone(), js, params, layout, out)
        .map_err(|error| not_started(&id, error))?;

    // **After** the start, never before: only what actually runs is
    // remembered. An effect whose load fails must not leave behind an id that
    // the file presents as applied.
    remember_active_effect(&app, device, Some(&id));
    Ok(())
}

/// Starts `effect` on `device` with the settings saved for it on that device.
///
/// What the tray and resuming do, through [`start_effect`] itself: neither may
/// start an effect differently from the gallery.
pub(crate) fn start_saved(app: &AppHandle, device: DeviceRef, effect: &str) -> CmdResult<()> {
    let store = crate::storage::store(app)?;
    let entry = store
        .list_effects()?
        .into_iter()
        .find(|e| e.id == effect)
        .ok_or_else(|| Failure::new("effectNotFound").with("name", effect))?;
    let settings = store.read_settings()?;
    let params = crate::storage::starting_params(
        &entry.manifest,
        settings.effect_params(device.vid, device.pid, effect),
    );
    start_effect(
        app.clone(),
        app.state(),
        device,
        effect.to_owned(),
        serde_json::Value::Object(params),
    )
}

/// Gives a device that just opened the effect applied on it (#102): at startup,
/// on adoption, and on replug.
///
/// Nothing happens when the setting is off, when nothing is applied, or when that
/// effect already runs on the device — a replug finds the loop still writing to
/// the reopened handle.
///
/// An effect that cannot start keeps its record: deleted, it is the gallery's
/// "no longer in the folder" notice; edited outside Candeo, it waits for the
/// window to compile it, and [`resume_waiting`] tries again then.
pub(crate) fn resume_applied(app: &AppHandle, device: DeviceRef) {
    let settings = match crate::storage::store(app).and_then(|s| s.read_settings()) {
        Ok(settings) => settings,
        Err(e) => {
            tracing::warn!(device = %device, "applied effect not resumed: {e}");
            return;
        }
    };
    let running = app
        .state::<AppState>()
        .engine
        .device_status()
        .into_iter()
        .find(|s| s.device == device && s.status.running)
        .and_then(|s| s.status.effect_id);
    let Some(effect) = to_resume(&settings, device, running.as_deref()) else {
        return;
    };
    match start_saved(app, device, &effect) {
        Ok(()) => tracing::info!(device = %device, effect, "applied effect resumed"),
        Err(e) if e.code == "effectNotCompiled" => {
            tracing::info!(device = %device, effect, "applied effect waits for compilation")
        }
        Err(e) => tracing::warn!(device = %device, effect, "applied effect not resumed: {e}"),
    }
}

/// The effect to start on `device`, given what runs there now: the applied one,
/// unless the setting is off or it already runs.
fn to_resume(
    settings: &crate::storage::Settings,
    device: DeviceRef,
    running: Option<&str>,
) -> Option<String> {
    if !settings.preferences.resume_effects {
        return None;
    }
    let applied = settings.active_effect(device.vid, device.pid)?;
    (running != Some(applied)).then(|| applied.to_owned())
}

/// Resumes, on every open device, `effect` if it is the one applied there: called
/// once the window has compiled it.
pub(crate) fn resume_waiting(app: &AppHandle, effect: &str) {
    let Ok(settings) = crate::storage::store(app).and_then(|s| s.read_settings()) else {
        return;
    };
    for device in app.state::<AppState>().open_devices() {
        if settings.active_effect(device.vid, device.pid) == Some(effect) {
            resume_applied(app, device);
        }
    }
}

/// The parameters as the loop reads them.
fn serialised(params: &serde_json::Value) -> CmdResult<String> {
    serde_json::to_string(params)
        .map_err(|e| Failure::unexpected(format!("parameters not serialisable: {e}")))
}

/// A load that failed: the effect's own error, for its author, in English.
fn not_started(id: &str, error: String) -> Failure {
    Failure::new("effectNotStarted")
        .with("name", id)
        .with("error", error)
}

#[tauri::command]
pub fn stop_effect(app: AppHandle, state: State<'_, AppState>, device: DeviceRef) {
    state.engine.stop(device);
    remember_active_effect(&app, device, None);
}

/// Remembers — or forgets, with `None` — the effect applied on this device.
///
/// # A failure is logged, not propagated
///
/// The effect runs, the keyboard is lit: making "Appliquer" (Apply) fail because
/// the disk did not take note would make the lighting pay for an incident that
/// does not concern it. What is lost is bounded and fits in one line — the file
/// will not resume this effect later.
pub(crate) fn remember_active_effect(app: &AppHandle, device: DeviceRef, effect: Option<&str>) {
    let write = || -> CmdResult<()> {
        let store = crate::storage::store(app)?;
        let mut settings = store.read_settings()?;
        // Nothing new: do not go through the temporary file and its rename
        // again. Starting the same effect twice is a double click.
        if settings.set_active_effect(device.vid, device.pid, effect) {
            store.write_settings(&settings)?;
        }
        Ok(())
    };
    if let Err(e) = write() {
        tracing::warn!(device = %device, "applied effect not remembered: {e}");
    }
}

// ---------------------------------------------------------------- preview

/// Starts the preview of an effect, **without touching the keyboard or the
/// disk**.
///
/// It is the exact counterpart of [`start_effect`], minus everything that
/// commits: no hardware output, no write to `settings.json`, and above all **no
/// device loop stopped**. Selecting an effect in the gallery goes through here;
/// the effect running on the keyboard keeps running.
///
/// `device` names the device whose **layout is borrowed**. `None` falls back to
/// the default layout: one previews without owning the keyboard, and without
/// having adopted any — the same reason `get_default_layout` exists.
#[tauri::command]
pub fn start_preview(
    app: AppHandle,
    state: State<'_, AppState>,
    device: Option<DeviceRef>,
    id: String,
    params: serde_json::Value,
) -> CmdResult<()> {
    let layout = match device {
        Some(d) => crate::find_layout(d)?,
        None => crate::default_layout(),
    };
    let js = crate::storage::store(&app)?.effect_js(&crate::storage::EffectKey::parse(&id)?)?;
    let params = serialised(&params)?;

    state
        .engine
        .start_preview(DeviceRef::of(layout), id.clone(), js, params, layout)
        .map_err(|error| not_started(&id, error))
}

#[tauri::command]
pub fn stop_preview(state: State<'_, AppState>) {
    state.engine.stop_preview();
}

/// Adjusts the preview parameters live, as [`set_effect_params`] does for a
/// device. The loop re-reads the JSON on every frame.
#[tauri::command]
pub fn set_preview_params(state: State<'_, AppState>, params: serde_json::Value) -> CmdResult<()> {
    state.engine.set_preview_params(serialised(&params)?);
    Ok(())
}

/// Opens the preview frame stream to the simulator.
///
/// A channel separate from the devices' one, and that is what allows watching
/// an effect while another one runs on the keyboard: both streams exist at the
/// same time, and the window picks which one it shows.
#[tauri::command]
pub fn subscribe_preview_frames(state: State<'_, AppState>, channel: Channel<InvokeResponseBody>) {
    state.engine.set_preview_channel(Some(channel));
}

#[tauri::command]
pub fn unsubscribe_preview_frames(state: State<'_, AppState>) {
    state.engine.set_preview_channel(None);
}

/// Adjusts the parameters live. The loop does not restart: it re-reads the
/// JSON on every frame.
#[tauri::command]
pub fn set_effect_params(
    state: State<'_, AppState>,
    device: DeviceRef,
    params: serde_json::Value,
) -> CmdResult<()> {
    state.engine.set_params(device, serialised(&params)?);
    Ok(())
}

/// Turns a device's keyboard output on or off, without touching the simulator.
#[tauri::command]
pub fn set_output_to_keyboard(state: State<'_, AppState>, device: DeviceRef, on: bool) {
    state.engine.set_to_keyboard(device, on);
}

/// Opens the frame stream of **one device** to the simulator.
///
/// A channel, not a global event: the destination is known, the scope is
/// explicit, and the binary goes through raw. Releasing the channel on the
/// front end, or calling [`unsubscribe_frames`], stops the stream **without
/// stopping the effect**, which keeps feeding the keyboard with the window
/// closed.
///
/// The simulator follows the selected device: changing the selection means
/// subscribing elsewhere, not multiplexing a single stream.
#[tauri::command]
pub fn subscribe_frames(
    state: State<'_, AppState>,
    device: DeviceRef,
    channel: Channel<InvokeResponseBody>,
) {
    state.engine.set_channel(device, Some(channel));
}

#[tauri::command]
pub fn unsubscribe_frames(state: State<'_, AppState>, device: DeviceRef) {
    state.engine.set_channel(device, None);
}

/// Engine state: what runs **on the devices**, and what is **being watched**.
///
/// Polled rather than pushed: an error raised while the window was closed must
/// be readable when it reopens, which a one-off event does not allow.
///
/// `devices` holds one entry per device targeted since startup — not only per
/// open device, nor per running loop: "this device is doing nothing" and "I
/// know nothing about this device" are not the same statement.
///
/// `preview` is **separate**, and the interface cannot confuse the two: see
/// [`PreviewStatus`].
#[tauri::command]
pub fn engine_status(state: State<'_, AppState>) -> EngineReport {
    state.engine.report()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal effect, written the way a user would write it.
    const EFFECT: &str = r#"
        import { hsv } from '@candeo/effects-api'
        export default {
          name: 'Test',
          render({ layout, time, frame, params }) {
            for (const key of layout.keys) {
              frame.set(key, hsv(time * Number(params.speed ?? 0) + key.col * 10, 1, 1))
            }
          },
        }
    "#;

    fn layout() -> &'static Layout {
        &candeo_device::DEATHSTALKER_V2_PRO
    }

    /// `unwrap_err` would require `(Runtime, Context)` to be `Debug`, which
    /// rquickjs does not provide.
    fn load_error(js: &str) -> String {
        match prepare(js, layout()) {
            Ok(_) => panic!("loading should have failed"),
            Err(e) => e,
        }
    }

    // ------------------------------------------------------------ one device

    /// Two made-up devices: the only real layout is unique, and everything that
    /// is "per device" only makes sense from two onwards. They only exist to be
    /// told apart — the geometry stays that of the real layout, so that the
    /// effects render real frames.
    const FIRST: DeviceRef = DeviceRef {
        vid: 0x1532,
        pid: 0x1111,
    };
    const SECOND: DeviceRef = DeviceRef {
        vid: 0x1532,
        pid: 0x2222,
    };

    /// #102: a device that opens gets its applied effect back, unless the setting
    /// is off, nothing is applied there, or that effect already runs.
    #[test]
    fn the_applied_effect_is_resumed_unless_it_runs_or_resuming_is_off() {
        let mut settings = crate::storage::Settings::default();
        settings.set_active_effect(FIRST.vid, FIRST.pid, Some("Rain"));

        assert_eq!(to_resume(&settings, FIRST, None), Some("Rain".into()));
        assert_eq!(
            to_resume(&settings, FIRST, Some("Swirl")),
            Some("Rain".into())
        );
        assert_eq!(to_resume(&settings, FIRST, Some("Rain")), None);
        assert_eq!(to_resume(&settings, SECOND, None), None);

        settings.preferences.resume_effects = false;
        assert_eq!(to_resume(&settings, FIRST, None), None);
    }

    /// Beyond this, the expected condition is considered not to be coming.
    ///
    /// Generous, and on purpose: at 30 frames per second a few frames fit in a
    /// few tens of milliseconds, but the pace of a CI runner is not that of a
    /// development machine. A test that sleeps for a chosen duration would be
    /// either slow or flaky.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// A device output driven from the test.
    ///
    /// That is the whole point of [`DeviceOut`]: making **one** device fail
    /// without plugging any in. With the `Keyboard` hard-wired, the invariant "a
    /// failing device affects no other" could only have been checked with two
    /// keyboards on the desk, so never.
    #[derive(Default)]
    struct Output {
        written: AtomicU32,
        failing: AtomicBool,
        /// Writes still to fail before this output recovers on its own.
        transient_failures: AtomicU32,
        /// Set when the loop dropped the device, as the real handle does.
        closed: AtomicBool,
        /// Set when the loop turned the backlight off.
        turned_off: AtomicBool,
        /// The last frame written.
        last: Mutex<Vec<Rgb>>,
    }

    impl DeviceOut for Arc<Output> {
        fn present(&self, colors: &[Rgb], abandon: bool) -> Option<Result<(), String>> {
            if self.closed.load(Ordering::Relaxed) {
                return None;
            }
            let transient = self
                .transient_failures
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok();
            if transient || self.failing.load(Ordering::Relaxed) {
                if abandon {
                    self.closed.store(true, Ordering::Relaxed);
                }
                return Some(Err("write refused by the device".into()));
            }
            *self.last.lock().unwrap() = colors.to_vec();
            self.written.fetch_add(1, Ordering::Relaxed);
            Some(Ok(()))
        }

        fn turn_off(&self) {
            self.turned_off.store(true, Ordering::Relaxed);
        }
    }

    /// #72: a device whose writes keep failing is **closed** after
    /// [`MAX_DEVICE_WRITE_ERRORS`] frames. An unplugged keyboard leaves a dead
    /// handle, and keeping it made every view report the device as open. The
    /// loop itself goes on — the simulator still animates — and the error kept
    /// for the window says the device was closed.
    #[test]
    fn a_device_that_keeps_failing_is_closed_but_the_loop_goes_on() {
        let engine = Engine::default();
        let broken = Arc::new(Output::default());
        broken.failing.store(true, Ordering::Relaxed);

        start(&engine, FIRST, "broken", Arc::clone(&broken));
        wait_for("the failing device was never closed", || {
            broken.closed.load(Ordering::Relaxed)
        });

        let status = status(&engine, FIRST);
        assert!(status.running, "closing the device stopped the loop");
        assert!(!status.reaching_keyboard);
        assert_eq!(status.device_error, Some(Failure::new("deviceClosed")));

        engine.stop(FIRST);
    }

    /// A few failures below the threshold close nothing: a transient error
    /// must not cost a manual reconnection, and a success clears the count.
    #[test]
    fn a_transient_write_failure_does_not_close_the_device() {
        let engine = Engine::default();
        let output = Arc::new(Output::default());
        output.transient_failures.store(3, Ordering::Relaxed);

        start(&engine, FIRST, "hiccup", Arc::clone(&output));
        wait_for("writing never recovered", || {
            output.written.load(Ordering::Relaxed) >= 3
        });

        assert!(
            !output.closed.load(Ordering::Relaxed),
            "closed on a transient failure"
        );
        let status = status(&engine, FIRST);
        assert!(status.reaching_keyboard);
        assert_eq!(status.device_error, None);

        engine.stop(FIRST);
    }

    fn start(engine: &Engine, device: DeviceRef, effect_id: &str, out: Arc<Output>) {
        engine
            .start(
                device,
                effect_id.into(),
                EFFECT.into(),
                "{}".into(),
                layout(),
                Box::new(out),
            )
            .expect("start");
    }

    /// A device's status, taken from the list the engine returns.
    fn status(engine: &Engine, device: DeviceRef) -> EngineStatus {
        engine
            .device_status()
            .into_iter()
            .find(|s| s.device == device)
            .unwrap_or_else(|| panic!("no status for {device}"))
            .status
    }

    /// Waits for a condition to come true, or fails. See [`PATIENCE`].
    fn wait_for(what: &str, ready: impl FnMut() -> bool) {
        wait_at_most(PATIENCE, what, ready);
    }

    /// Like [`wait_for`], but with a patience computed from the limit under
    /// test.
    ///
    /// [`PATIENCE`] is a fixed duration, chosen for conditions that come true
    /// within a few frames. A limit, on the other hand, promises a duration:
    /// stopping a looping effect takes [`MAX_CONSECUTIVE_ERRORS`] cut frames,
    /// and a cut frame lasts the whole [`FRAME_BUDGET`] — which is not the same
    /// from one build profile to the other. Waiting for a multiple of what is
    /// under test keeps the test right in both cases.
    fn wait_at_most(patience: Duration, what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + patience;
        while Instant::now() < deadline {
            if ready() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("{what}: nothing in {patience:?}");
    }

    /// **The adoption invariant, at the engine level.**
    ///
    /// `a_failing_device_blocks_no_other` checks it at open time;
    /// this one checks it once the loops are running, where the device fails
    /// while in use. A device whose every write fails must take nothing away
    /// from the others: not their loop, not their frames, not their state — and
    /// stopping it must not stop them.
    #[test]
    fn a_broken_device_affects_no_other() {
        let engine = Engine::default();
        let broken = Arc::new(Output::default());
        broken.failing.store(true, Ordering::Relaxed);
        let healthy = Arc::new(Output::default());

        start(&engine, FIRST, "casse", Arc::clone(&broken));
        start(&engine, SECOND, "sain", Arc::clone(&healthy));

        wait_for("the healthy neighbor writes nothing", || {
            healthy.written.load(Ordering::Relaxed) >= 3
        });

        let failed = status(&engine, FIRST);
        assert!(failed.running, "the broken device's loop stopped");
        assert!(
            failed.device_error.is_some(),
            "the write failure was not recorded"
        );
        assert!(!failed.reaching_keyboard);
        assert_eq!(broken.written.load(Ordering::Relaxed), 0);

        let ok = status(&engine, SECOND);
        assert!(ok.running, "the neighbor's loop stopped");
        assert_eq!(ok.device_error, None, "the neighbor's failure spilled over");
        assert!(ok.reaching_keyboard, "the neighbor is no longer reached");
        assert_eq!(
            ok.error, None,
            "effect error on the neighbor: {:?}",
            ok.error
        );

        // Stopping the broken device leaves the other one running: the loops
        // share no thread, no lock and no state.
        let before = healthy.written.load(Ordering::Relaxed);
        engine.stop(FIRST);
        wait_for("the neighbor stopped along with it", || {
            healthy.written.load(Ordering::Relaxed) > before
        });
        assert!(!status(&engine, FIRST).running);
        assert!(status(&engine, SECOND).running);

        engine.stop(SECOND);
    }

    /// Each device carries its own effect and output. Turning one off does not
    /// turn off the other — otherwise "envoyer au clavier" (send to keyboard)
    /// would be a global switch disguised as a device setting.
    #[test]
    fn each_device_carries_its_own_effect_and_output() {
        let engine = Engine::default();
        let a = Arc::new(Output::default());
        let b = Arc::new(Output::default());

        start(&engine, FIRST, "premier", Arc::clone(&a));
        start(&engine, SECOND, "second", Arc::clone(&b));

        assert_eq!(status(&engine, FIRST).effect_id.as_deref(), Some("premier"));
        assert_eq!(status(&engine, SECOND).effect_id.as_deref(), Some("second"));

        engine.set_to_keyboard(SECOND, false);
        wait_for("the second output stays on", || {
            !status(&engine, SECOND).reaching_keyboard
        });

        let frozen = b.written.load(Ordering::Relaxed);
        let before = a.written.load(Ordering::Relaxed);
        wait_for("the first no longer writes", || {
            a.written.load(Ordering::Relaxed) > before + 2
        });

        assert!(status(&engine, FIRST).to_keyboard, "the cut spilled over");
        assert!(status(&engine, FIRST).reaching_keyboard);
        assert_eq!(
            b.written.load(Ordering::Relaxed),
            frozen,
            "the output turned off still writes"
        );

        engine.stop(FIRST);
        engine.stop(SECOND);
    }

    /// A stopped device keeps its line: "this device is doing nothing" and "I
    /// know nothing about this device" are not the same statement, and the
    /// interface must be able to tell them apart.
    #[test]
    fn a_stopped_device_keeps_its_status_line() {
        let engine = Engine::default();
        start(&engine, FIRST, "premier", Arc::new(Output::default()));
        engine.stop(FIRST);

        let s = status(&engine, FIRST);
        assert!(!s.running);
        assert_eq!(s.effect_id, None);
        assert_eq!(engine.device_status().len(), 1);
    }

    /// **What deleting an effect must get from the engine.**
    ///
    /// A deleted effect may be running on several devices, and it must stop on
    /// all of them — a forgotten loop would keep running an `effect.js` loaded
    /// into memory, with no visible error, while its folder no longer exists.
    /// The loops running **something else** are not concerned: deleting an
    /// effect does not turn off the other keyboards.
    #[test]
    fn stopping_an_effect_stops_it_everywhere_and_nowhere_else() {
        let engine = Engine::default();
        let neighbor = Arc::new(Output::default());

        start(&engine, FIRST, "a-supprimer", Arc::new(Output::default()));
        start(&engine, SECOND, "autre", Arc::clone(&neighbor));

        let stopped = engine.stop_everywhere("a-supprimer");
        assert_eq!(stopped, vec![FIRST]);

        let deleted = status(&engine, FIRST);
        assert!(!deleted.running);
        assert_eq!(
            deleted.effect_id, None,
            "the status line still names an effect that no longer exists"
        );

        // The neighbor, for its part, noticed nothing: it is still running, and
        // its frames keep going out.
        let before = neighbor.written.load(Ordering::Relaxed);
        assert!(status(&engine, SECOND).running);
        wait_for("the neighbor stopped along with it", || {
            neighbor.written.load(Ordering::Relaxed) > before
        });

        engine.stop(SECOND);
    }

    /// The same effect on two devices: both loops go.
    #[test]
    fn stopping_an_effect_covers_every_device_running_it() {
        let engine = Engine::default();
        start(&engine, FIRST, "partout", Arc::new(Output::default()));
        start(&engine, SECOND, "partout", Arc::new(Output::default()));

        assert_eq!(engine.stop_everywhere("partout"), vec![FIRST, SECOND]);
        assert!(!status(&engine, FIRST).running);
        assert!(!status(&engine, SECOND).running);
    }

    /// An id that nothing is running stops nothing. That is the common case:
    /// deleting an effect that was never applied.
    #[test]
    fn stopping_an_effect_running_nowhere_touches_nothing() {
        let engine = Engine::default();
        start(&engine, FIRST, "en-cours", Arc::new(Output::default()));

        assert!(engine.stop_everywhere("jamais-lance").is_empty());
        assert!(status(&engine, FIRST).running);

        engine.stop(FIRST);
    }

    /// What resetting the configuration expects from the engine: not a single
    /// loop left, whatever the effect and whatever the device — preview
    /// included, which writes to nothing but keeps a QuickJS context alive.
    #[test]
    fn stopping_everything_leaves_no_loop() {
        let engine = Engine::default();
        start(&engine, FIRST, "premier", Arc::new(Output::default()));
        start(&engine, SECOND, "second", Arc::new(Output::default()));
        preview_effect(&engine, FIRST, "regarde");

        engine.stop_all();

        assert!(engine.device_status().iter().all(|s| !s.status.running));
        // The lines stay: "this device is doing nothing" and "I know nothing
        // about this device" are not the same statement, reset or not.
        assert_eq!(engine.device_status().len(), 2);
        assert!(engine.preview_status().is_none());
    }

    // ------------------------------------------------------------ preview

    fn preview_effect(engine: &Engine, layout_of: DeviceRef, effect_id: &str) {
        engine
            .start_preview(
                layout_of,
                effect_id.into(),
                EFFECT.into(),
                "{}".into(),
                layout(),
            )
            .expect("preview start");
    }

    /// **The heart of issue #63.** Previewing Y while X runs on the keyboard
    /// must stop nothing: otherwise browsing the gallery would turn off the
    /// current lighting, and it would only show once shipped.
    #[test]
    fn the_preview_does_not_interrupt_the_device_effect() {
        let engine = Engine::default();
        let output = Arc::new(Output::default());
        start(&engine, FIRST, "applique", Arc::clone(&output));

        // Three previews in a row, like browsing the gallery.
        for effect in ["regarde-1", "regarde-2", "regarde-3"] {
            preview_effect(&engine, FIRST, effect);
        }

        let device = status(&engine, FIRST);
        assert!(device.running, "the preview stopped the device's loop");
        assert_eq!(device.effect_id.as_deref(), Some("applique"));

        // And frames keep going out to the keyboard during the preview.
        let before = output.written.load(Ordering::Relaxed);
        wait_for("the keyboard is no longer fed", || {
            output.written.load(Ordering::Relaxed) > before + 2
        });

        engine.stop_all();
    }

    /// **Not a single byte goes to a keyboard from a preview.** That is what
    /// [`NoOutput`] guarantees, and it is the only guarantee that matters: an
    /// output opened by accident would turn the preview into an application.
    #[test]
    fn the_preview_writes_to_no_device() {
        let engine = Engine::default();
        let output = Arc::new(Output::default());
        start(&engine, FIRST, "applique", Arc::clone(&output));
        engine.stop(FIRST);

        let frozen = output.written.load(Ordering::Relaxed);
        preview_effect(&engine, FIRST, "regarde");
        wait_for("the preview rendered no frame", || {
            engine
                .preview_status()
                .is_some_and(|p| p.running && p.error.is_none())
        });
        // A few frames go by while the test sleeps.
        std::thread::sleep(Duration::from_millis(120));

        assert_eq!(
            output.written.load(Ordering::Relaxed),
            frozen,
            "the preview wrote to the device's output"
        );
        engine.stop_all();
    }

    /// The preview is not mistaken for any device: it adds no line to the list
    /// that the tray icon and the diagnostic read.
    #[test]
    fn the_preview_is_not_in_the_device_list() {
        let engine = Engine::default();
        preview_effect(&engine, FIRST, "regarde");

        assert!(
            engine.device_status().is_empty(),
            "the preview slipped into the device list"
        );

        let report = engine.report();
        assert!(report.devices.is_empty());
        let preview = report.preview.expect("no preview");
        assert_eq!(preview.effect_id.as_deref(), Some("regarde"));
        assert_eq!(
            preview.layout_of, FIRST,
            "the borrowed layout is not the one requested"
        );

        engine.stop_preview();
        assert!(
            engine.preview_status().is_none(),
            "a stopped preview is still announced"
        );
    }

    /// Deleting an effect also stops the **preview** running it: the
    /// JavaScript is loaded into memory, and the screen would keep animating an
    /// effect missing from the library. And it does not appear in the list of
    /// stopped devices — no device was touched.
    #[test]
    fn deleting_an_effect_also_stops_its_preview() {
        let engine = Engine::default();
        start(&engine, FIRST, "autre", Arc::new(Output::default()));
        preview_effect(&engine, FIRST, "a-supprimer");

        assert!(engine.stop_everywhere("a-supprimer").is_empty());
        assert!(engine.preview_status().is_none());
        assert!(
            status(&engine, FIRST).running,
            "the device's loop was stopped along the way"
        );

        engine.stop_all();
    }

    /// Two selections in a row: the second **replaces** the first, it does not
    /// add to it. There is only one preview loop, hence only one QuickJS context
    /// at a time.
    #[test]
    fn changing_the_selection_replaces_the_preview() {
        let engine = Engine::default();
        preview_effect(&engine, FIRST, "premier-regarde");
        preview_effect(&engine, SECOND, "second-regarde");

        let preview = engine.preview_status().expect("no preview");
        assert_eq!(preview.effect_id.as_deref(), Some("second-regarde"));
        assert_eq!(preview.layout_of, SECOND);
        assert!(engine.device_status().is_empty());

        engine.stop_preview();
    }

    // ------------------------------------------------------------ key presses

    /// Lights every key pressed, white.
    const READS_KEYS: &str = r#"
        export default {
          inputs: ['keys'],
          render({ presses, frame }) {
            for (const { key } of presses) frame.set(key, { r: 255, g: 255, b: 255 })
          },
        }
    "#;

    /// An engine whose presses come from the test, not from the keyboard.
    fn engine_with(presses: &Arc<Presses>) -> Engine {
        let mut engine = Engine::default();
        engine.presses = Arc::clone(presses);
        engine
    }

    fn lit(output: &Output, index: u16) -> bool {
        output
            .last
            .lock()
            .unwrap()
            .get(usize::from(index))
            .is_some_and(|c| c.r == 255)
    }

    /// A press lights its key on the keyboard it was typed on, and on no other:
    /// 0x11 is LED 46, 0x1E is LED 67.
    #[test]
    fn a_device_sees_the_presses_of_its_own_keyboard() {
        let presses = Presses::manual();
        let engine = engine_with(&presses);
        let output = Arc::new(Output::default());
        start_js(&engine, FIRST, READS_KEYS, Arc::clone(&output)).expect("start");
        wait_for("the loop never read presses", || presses.readers() == 1);

        presses.push(presses::Press {
            at: Instant::now(),
            scancode: 0x11,
            device: Some((FIRST.vid, FIRST.pid)),
        });
        presses.push(presses::Press {
            at: Instant::now(),
            scancode: 0x1E,
            device: Some((SECOND.vid, SECOND.pid)),
        });

        wait_for("the pressed key never lit", || lit(&output, 46));
        assert!(
            !lit(&output, 67),
            "a press from another keyboard lit this one"
        );
        engine.stop(FIRST);
    }

    /// Ripples draws its ring from the pressed key: at the instant of the press,
    /// the key itself takes the ring color; at rest, the background.
    #[test]
    fn ripples_starts_its_ring_on_the_pressed_key() {
        let (_rt, ctx) = prepare(crate::shipped::source("Ripples"), layout()).expect("load");
        let len = layout().led_count();
        let rest = render_once(&ctx, 1.0, 0, "{}", len).expect("render");

        let now = Instant::now();
        let press = presses::Press {
            at: now,
            scancode: 0x11,
            device: None,
        };
        let pressed = presses::positions(layout()).to_json(&[press], now, 1.0);
        let lit = render_with_presses(&ctx, 1.0, 1, "{}", &pressed, len).expect("render");

        let led = 46 * 3;
        assert_eq!(&rest[led..led + 3], &[6, 12, 28], "background at rest");
        assert_eq!(
            &lit[led..led + 3],
            &[64, 200, 255],
            "ring color on the pressed key"
        );
        assert_eq!(&lit[0..3], &[6, 12, 28], "Escape, far away, untouched yet");
    }

    /// Presses are read only while a loop runs an effect declaring them, the
    /// preview included, and no longer once the last one stops.
    #[test]
    fn presses_are_read_only_while_an_effect_declares_them() {
        let presses = Presses::manual();
        let engine = engine_with(&presses);

        start_js(&engine, FIRST, EFFECT, Arc::new(Output::default())).expect("start");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(presses.readers(), 0, "an effect that reads no keys");

        start_js(&engine, SECOND, READS_KEYS, Arc::new(Output::default())).expect("start");
        engine
            .start_preview(
                FIRST,
                "keys".into(),
                READS_KEYS.into(),
                "{}".into(),
                layout(),
            )
            .expect("preview start");
        wait_for("both loops read presses", || presses.readers() == 2);

        engine.stop(SECOND);
        assert_eq!(presses.readers(), 1);
        engine.stop_preview();
        assert_eq!(presses.readers(), 0);
        engine.stop(FIRST);
    }

    /// END TO END — writes to the REAL keyboard. `#[ignore]` by default.
    ///
    /// `cargo test -p candeo-desktop end_to_end -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn end_to_end_on_the_real_keyboard() {
        let api = hidapi::HidApi::new().expect("HID");
        let l = layout();
        let kb = match Keyboard::open(&api, l) {
            Ok(kb) => kb,
            Err(e) => panic!("cannot open: {e}"),
        };
        println!("keyboard open: {}", l.name);

        let device = DeviceRef {
            vid: l.vid,
            pid: l.pid,
        };
        let handle: Handle = Arc::new(Mutex::new(Some(kb)));
        let engine = Engine::default();
        let js = crate::shipped::source("Radial wave");

        engine
            .start(
                device,
                "onde-radiale".into(),
                js.to_string(),
                "{}".into(),
                l,
                Box::new(Arc::clone(&handle)),
            )
            .expect("start");
        println!("engine started — 3 s of radial wave on the keyboard");
        std::thread::sleep(Duration::from_secs(3));

        let s = status(&engine, device);
        println!("status: running={} error={:?}", s.running, s.error);
        assert!(s.running, "the loop stopped");
        assert!(s.error.is_none(), "error while rendering: {:?}", s.error);
        assert!(
            s.reaching_keyboard,
            "no frame reaches the keyboard: {:?}",
            s.device_error
        );

        engine.stop(device);
        println!("stopped cleanly");
    }

    #[test]
    fn an_effect_renders_a_full_frame() {
        let (_rt, ctx) = prepare(EFFECT, layout()).expect("load");
        let bytes = render_once(&ctx, 0.0, 0, "{}", layout().led_count()).expect("render");

        // 132 positions, not 106: a frame covers the whole matrix.
        assert_eq!(bytes.len(), 132 * 3);
    }

    #[test]
    fn positions_without_an_led_stay_black() {
        let (_rt, ctx) = prepare(EFFECT, layout()).expect("load");
        let bytes = render_once(&ctx, 1.0, 0, "{}", layout().led_count()).expect("render");

        // (0, 1) is a hole in the matrix — the effect iterates over
        // `layout.keys`, so it cannot reach it.
        let trou = 1usize;
        assert_eq!(&bytes[trou * 3..trou * 3 + 3], &[0, 0, 0]);

        // Escape, on the other hand, is lit.
        assert_ne!(&bytes[0..3], &[0, 0, 0]);
    }

    #[test]
    fn a_syntax_error_surfaces_at_load() {
        let err = load_error("this is not JavaScript {{{");
        assert!(
            err.contains("loading the effect"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn a_module_without_a_default_export_is_refused() {
        let err = load_error("export const x = 1");
        assert!(err.contains("default export"), "unexpected message: {err}");
    }

    /// An exception at run time must not bring the engine down: it surfaces as
    /// `Err`, and the loop counts it and carries on.
    #[test]
    fn a_runtime_exception_is_caught() {
        let js = "export default { name: 'X', render() { throw new Error('boum') } }";
        let (_rt, ctx) = prepare(js, layout()).expect("load");
        let err = render_once(&ctx, 0.0, 0, "{}", layout().led_count()).unwrap_err();
        assert!(err.contains("boum"), "unexpected message: {err}");
    }

    /// An out-of-range color must become a valid byte, not make the conversion
    /// fail far from its cause.
    #[test]
    fn out_of_range_colors_are_clamped() {
        let js = r#"
            export default {
              name: 'X',
              render({ layout, frame }) {
                for (const key of layout.keys) frame.set(key, { r: 999, g: -5, b: NaN })
              },
            }
        "#;
        let (_rt, ctx) = prepare(js, layout()).expect("load");
        let bytes = render_once(&ctx, 0.0, 0, "{}", layout().led_count()).expect("render");
        assert_eq!(&bytes[0..3], &[255, 0, 0]);
    }

    /// `api.js` and `packages/effects-api/src/index.ts` describe the same API.
    /// If a name disappears from here, the editor would promise a missing
    /// function.
    #[test]
    fn api_js_exports_match_the_typescript_surface() {
        let js = r#"
            import * as api from '@candeo/effects-api'
            export default {
              name: 'X',
              render({ frame }) {
                const manquants = ['rgb','hsv','mix','lerp','BLACK','defineEffect','center','bounds'].filter(n => api[n] === undefined)
                if (manquants.length) throw new Error('absents de api.js : ' + manquants.join(', '))
                frame.fill(api.BLACK)
              },
            }
        "#;
        let (_rt, ctx) = prepare(js, layout()).expect("load");
        render_once(&ctx, 0.0, 0, "{}", layout().led_count()).expect("render");
    }

    // ---------------------------------------------------------------- geometry

    /// The key rectangle makes it all the way to the effect, in the Rust unit.
    ///
    /// The space bar is the case that sums up the subject: **one** matrix cell,
    /// **6.25 u** of keycap. Without the rectangle, an effect cannot tell it
    /// apart from a letter key.
    #[test]
    fn the_layout_given_to_the_effect_carries_the_geometry() {
        let json: serde_json::Value =
            serde_json::from_str(&layout_json(layout())).expect("layout JSON");
        let keys = json["keys"].as_array().expect("keys");

        let space = keys
            .iter()
            .find(|k| k["scancode"] == 0x39)
            .expect("the space bar");
        assert_eq!(space["col"], 6, "a single matrix cell");
        assert_eq!(space["w"], 6.25, "and 6.25 u of keycap");
        assert_eq!(space["x"], 3.75);
        assert_eq!(space["y"], 5.5);
        assert_eq!(space["h"], 1.0);

        assert!(
            keys.iter().all(|k| k["w"].as_f64().unwrap_or(0.0) > 0.0),
            "a key without a width cannot be told apart from a key without geometry"
        );
    }

    /// An effect finds a key by its scancode, whatever the legend: 0x11 is the key
    /// right of the one right of Tab's, engraved Z on this AZERTY keyboard. Fn
    /// sends nothing, and carries no scancode rather than a false one.
    #[test]
    fn the_layout_given_to_the_effect_carries_scancodes() {
        let json: serde_json::Value =
            serde_json::from_str(&layout_json(layout())).expect("layout JSON");
        let keys = json["keys"].as_array().expect("keys");

        let z = keys.iter().find(|k| k["scancode"] == 0x11).expect("0x11");
        assert_eq!((z["row"].as_u64(), z["col"].as_u64()), (Some(2), Some(2)));
        let fn_key = keys.iter().find(|k| k["index"] == 121).expect("Fn");
        assert!(fn_key.get("scancode").is_none() && fn_key.get("label").is_none());
    }

    /// A layout whose arrangement nobody has drawn.
    ///
    /// There is none in this repository — `candeo_device::Key` carries a
    /// mandatory rectangle — but that is what a device contributed without the
    /// `geometry` capability of `docs/design/device-sdk.md` §3.2 will be. Two
    /// positions are enough: what is tested is the absence of the fields.
    const NO_GEOMETRY: &str = r#"{"name":"Undrawn layout","rows":1,"cols":2,"keys":[{"index":0,"row":0,"col":0,"label":"A"},{"index":1,"row":0,"col":1,"label":"B"}]}"#;

    /// **The pattern we are fighting.** Without geometry, `key.x` is
    /// `undefined`, the distance `NaN`, and the color would be clamped to zero:
    /// a black keyboard, without a single error, and a round of debugging to
    /// understand why. The effect must fail by naming what is missing.
    #[test]
    fn the_radial_wave_refuses_a_layout_without_geometry() {
        let js = crate::shipped::source("Radial wave");
        let (_rt, ctx) = prepare_with_layout(js, 2, NO_GEOMETRY.to_string(), None).expect("load");

        let err = render_once(&ctx, 0.0, 0, "{}", 2).unwrap_err();
        assert!(err.contains("geometry"), "unexpected message: {err}");
        assert!(
            err.contains('A') || err.contains('B'),
            "the message must name the offending key: {err}"
        );
    }

    /// And the diagonal wave, for its part, does run there: that is the whole
    /// point of having kept it rather than fixed it. A layout that has not been
    /// drawn keeps an effect.
    #[test]
    fn the_diagonal_wave_runs_without_geometry() {
        let js = crate::shipped::source("Diagonal wave");
        let (_rt, ctx) = prepare_with_layout(js, 2, NO_GEOMETRY.to_string(), None).expect("load");

        let bytes = render_once(&ctx, 0.0, 0, "{}", 2).expect("render");
        assert!(
            bytes.iter().any(|&c| c != 0),
            "the diagonal wave only needs `row` and `col`"
        );
    }

    // ------------------------------------------------------- execution limits

    /// A loop that never gives control back, written the way it gets written by
    /// accident: an exit condition that never comes.
    const ENDLESS_RENDER: &str = r#"
        export default {
          name: 'Sans fin',
          render() {
            let i = 0
            while (i >= 0) i += 1
          },
        }
    "#;

    /// A state that grows on every frame, and that nothing frees.
    const ENDLESS_ALLOCATION: &str = r#"
        const garde = []
        export default {
          name: 'Fuite',
          render() {
            garde.push(new Uint8Array(4 * 1024 * 1024))
          },
        }
    "#;

    /// Like [`start`], but with a given effect, and without requiring it to
    /// start.
    fn start_js(
        engine: &Engine,
        device: DeviceRef,
        js: &str,
        out: Arc<Output>,
    ) -> Result<(), String> {
        engine.start(
            device,
            "borne".into(),
            js.into(),
            "{}".into(),
            layout(),
            Box::new(out),
        )
    }

    /// Runs `f` on the side, and returns its result — or fails if it does not
    /// come back.
    ///
    /// Calling a function suspected of never returning directly gives a test
    /// that cannot fail: it hangs, and it is CI that ends up killing it, hours
    /// later. Here, the test decides. The thread left behind would spin in the
    /// void, but it prevents nothing from finishing — and a test that has
    /// already failed has nothing left to protect.
    fn without_hanging<T: Send + 'static>(
        patience: Duration,
        what: &str,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(patience)
            .unwrap_or_else(|_| panic!("{what}: nothing in {patience:?}"))
    }

    /// **An effect that never gives control back no longer freezes its
    /// thread.**
    ///
    /// This is the failure the limits exist to handle: without them, the `stop`
    /// flag would never be read again, and closing the window would not help —
    /// an effect runs with the window closed. The stop must therefore come from
    /// the engine, through the ordinary error path.
    #[test]
    fn an_effect_looping_on_every_frame_is_stopped_cleanly() {
        let engine = Engine::default();
        let out = Arc::new(Output::default());
        start_js(&engine, FIRST, ENDLESS_RENDER, Arc::clone(&out)).expect("start");

        // The test's safeguard: if nothing stopped the loop, this is what would
        // fail, rather than CI timing out hours later. Three times what the
        // limit promises — thirty cut frames, each taking its whole budget.
        wait_at_most(
            FRAME_BUDGET * MAX_CONSECUTIVE_ERRORS * 3,
            "the loop did not stop",
            || !status(&engine, FIRST).running,
        );

        let error = status(&engine, FIRST).error.expect("no error recorded");
        assert!(
            error.contains("computing time"),
            "the cause is not named: {error}"
        );
        assert_eq!(
            out.written.load(Ordering::Relaxed),
            0,
            "a frame came out of an effect that never finished one"
        );
        // #48: stopped on its own, the effect does not leave a frame that looks
        // like it still runs.
        assert!(out.turned_off.load(Ordering::Relaxed), "backlight left on");

        // The loop did give its thread back: otherwise this is where the test
        // would stop forever, waiting for it to end.
        engine.stop(FIRST);
    }

    /// **An effect that allocates endlessly now takes down only itself.**
    ///
    /// The first frames go through — the effect is allowed to keep state —,
    /// then the limit hits and exceeding it becomes a frame error like any
    /// other.
    #[test]
    fn an_effect_allocating_endlessly_is_stopped_cleanly() {
        let engine = Engine::default();
        let out = Arc::new(Output::default());
        start_js(&engine, FIRST, ENDLESS_ALLOCATION, Arc::clone(&out)).expect("start");

        wait_for("the loop did not stop", || !status(&engine, FIRST).running);

        let error = status(&engine, FIRST).error.expect("no error recorded");
        assert!(error.contains("memory"), "the cause is not named: {error}");

        engine.stop(FIRST);
    }

    /// **A loop outside `render` no longer blocks the start.**
    ///
    /// The module body runs at load time, outside any frame, and `start` waits
    /// for its verdict while holding the device lock: without a limit, that
    /// keyboard would never start or stop anything again.
    #[test]
    fn an_effect_looping_at_load_gives_control_back() {
        let error = without_hanging(LOAD_BUDGET * 3, "starting never returned", || {
            let engine = Engine::default();
            start_js(
                &engine,
                FIRST,
                "while (true) {}\nexport default { name: 'X', render() {} }",
                Arc::new(Output::default()),
            )
            .expect_err("loading should have been interrupted")
        });

        assert!(
            error.contains("computing time"),
            "the cause is not named: {error}"
        );
    }

    /// An ordinary exception keeps its message: the limits only explain what
    /// they cut, and a `throw` from the effect already speaks for itself.
    #[test]
    fn an_ordinary_exception_keeps_its_message() {
        let engine = Engine::default();
        let js = "export default { name: 'X', render() { throw new Error('boum') } }";
        start_js(&engine, FIRST, js, Arc::new(Output::default())).expect("start");

        wait_for("no error recorded", || {
            status(&engine, FIRST).error.is_some()
        });
        let error = status(&engine, FIRST).error.unwrap();
        assert!(error.contains("boum"), "message rewritten: {error}");
        assert!(!status(&engine, FIRST).reads_keys);

        engine.stop(FIRST);
    }

    /// #44: an effect reading key presses could write them into its error; the
    /// window still shows the text, the log and the diagnostic do not.
    #[test]
    fn the_error_of_an_effect_reading_keys_stays_out_of_the_log() {
        let engine = Engine::default();
        let js = "export default { name: 'X', inputs: ['keys'], render() { throw new Error('A pressed') } }";
        start_js(&engine, FIRST, js, Arc::new(Output::default())).expect("start");

        wait_for("no error recorded", || {
            status(&engine, FIRST).error.is_some()
        });
        let s = status(&engine, FIRST);
        assert!(s.reads_keys);
        let error = s.error.unwrap();
        assert!(
            error.contains("A pressed"),
            "the window lost the text: {error}"
        );
        assert!(!loggable(&error, s.reads_keys).contains("A pressed"));
        assert_eq!(loggable("boum", false), "boum");

        engine.stop(FIRST);
    }

    /// **The limits must strangle no honest effect.**
    ///
    /// The frame buffer costs nothing — `bootstrap.js` reuses it — but an effect
    /// is allowed to keep state and to keep it alive. Two thousand particles and
    /// a trail of one second of frames, for a keyboard with 132 LEDs, is
    /// outlandish on purpose: if the limits let that one through, they let
    /// through anything anyone will write.
    ///
    /// Rendering goes beyond the depth of the trail, to its steady state: a
    /// state that is only freed at the sixtieth frame does not show over thirty.
    ///
    /// The margin is checked, not only the success: an effect that passed by a
    /// hair would no longer pass on the machine next door.
    #[test]
    fn the_limits_let_a_stateful_effect_through() {
        let js = r#"
            const particules = Array.from({ length: 2000 }, (_, i) => ({
              x: (i * 7) % 20,
              y: (i * 3) % 6,
              vx: 0.11,
              vy: 0.07,
            }))
            const trainee = []

            export default {
              name: 'Particules',
              render({ layout, frame }) {
                for (const p of particules) {
                  p.x = (p.x + p.vx) % layout.cols
                  p.y = (p.y + p.vy) % layout.rows
                }
                // Une seconde d'images conservées, la plus ancienne libérée.
                trainee.push(particules.map((p) => (p.x + p.y) | 0))
                if (trainee.length > 60) trainee.shift()

                for (const key of layout.keys) {
                  frame.set(key, { r: (key.col * 8) % 256, g: (key.row * 40) % 256, b: 60 })
                }
              },
            }
        "#;

        let budget = Rc::new(Budget::default());
        budget.grant(LOAD_BUDGET);
        let (rt, ctx) = prepare_budgeted(js, layout(), &budget).expect("load");

        for frame in 0..90u32 {
            budget.grant(FRAME_BUDGET);
            render_once(
                &ctx,
                f64::from(frame) / f64::from(FPS),
                frame,
                "{}",
                layout().led_count(),
            )
            .unwrap_or_else(|e| panic!("frame {frame}: {e}"));
        }

        let used = rt.memory_usage().malloc_size as usize;
        assert!(
            used * 4 < MEMORY_BUDGET,
            "a stateful effect is close to the memory limit: {used} bytes of {MEMORY_BUDGET}"
        );
    }

    // ------------------------------------------------------------ shipped effects
    //
    // Shipped effects go through the same engine as the user's, hence through
    // the same tests. A broken shipped effect must not be discovered at run
    // time, by whoever opens it first.

    /// Sampling times. Several, and not only zero: a division by a cycle's
    /// duration or an overrun past the last row only shows once the animation
    /// has started.
    const SAMPLE_TIMES: [f64; 4] = [0.0, 0.4, 1.3, 2.7];

    #[test]
    fn every_shipped_effect_renders_a_full_frame() {
        for b in &crate::shipped::ALL {
            let (_rt, ctx) =
                prepare(b.source, layout()).unwrap_or_else(|e| panic!("{}: {e}", b.name));

            for (i, time) in SAMPLE_TIMES.iter().enumerate() {
                let bytes = render_once(&ctx, *time, i as u32, "{}", layout().led_count())
                    .unwrap_or_else(|e| panic!("{} at t={time}: {e}", b.name));

                assert_eq!(bytes.len(), 132 * 3, "{} at t={time}", b.name);
                // (0, 1) is a hole in the matrix. An effect that reaches it
                // does not iterate over `layout.keys`: it works on the 132
                // cells instead of the 106 lit positions.
                assert_eq!(
                    &bytes[3..6],
                    &[0, 0, 0],
                    "{} writes to a position without an LED",
                    b.name
                );
            }
        }
    }

    /// A shipped effect must be visible from its first frame: a black frame at
    /// start looks like an effect that did not start.
    #[test]
    fn every_shipped_effect_lights_something_from_the_first_frame() {
        for b in &crate::shipped::ALL {
            let (_rt, ctx) =
                prepare(b.source, layout()).unwrap_or_else(|e| panic!("{}: {e}", b.name));
            let bytes = render_once(&ctx, 0.0, 0, "{}", layout().led_count()).expect("render");

            assert!(
                bytes.iter().any(|&c| c != 0),
                "{} renders an entirely black frame",
                b.name
            );
        }
    }

    // ------------------------------------------------- the two waves, side by side
    //
    // Two pairs of keys from the **real** layout, chosen so that each one tells
    // the two spaces apart. Nothing is simulated here: the geometry comes from
    // `layout.rs`, and that is what makes the check possible without a keyboard.

    /// The color of an LED in a rendered frame.
    fn color_at(frame: &[u8], index: usize) -> &[u8] {
        &frame[index * 3..index * 3 + 3]
    }

    /// The first frame of a shipped effect, on the default layout.
    fn first_frame(name: &str) -> Vec<u8> {
        let js = crate::shipped::source(name);
        let (_rt, ctx) = prepare(js, layout()).unwrap_or_else(|e| panic!("{name}: {e}"));
        render_once(&ctx, 0.0, 0, "{}", layout().led_count()).expect("render")
    }

    /// "L" (index 75) and "ù" (index 77) are at the **same physical distance**
    /// from the center of the drawing — 1 u on either side, 0.75 u lower.
    ///
    /// The radial wave must therefore paint them the same color. That is the
    /// definition of "radial", checked rather than announced.
    #[test]
    fn the_radial_wave_measures_physical_distance() {
        let radial = first_frame("Radial wave");
        assert_eq!(
            color_at(&radial, 75),
            color_at(&radial, 77),
            "two keys at equal physical distance must have the same color"
        );
    }

    /// "L" (index 75) and "*" (index 78) are at 1.25 u and 2.14 u from the center
    /// of the drawing, because the row is staggered and the L-shaped Enter key
    /// does not fall on the grid: the radial wave tells them apart.
    #[test]
    fn the_radial_wave_follows_the_staggered_rows() {
        let radial = first_frame("Radial wave");
        assert_ne!(
            color_at(&radial, 75),
            color_at(&radial, 78),
            "physically, they are not at equal distance"
        );
    }

    /// **The diagonal wave leaves the top-left corner.** Any wave
    /// measured from the center looked like the radial one on a keyboard 22 cells
    /// wide and 6 high: both became near-vertical bands. Counting steps from a
    /// corner changes the motion itself.
    ///
    /// Checked on every pair of keys of the real layout: the same diagonal
    /// (column + row) gives the same color, and mirrored keys, at equal distance
    /// from the center, no longer do.
    #[test]
    fn the_diagonal_wave_starts_from_the_corner() {
        let diagonal = first_frame("Diagonal wave");
        let l = layout();
        let keys: Vec<(u16, u8, u8)> = (0..l.rows)
            .flat_map(|row| (0..l.cols).map(move |col| (row, col)))
            .filter_map(|(row, col)| l.at(row, col).map(|index| (index, row, col)))
            .collect();

        for (i, &(a, a_row, a_col)) in keys.iter().enumerate() {
            for &(b, b_row, b_col) in &keys[i + 1..] {
                if a_row + a_col == b_row + b_col {
                    assert_eq!(
                        color_at(&diagonal, a.into()),
                        color_at(&diagonal, b.into()),
                        "LEDs {a} and {b} are on the same diagonal"
                    );
                }
            }
        }

        // "L" (row 3, column 9) and "*" (row 3, column 12) mirror each other
        // around the center: a centered wave painted them alike.
        assert_ne!(
            color_at(&diagonal, 75),
            color_at(&diagonal, 78),
            "the wave must not be symmetric around the center any more"
        );
    }

    /// The bands move **away** from the corner. With speed and scale equal, one
    /// second moves them by exactly one step: "L" (row 3, column 9) then shows
    /// the color "K" (row 3, column 8) had a second earlier.
    #[test]
    fn the_diagonal_wave_moves_away_from_the_corner() {
        let js = crate::shipped::source("Diagonal wave");
        let (_rt, ctx) = prepare(js, layout()).expect("load");
        let params = r#"{"speed":18,"scale":18}"#;
        let len = layout().led_count();
        let before = render_once(&ctx, 0.0, 0, params, len).expect("render");
        let after = render_once(&ctx, 1.0, 1, params, len).expect("render");

        assert_eq!(
            color_at(&after, 75),
            color_at(&before, 74),
            "one second later, the color one step closer to the corner has moved on"
        );
    }

    /// The space bar is painted according to **the middle of its keycap**.
    ///
    /// One matrix cell, 6.25 u wide: its center is at 6.875 u, not at the left
    /// edge (3.75 u) nor at column 6. The key of the row above whose keycap is
    /// centered at the same place — "B", at 6.75 u — must therefore be at almost
    /// the same distance from the center, although nothing in the matrix says
    /// so.
    #[test]
    fn the_radial_wave_places_the_space_bar_at_the_middle_of_its_keycap() {
        let radial = first_frame("Radial wave");

        // Distances to the center of the drawing (11.25; 3.25): "Espace" (Space)
        // at 5.17 u, "B" at 4.83 u — a 0.34 u gap, hence neighboring hues.
        // Measuring from the left edge of the keycap (3.75 u) would widen the
        // gap to 2.7 u, and the two colors would have nothing in common.
        let space = color_at(&radial, 116);
        let key_b = color_at(&radial, 94);
        let gap = space
            .iter()
            .zip(key_b)
            .map(|(e, t)| e.abs_diff(*t) as u32)
            .max()
            .expect("three components");

        assert!(
            gap < 60,
            "Space and B are physically close, their colors should be too: {space:?} vs {key_b:?}"
        );
    }
}
