//! The log: where records go, and how to reach them.
//!
//! Before this module, nothing was readable in `release`: eight `eprintln!` in the
//! Rust, three `console.` in the window, and a binary compiled with
//! `windows_subsystem = "windows"`, hence with no console to receive them. Three
//! failures already identified fell into that void: a device failing to open at
//! startup, the silent degradation of the single instance under Linux (#45), and
//! the "non-blocking" warning about an unexpected firmware version (#35).
//!
//! # Four layers, one of which is always forgotten
//!
//! 1. a **facade**: the `tracing` macros, called everywhere, which do not know
//!    where records go;
//! 2. a **subscriber** that filters by level and formats;
//! 3. **destinations**: a rolling file, and standard output in development;
//! 4. **a way to reach the file from the app**: [`open_log_dir`] and
//!    [`diagnostic`]. A log nobody knows how to find is useless.
//!
//! # `tracing` rather than `tauri-plugin-log`
//!
//! The plugin is first-party and would cover the essentials in a few lines. What
//! it does not give is context: **there is one render loop per device**, and
//! "write refused" is useless without knowing which one. A *span* opened per loop
//! (see [`crate::runtime`]) attaches the device and the effect to everything
//! logged inside it, **without carrying a device identifier through every call**.
//! That is exactly the shape of our failures, and it is what keeps a log readable
//! when two devices run together.
//!
//! The price is one more crate and a filter to configure.
//!
//! # Two non-negotiable rules
//!
//! **Log transitions, never occurrences.** At 30 frames per second, a failing
//! write would produce thirty lines per second and bury the only one that matters.
//! The engine already has the right model: the error is set, then cleared on
//! recovery, and the stop comes after a fixed number of consecutive failures;
//! [`transition`] is what carries that over to the log.
//!
//! **The serial number does not leak.** The protocol returns one and it identifies
//! a specific unit; a log pasted into a bug report must not disclose it. See
//! [`fingerprint`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_opener::OpenerExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt as fmt_layer, reload, EnvFilter, Registry};

use crate::{AppState, CmdResult, DeviceRef, Failure};

/// The environment variable that overrides everything else.
///
/// `CANDEO_LOG` and not `RUST_LOG`: the latter is shared by all Rust tooling, and
/// someone who set it to read what `cargo` is saying would change the app's log
/// along the way, without meaning to.
pub const VARIABLE: &str = "CANDEO_LOG";

/// The level when nothing says otherwise: the lifecycle, and nothing more.
const DEFAULT_LEVEL: LogLevel = LogLevel::Info;

/// `candeo.2026-09-12.log`, in the log directory.
const FILE_PREFIX: &str = "candeo";
const FILE_SUFFIX: &str = "log";

/// Files kept by default, one per day: a week, which covers "it started on
/// Monday", the useful reach of a bug report. Someone can keep more, or all of
/// them with `0`: `preferences.logFilesKept`, as OpenRGB's `file_count_limit`.
///
/// ⚠️ **The cap is on the number of files, not their size.** A high level carries
/// per-frame records: the current day can grow a lot before rotation cuts it.
/// What bounds the size is the level, hence [`JournalStatus::verbose`], and the
/// notice the interface shows about it.
pub const DEFAULT_FILES_KEPT: u32 = 7;

/// The cap in force, set from the settings. `0` until they are read, so that
/// nothing is deleted on a default someone did not choose.
static FILES_KEPT: AtomicU32 = AtomicU32::new(0);

/// Target of records coming from the window.
///
/// A fixed target, with the origin as a field: `tracing` requires a target that is
/// constant at compile time, and in any case "everything from the WebView" is what
/// we want to be able to filter with a single word.
const WEBVIEW_TARGET: &str = "candeo_webview";

/// The crates whose level follows the one that is set.
///
/// The others (Tauri, `hidapi`, WebView) stay at `warn`: raising the log to
/// `debug` to follow a render loop must not drown the file in a third-party
/// library's trace, which is precisely what we are not looking for.
const OUR_CRATES: &[&str] = &[
    "candeo_desktop_lib",
    "candeo_device",
    "candeo_protocol",
    WEBVIEW_TARGET,
];

// ---------------------------------------------------------------- levels

/// The levels, as they mean something **here**.
///
/// | Level | What it means |
/// |---|---|
/// | `error` | the user's lighting is broken |
/// | `warn` | degraded but working: unexpected firmware (#35), single-instance exclusion not working (#45) |
/// | `info` | lifecycle: device adopted, effect started, effect stopped |
/// | `debug` / `trace` | per frame, **off by default** |
///
/// The declaration order is increasing verbosity, and it is relied on:
/// [`directives`] derives from it the level granted to third-party crates.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// The name `EnvFilter` understands, and the one read in the file.
    fn name(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }

    /// The level named by this string, if there is one.
    ///
    /// Case- and whitespace-insensitive: it is typed by hand in a terminal, often
    /// in upper case out of habit from other tools.
    fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// True if this level carries per-frame records.
    fn is_verbose(self) -> bool {
        self >= Self::Debug
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The directives given to the filter for this level.
///
/// Two parts, and the second is the reason this function exists: our crates at
/// the requested level, **everything else at least as quiet**. A global `trace`
/// would make the file unreadable without teaching anything about Candeo.
///
/// The `min` covers the only case where the rule flips: asking for `error` is
/// asking for silence, and leaving third parties at `warn` would then be chattier
/// than what was asked for ourselves.
fn directives(level: LogLevel) -> String {
    let third_party = level.min(LogLevel::Warn);
    let mut out = String::from(third_party.name());
    for target in OUR_CRATES {
        out.push(',');
        out.push_str(target);
        out.push('=');
        out.push_str(level.name());
    }
    out
}

/// What the priority rule decided.
#[derive(Debug, PartialEq, Eq)]
struct Resolution {
    /// What is given to the filter.
    directives: String,
    /// The level, when a single word sums it up. `None` when the environment
    /// variable holds a finer directive that no level names.
    level: Option<LogLevel>,
    /// True if the environment variable decided.
    forced: bool,
}

/// **The priority rule: environment > saved setting > default.**
///
/// The environment always wins, and it is not a matter of taste: it is what
/// makes it possible to diagnose an app that does not get far enough to read its
/// settings, such as a configuration directory that cannot be found or an
/// unreadable `settings.json`.
///
/// `CANDEO_LOG` accepts both forms: a bare level (`debug`), which is the usual
/// gesture, or a full directive
/// (`candeo_desktop_lib::runtime=trace,warn`) to target a specific module. In the
/// second case no level sums up what is requested, and the interface says so
/// rather than inventing one.
///
/// An **empty** variable counts as an unset one: `CANDEO_LOG=` is what a shell
/// writes once it has "cleared" it, and reading it as an empty directive would
/// cut off the whole log without anyone asking for it.
///
/// A pure function, deliberately: the rule is checked without a subscriber,
/// without a disk and without an app.
fn resolve(env: Option<&str>, setting: Option<LogLevel>) -> Resolution {
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        Some(raw) => match LogLevel::from_name(raw) {
            Some(level) => Resolution {
                directives: directives(level),
                level: Some(level),
                forced: true,
            },
            None => Resolution {
                directives: raw.to_string(),
                level: None,
                forced: true,
            },
        },
        None => {
            let level = setting.unwrap_or(DEFAULT_LEVEL);
            Resolution {
                directives: directives(level),
                level: Some(level),
                forced: false,
            }
        }
    }
}

// ---------------------------------------------------------------- subscriber

/// What initialization leaves behind, and what the commands read back.
struct Collector {
    /// The filter's reload handle.
    ///
    /// **This is the requirement that shapes everything else**: the defect we are
    /// chasing may not survive a restart. A keyboard that drops out after two
    /// hours, an effect that drifts slowly: saying "restart in verbose mode"
    /// amounts to asking to reproduce what was just observed, and often that is
    /// not possible. It is set up **from initialization**: adding it later would
    /// mean reworking initialization from end to end.
    filter: reload::Handle<EnvFilter, Registry>,
    /// True if [`VARIABLE`] decided for this run.
    forced: bool,
    /// The log directory. `None` when it could not be resolved: the log then runs
    /// without a file, which beats having no log.
    dir: Option<PathBuf>,
}

/// Once per process, and read-only afterwards.
static COLLECTOR: OnceLock<Collector> = OnceLock::new();

/// Starts the log. **Cannot fail.**
///
/// Called at the top of the app's `setup`, that is, at the first moment
/// `app_log_dir()` exists, and above all **before the store is resolved**. The
/// order is not cosmetic: a failure to resolve the configuration directory is
/// exactly the kind of thing we want to see, and it would happen before there was
/// anything to write it to.
///
/// The saved setting, however, is not read yet: we start at the default, then
/// [`reload_level_setting`] adjusts. Never the other way round.
///
/// Nothing propagates, and two failures are absorbed here: a log directory that
/// cannot be found (the log then runs without a file) and a subscriber already
/// installed, which only happens on a second call. Neither must prevent the
/// window from opening: it is what would allow fixing the situation.
pub fn init(app: &AppHandle) {
    let resolution = resolve(std::env::var(VARIABLE).ok().as_deref(), None);

    // An unreadable directive must not deprive the person who will fix it of a
    // log: we fall back to the default, and say so, once the subscriber is in
    // place, since before that there is nobody to hear it.
    let (filter, rejected_directive) = match EnvFilter::try_new(&resolution.directives) {
        Ok(f) => (f, None),
        Err(e) => (
            EnvFilter::new(directives(DEFAULT_LEVEL)),
            Some(format!("{} : {e}", resolution.directives)),
        ),
    };
    let (filter_layer, handle) = reload::Layer::new(filter);

    let (dir, file, file_error) = match open_log_file(app) {
        Ok((dir, appender)) => (Some(dir), Some(appender), None),
        Err(e) => (None, None, Some(e)),
    };

    // `with_ansi(false)` on the file: color sequences mean nothing in a file, and
    // they make what gets pasted into a bug report unreadable.
    //
    // Direct writes, with no writer thread in between: we only log transitions,
    // so the volume is negligible, and a log whose last lines are lost in the very
    // failure being chased is worthless.
    let file_layer = file.map(|appender| {
        fmt_layer::layer()
            .with_ansi(false)
            .with_target(true)
            .with_writer(appender)
    });

    // In development only: in `release` the binary is compiled
    // `windows_subsystem = "windows"`, and there is no console to receive anything.
    let console_layer = cfg!(debug_assertions).then(fmt_layer::layer);

    // `try_init` rather than `init`: installing a subscriber when there already is
    // one is a programming error, not a reason to refuse to start the app.
    let registered = tracing_subscriber::registry()
        .with(filter_layer)
        .with(file_layer)
        .with(console_layer)
        .try_init()
        .is_ok();
    if !registered {
        return;
    }

    let _ = COLLECTOR.set(Collector {
        filter: handle,
        forced: resolution.forced,
        dir: dir.clone(),
    });

    tracing::info!(
        version = app.package_info().version.to_string(),
        os = std::env::consts::OS,
        architecture = std::env::consts::ARCH,
        log = dir.as_deref().map(crate::paths::shown),
        "Candeo starting"
    );
    if let Some(e) = file_error {
        tracing::error!("no log on disk, only standard output remains: {e}");
    }
    if let Some(e) = rejected_directive {
        tracing::warn!("{VARIABLE} unreadable, default level applied: {e}");
    }
    if let Some(level) = resolution.level {
        warn_if_verbose(level);
    }
}

/// The rolling file, and the directory that holds it.
///
/// **Through `app_log_dir()`, never a hard-coded path**: on Windows data and
/// configuration share a location, on Linux they do not, and logs are neither
/// one nor the other.
fn open_log_file(app: &AppHandle) -> Result<(PathBuf, Pruning), String> {
    let dir = app
        .path()
        .app_log_dir()
        .map_err(|e| format!("no log folder: {e}"))?;
    let appender = Pruning {
        appender: rolling_appender_in(&dir)?,
        dir: dir.clone(),
        day: AtomicU64::new(today()),
    };
    Ok((dir, appender))
}

/// The rolling file, cut down to [`FILES_KEPT`] when a new day's file starts.
///
/// `tracing-appender` takes its cap when the file is opened, and the file is
/// opened before the settings are read: a cap someone sets has to be applied here.
struct Pruning {
    appender: tracing_appender::rolling::RollingFileAppender,
    dir: PathBuf,
    /// Day of the last write, counted as the daily rotation counts it (UTC).
    day: AtomicU64,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Pruning {
    type Writer =
        <tracing_appender::rolling::RollingFileAppender as tracing_subscriber::fmt::MakeWriter<
            'a,
        >>::Writer;

    fn make_writer(&'a self) -> Self::Writer {
        // The writer first: a new day's file is created there, and counts among
        // those kept.
        let writer = self.appender.make_writer();
        let now = today();
        if self.day.swap(now, Ordering::Relaxed) != now {
            // Nothing is logged from here: this runs inside a write.
            let _ = prune(&self.dir, FILES_KEPT.load(Ordering::Relaxed));
        }
        writer
    }
}

/// Days since the epoch, in UTC like the daily rotation.
fn today() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400)
}

/// Deletes the log files beyond the `keep` most recent; `0` keeps them all.
fn prune(dir: &Path, keep: u32) -> std::io::Result<()> {
    let names = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned());
    for name in files_beyond(names, keep) {
        std::fs::remove_file(dir.join(name))?;
    }
    Ok(())
}

/// The daily log files beyond the `keep` most recent, `candeo.YYYY-MM-DD.log`
/// sorting by date. Anything else in the folder is not ours to delete.
fn files_beyond(names: impl IntoIterator<Item = String>, keep: u32) -> Vec<String> {
    if keep == 0 {
        return Vec::new();
    }
    let prefix = format!("{FILE_PREFIX}.");
    let suffix = format!(".{FILE_SUFFIX}");
    let mut logs: Vec<String> = names
        .into_iter()
        .filter(|n| {
            n.strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(&suffix))
                .is_some_and(|date| {
                    date.len() == 10
                        && date.chars().enumerate().all(|(i, c)| {
                            if i == 4 || i == 7 {
                                c == '-'
                            } else {
                                c.is_ascii_digit()
                            }
                        })
                })
        })
        .collect();
    logs.sort_unstable_by(|a, b| b.cmp(a));
    logs.split_off((keep as usize).min(logs.len()))
}

/// Sets the number of files kept, and deletes those beyond it now.
fn apply_files_kept(keep: u32) {
    FILES_KEPT.store(keep, Ordering::Relaxed);
    let Some(dir) = COLLECTOR.get().and_then(|c| c.dir.as_ref()) else {
        return;
    };
    if let Err(e) = prune(dir, keep) {
        tracing::warn!("old log files not deleted: {e}");
    }
}

/// The rolling file of a given directory.
///
/// Split from [`open_log_file`] for the only reason that counts: it is the part a
/// test can exercise. A rejected rotation configuration would otherwise only show
/// when the app launches, as a log that writes nowhere.
fn rolling_appender_in(
    dir: &std::path::Path,
) -> Result<tracing_appender::rolling::RollingFileAppender, String> {
    // Created here rather than on the first write: a directory that cannot be
    // created is reported now, not at the first failure we wanted to record.
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create {}: {e}", crate::paths::shown(dir)))?;

    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX)
        .build(dir)
        .map_err(|e| format!("log not opened in {}: {e}", crate::paths::shown(dir)))
}

/// Reads the level saved in `settings.json` back and applies it.
///
/// Called right after [`init`], and that is the whole startup order: start at
/// the default, **then** adjust. The store is therefore resolved here for the
/// second time during startup (adoption resolves it too), and that is the
/// accepted price of not making the log depend on what uses it.
///
/// Does nothing when [`VARIABLE`] decided: the priority rule also holds at
/// startup, otherwise the saved setting would override what was just requested on
/// the command line.
pub fn reload_level_setting(app: &AppHandle) {
    match crate::storage::store(app).and_then(|s| s.read_settings()) {
        // The log is already in place: it is precisely for this message that it
        // had to start before the store. Old files are left alone: nobody chose
        // how many to keep.
        Err(e) => tracing::error!("log settings not read back, the defaults apply: {e}"),
        Ok(settings) => {
            apply_files_kept(settings.preferences.log_files_kept);
            if COLLECTOR.get().is_some_and(|c| c.forced) {
                return;
            }
            if let Some(level) = settings.preferences.log_level {
                apply_level(level);
            }
        }
    }
}

/// Brings the level back to the default, without restarting.
///
/// Called by the configuration reset: the setting has just been erased from the
/// file, and a screen still showing "détaillé" (verbose) would be lying. No effect
/// when [`VARIABLE`] decided: priority is not suspended for a reset.
pub(crate) fn reset_level_to_default() {
    apply_files_kept(DEFAULT_FILES_KEPT);
    if COLLECTOR.get().is_some_and(|c| c.forced) {
        return;
    }
    apply_level(DEFAULT_LEVEL);
}

/// Replaces the filter of the subscriber **already in place**.
///
/// This is the live change: no restart, no file reopened, and open spans, hence
/// the running render loops, keep their context.
fn apply_level(level: LogLevel) {
    let Some(collector) = COLLECTOR.get() else {
        return;
    };
    match EnvFilter::try_new(directives(level)) {
        Ok(filter) => match collector.filter.reload(filter) {
            Ok(()) => {
                tracing::info!(level = %level, "log level changed");
                warn_if_verbose(level);
            }
            Err(e) => tracing::error!("log level unchanged: {e}"),
        },
        // The directives are built from a known level: this path is only
        // reachable through a mistake in [`directives`].
        Err(e) => tracing::error!("log directives rejected: {e}"),
    }
}

/// Writes into the file itself that a high level is active.
///
/// The interface already says so ([`JournalStatus::verbose`]), but the log is
/// read elsewhere and later, often by someone else: a file of several gigabytes
/// must carry its own explanation, rather than leaving the reader to find out
/// what ran away.
fn warn_if_verbose(level: LogLevel) {
    if level.is_verbose() {
        tracing::warn!(
            "level \"{level}\": the log carries per-frame records and grows fast. \
             Rotation caps the number of files, not the size of today's file; \
             go back to \"info\" once the capture is done."
        );
    }
}

// ---------------------------------------------------------------- transitions

/// What a change of error state gives to log.
///
/// **The case that matters is [`Unchanged`](Transition::Unchanged)**, and it
/// covers two situations that have nothing in common but their conclusion:
///
/// - everything is fine and already was: the vast majority of frames;
/// - **the failure persists, and the reason changed.** This is not a new
///   incident: a message carrying a counter, a position or a timestamp would vary
///   on every frame, and logging that change would reopen exactly the flood we are
///   trying to avoid: thirty lines per second, none of which teaches anything more
///   than the first.
///
/// The first reason, however, is recorded: it is the one that names the failure.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Transition {
    /// The failure begins. One line, with its reason.
    Started,
    /// It is over. One line, otherwise the interface and the log would show a
    /// stale error indefinitely.
    Recovered,
    /// Nothing to say.
    Unchanged,
}

/// The transition between two successive error states.
pub(crate) fn transition<T: ?Sized>(before: Option<&T>, after: Option<&T>) -> Transition {
    match (before, after) {
        (None, Some(_)) => Transition::Started,
        (Some(_), None) => Transition::Recovered,
        _ => Transition::Unchanged,
    }
}

// ---------------------------------------------------------------- serial

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A stable fingerprint of the serial number, **that does not disclose it**.
///
/// The protocol returns a serial number (`0x00`/`0x82`) and it identifies a
/// specific unit; the USB descriptor carries none. A log pasted into a bug report
/// must not reveal it; but erasing it outright would make two keyboards of the
/// same model indistinguishable, which is precisely when the log helps most.
///
/// FNV-1a written here rather than a standard library hasher: `DefaultHasher`
/// does not promise to return the same value from one Rust version to the next,
/// and a fingerprint that changes on recompilation would no longer allow matching
/// two logs of the same device.
///
/// This is not cryptographic protection and it does not need to be: it prevents
/// an accidental disclosure, not an attack.
pub(crate) fn fingerprint(serial: &str) -> String {
    let mut h = FNV_OFFSET_BASIS;
    for b in serial.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{h:016x}")
}

/// The fingerprint of a serial that may not exist.
///
/// "none" is not a degenerate case: it is what hidraw returns under
/// Linux when the udev rule does not grant read access to attributes, and telling
/// it apart from a present serial is what avoids hunting for a device failure
/// where there is only a missing permission.
pub(crate) fn fingerprint_of(serial: Option<&str>) -> String {
    serial.map_or_else(|| "none".to_string(), fingerprint)
}

// ---------------------------------------------------------------- commands

/// The log status, as the interface shows it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalStatus {
    /// The applied level. `None` when [`VARIABLE`] holds a directive that no level
    /// sums up: we do not pretend to name it.
    pub level: Option<LogLevel>,
    /// The level saved in `settings.json`.
    ///
    /// Distinct from `level`: it is the one the interface offers to change, and it
    /// stays editable even when the environment variable wins for this run.
    pub setting: Option<LogLevel>,
    /// True if [`VARIABLE`] forces the level. The interface says so rather than
    /// letting the user believe that a setting with no effect was taken into account.
    pub forced_by_env: bool,
    /// The log directory, `None` if it could not be resolved.
    pub dir: Option<String>,
    /// True if the active level carries **per-frame** records.
    ///
    /// ⚠️ This is what makes it visible that a high level is active. Left on and
    /// forgotten, `trace` fills the disk silently, and rotation caps the number of
    /// files, not the size of today's file.
    ///
    /// Read from the filter actually installed, not derived from `level`: it is
    /// the only way to answer correctly when the directive comes from the
    /// environment and sums up to no level.
    pub verbose: bool,
    /// Log files kept, one per day; `0` keeps them all.
    pub files_kept: u32,
}

fn journal_status(setting: Option<LogLevel>, files_kept: u32) -> JournalStatus {
    let collector = COLLECTOR.get();
    JournalStatus {
        level: active_level(),
        setting,
        files_kept,
        forced_by_env: collector.is_some_and(|c| c.forced),
        dir: collector
            .and_then(|c| c.dir.as_ref())
            .map(|d| d.display().to_string()),
        verbose: tracing::level_filters::LevelFilter::current() >= tracing::Level::DEBUG,
    }
}

/// The most verbose level the filter lets through, if it names one.
fn active_level() -> Option<LogLevel> {
    tracing::level_filters::LevelFilter::current()
        .into_level()
        .and_then(|l| LogLevel::from_name(l.as_str()))
}

/// The log status: applied level, saved level, directory.
#[tauri::command]
pub fn get_journal(app: AppHandle) -> CmdResult<JournalStatus> {
    let preferences = crate::storage::store(&app)?.read_settings()?.preferences;
    Ok(journal_status(
        preferences.log_level,
        preferences.log_files_kept,
    ))
}

/// Changes how many log files are kept, saves it, and deletes those beyond it
/// now rather than at the next day's file.
#[tauri::command]
pub fn set_log_files_kept(app: AppHandle, keep: u32) -> CmdResult<JournalStatus> {
    let store = crate::storage::store(&app)?;
    let mut settings = store.read_settings()?;
    if settings.preferences.log_files_kept != keep {
        settings.preferences.log_files_kept = keep;
        store.write_settings(&settings)?;
    }
    apply_files_kept(keep);
    Ok(journal_status(settings.preferences.log_level, keep))
}

/// Changes the level **without restarting**, and saves it.
///
/// # It survives a restart, and that is a choice
///
/// Automatically going back to the default would protect against a full disk;
/// persistence serves whoever is chasing a defect **at startup**. A defect that
/// only happens at launch exists (device adoption is one), and asking that person
/// to raise the level after the fact is asking the impossible. So we persist, and
/// we say so: [`JournalStatus::verbose`] is there for that.
///
/// # When the environment variable wins
///
/// The setting is written, but **the filter is not touched**: priority holds for
/// the whole run, not only at startup. The setting will apply on the next launch
/// without the variable, and `forcedByEnv` tells the interface to announce it.
#[tauri::command]
pub fn set_log_level(app: AppHandle, level: LogLevel) -> CmdResult<JournalStatus> {
    let store = crate::storage::store(&app)?;
    let mut settings = store.read_settings()?;
    if settings.preferences.log_level != Some(level) {
        settings.preferences.log_level = Some(level);
        store.write_settings(&settings)?;
    }

    if !COLLECTOR.get().is_some_and(|c| c.forced) {
        apply_level(level);
    }
    Ok(journal_status(
        settings.preferences.log_level,
        settings.preferences.log_files_kept,
    ))
}

/// Opens the log directory in the system file manager.
///
/// **A log nobody knows how to find is useless**, and the path depends on the
/// system: giving it to read is not enough, the user has to be taken there.
#[tauri::command]
pub fn open_log_dir(app: AppHandle) -> CmdResult<()> {
    let dir = COLLECTOR
        .get()
        .and_then(|c| c.dir.clone())
        .ok_or_else(|| Failure::unexpected("no log folder: the log does not write to disk"))?;

    app.opener()
        .open_path(dir.display().to_string(), None::<&str>)
        .map_err(|e| Failure::unexpected(format!("cannot open {}: {e}", crate::paths::shown(&dir))))
}

/// The system's version as a bug report needs it: the release and build on
/// Windows, read from the registry because `ProductName` still says "Windows 10"
/// on Windows 11.
#[cfg(windows)]
fn os_version() -> String {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };

    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let key = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
    let text = |name: &str| -> Option<String> {
        let name = wide(name);
        let mut buffer = [0u16; 64];
        let mut size = std::mem::size_of_val(&buffer) as u32;
        // SAFETY: `size` is the buffer's size in bytes; both strings end in NUL.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        let chars = (size as usize / 2).saturating_sub(1);
        (status == 0).then(|| String::from_utf16_lossy(&buffer[..chars]))
    };
    let number = |name: &str| -> Option<u32> {
        let name = wide(name);
        let mut value = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: `value` is a DWORD, `size` its size; both strings end in NUL.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(value).cast(),
                &mut size,
            )
        };
        (status == 0).then_some(value)
    };

    let Some(build) = text("CurrentBuild") else {
        return "Windows, version not read".into();
    };
    let revision = number("UBR").map(|r| format!(".{r}")).unwrap_or_default();
    match text("DisplayVersion") {
        Some(release) => format!("Windows {release}, build {build}{revision}"),
        None => format!("Windows, build {build}{revision}"),
    }
}

/// The distribution and the kernel.
#[cfg(not(windows))]
fn os_version() -> String {
    let distribution = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| pretty_name(&content))
        .unwrap_or_else(|| std::env::consts::OS.to_owned());
    match std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(kernel) => format!("{distribution}, kernel {}", kernel.trim()),
        Err(_) => distribution,
    }
}

/// `PRETTY_NAME` from an `os-release` file.
#[cfg_attr(windows, allow(dead_code))]
fn pretty_name(os_release: &str) -> Option<String> {
    os_release
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim().trim_matches('"').to_owned())
}

/// The diagnostic, ready to be pasted into a bug report.
///
/// **It beats any digging through logs**: everything we ask for every time
/// (version, system, devices, engine state) fits in twenty lines, with no need
/// to explain where to look.
///
/// The serial number is not included: its [`fingerprint`] is enough to tell two
/// units apart, and that is all a bug report needs.
#[tauri::command]
pub fn diagnostic(app: AppHandle, state: State<'_, AppState>) -> CmdResult<String> {
    let mut out = String::new();
    let line = |out: &mut String, key: &str, value: &str| {
        out.push_str(key);
        out.push_str(": ");
        out.push_str(value);
        out.push('\n');
    };

    line(&mut out, "Candeo", &app.package_info().version.to_string());
    // The first two things asked about odd rendering or a crash at startup (#48).
    line(
        &mut out,
        "system",
        &format!("{} · {}", os_version(), std::env::consts::ARCH),
    );
    line(
        &mut out,
        "webview",
        &tauri::webview_version().unwrap_or_else(|e| format!("not read ({e})")),
    );
    // Settings and HID are gathered separately, and neither is unwrapped: being
    // unable to enumerate USB or read `settings.json` back is exactly what a
    // diagnostic must **say**, not what must interrupt it.
    let settings = crate::storage::store(&app).and_then(|s| s.read_settings());
    let api = crate::hid();

    let preferences = settings.as_ref().ok().map(|s| &s.preferences);
    let log = journal_status(
        preferences.and_then(|p| p.log_level),
        preferences.map_or(DEFAULT_FILES_KEPT, |p| p.log_files_kept),
    );
    let log_dir = COLLECTOR
        .get()
        .and_then(|c| c.dir.as_deref())
        .map(crate::paths::shown);
    line(
        &mut out,
        "log",
        &format!(
            "level {} ({}), {} kept{}",
            // The applied level, not the saved one: it is what explains what the
            // file contains, or does not.
            log.level
                .map_or_else(|| "directive".to_string(), |l| l.to_string()),
            log_dir.as_deref().unwrap_or("no file"),
            match log.files_kept {
                0 => "all files".to_string(),
                n => format!("{n} files"),
            },
            if log.forced_by_env {
                format!(", forced by {VARIABLE}")
            } else {
                String::new()
            }
        ),
    );

    // What decides whether closing the window stops effects. Without this line, a
    // report saying "my effect stops when I close" and another saying the
    // opposite would be indistinguishable: placing the icon can fail, and the close
    // button then becomes an exit again. See [`crate::tray`].
    line(
        &mut out,
        "tray",
        if crate::tray::installed() {
            "installed: closing the window hides it, the tray menu quits"
        } else {
            "missing: closing the window stops effects"
        },
    );

    out.push_str("\nDevices\n");
    for layout in crate::LAYOUTS {
        let device = DeviceRef::of(layout);
        let plugged_in = api
            .as_ref()
            .ok()
            .and_then(|api| crate::plugged(api, layout));
        // Read from the handle: the diagnostic makes no new exchange with the
        // device, it reports what opening obtained.
        let inspection = state.inspection(device);
        let serial = crate::known_serial(inspection.as_ref(), plugged_in.clone().flatten());
        let saved_state = settings
            .as_ref()
            .ok()
            .map(|s| decision(s.device_state(layout.vid, layout.pid, serial.as_deref())));

        line(
            &mut out,
            &format!("  {} {}", layout.name, device),
            &format!(
                "{} · {} · serial {} · layout {}×{} ({} cells, {} keys)",
                match plugged_in {
                    Some(_) => "plugged in",
                    None => "unplugged",
                },
                saved_state.unwrap_or("unknown state"),
                fingerprint_of(serial.as_deref()),
                layout.rows,
                layout.cols,
                layout.led_count(),
                layout.lit_count(),
            ),
        );

        // What the **file** saves for this device, next to what the engine does
        // with it below. The two must agree; when they diverge (an applied effect
        // that is not running, a saved brightness that no adoption reapplied),
        // this is precisely the line that shows it, and it costs the reader
        // nothing.
        if let Ok(s) = &settings {
            line(
                &mut out,
                "    saved",
                &format!(
                    "effect {} · brightness {}",
                    s.active_effect(layout.vid, layout.pid).unwrap_or("none"),
                    match s.brightness(layout.vid, layout.pid, serial.as_deref()) {
                        crate::storage::DEFAULT_BRIGHTNESS => "full (default)".to_string(),
                        n => n.to_string(),
                    }
                ),
            );
        }

        // **The first field anyone will ask for** in front of unexplained
        // behavior: the version read, next to the one of the survey. When closed,
        // we say it was not read rather than repeat the one from a past opening:
        // the unit plugged in since may no longer be the same.
        line(
            &mut out,
            "    firmware",
            &format!(
                "{} · layout surveyed on {}",
                match &inspection {
                    None => "not read, device closed".to_string(),
                    Some(i) => i
                        .firmware
                        .as_ref()
                        .map_or_else(|e| format!("not read ({e})"), ToString::to_string),
                },
                layout.surveyed_firmware
            ),
        );
        if let Some(i) = &inspection {
            line(
                &mut out,
                "    commands",
                &i.checks
                    .iter()
                    .map(|c| format!("{} {}", c.name, verdict(&c.verdict)))
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
            if let Err(e) = &i.serial {
                line(&mut out, "    protocol serial", &format!("not read ({e})"));
            }
            for warning in i.warnings(layout) {
                line(&mut out, "    warning", &warning.to_string());
            }
        }
    }
    if let Err(e) = &api {
        line(&mut out, "  USB enumeration", &e.to_string());
    }
    if let Err(e) = &settings {
        line(&mut out, "  settings", &e.to_string());
    }

    out.push_str("\nEngine\n");
    let report = state.engine.report();
    let engine = report.devices;
    if engine.is_empty() {
        out.push_str("  no device targeted since startup\n");
    }
    for s in engine {
        line(
            &mut out,
            &format!("  {}", s.device),
            &format!(
                "{} · effect {} · output {} · reaching {}{}{}",
                if s.status.running {
                    "running"
                } else {
                    "stopped"
                },
                s.status.effect_id.as_deref().unwrap_or("none"),
                if s.status.to_keyboard { "on" } else { "off" },
                s.status.reaching_keyboard,
                s.status.error.map_or(String::new(), |e| format!(
                    " · effect error: {}",
                    crate::runtime::loggable(&e, s.status.reads_keys)
                )),
                s.status
                    .device_error
                    .map_or(String::new(), |e| format!(" · write error: {e}")),
            ),
        );
    }

    // **On its own line, and named for what it is.** A preview writes to no
    // keyboard: confusing it with what precedes would send someone hunting on the
    // hardware side for a failure that is not there, and its absence from the list
    // above would read as an omission if nothing named it here.
    line(
        &mut out,
        "  preview",
        &match report.preview {
            None => "none".to_string(),
            Some(p) => format!(
                "{} · effect {} · borrowed layout {} · no keyboard output{}",
                if p.running { "running" } else { "stopped" },
                p.effect_id.as_deref().unwrap_or("none"),
                p.layout_of,
                p.error.map_or(String::new(), |e| format!(
                    " · effect error: {}",
                    crate::runtime::loggable(&e, p.reads_keys)
                )),
            ),
        },
    );

    Ok(out)
}

/// The adoption decision, in the language of the bug report.
///
/// A table rather than a lowercased `{:?}`: the diagnostic is read by a human, and
/// Rust variant names have no reason to appear in it. The compiler will demand
/// this line the day a fourth state exists.
fn decision(state: crate::storage::DeviceState) -> &'static str {
    match state {
        crate::storage::DeviceState::Detected => "detected",
        crate::storage::DeviceState::Adopted => "controlled",
        crate::storage::DeviceState::Ignored => "ignored",
    }
}

/// The verdict of a command, in the language of the bug report.
///
/// "known" and not "understood": the status byte confirms
/// that the class / command pair exists, never that its arguments are right. A
/// diagnostic saying "compatible" would send people looking elsewhere for an
/// argument failure.
fn verdict(v: &candeo_device::Verdict) -> String {
    use candeo_device::Verdict;
    match v {
        Verdict::Understood => "known (0x02), read back identical".to_string(),
        Verdict::Unsupported => "unknown (0x05), no longer sent".to_string(),
        Verdict::ReadBackDiffers { wrote, read } => {
            format!("accepted, but read back {read} after rewriting {wrote}")
        }
        Verdict::Unverified(reason) => format!("unverified ({reason})"),
    }
}

/// The level of a record coming from the window.
///
/// A dedicated type rather than [`LogLevel`]: the window has nothing to say beyond
/// `debug` (the engine's per-frame records do not go through it), and a level it
/// cannot produce has no reason to be accepted as an argument.
#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "camelCase")]
pub enum WebviewLevel {
    Error,
    Warn,
    Info,
    Debug,
}

/// Records an entry coming from the window.
///
/// Without it, `app.config.errorHandler` and effect compilation errors went to a
/// console nobody opens in `release`. They now arrive in the **same file** as
/// everything else: a failure reads from end to end, and the order between what
/// the window saw and what the engine saw is the order in the file.
///
/// `source` names where it comes from (a component, a module) as a field rather
/// than as a target: `tracing` requires a target constant at compile time, and in
/// any case "what comes from the WebView" is what we want to filter with one word.
///
/// Returns nothing and cannot fail: logging must never become a second failure to
/// handle in the error handler.
#[tauri::command]
pub fn log_from_webview(level: WebviewLevel, source: String, message: String) {
    match level {
        WebviewLevel::Error => tracing::error!(target: WEBVIEW_TARGET, source, "{message}"),
        WebviewLevel::Warn => tracing::warn!(target: WEBVIEW_TARGET, source, "{message}"),
        WebviewLevel::Info => tracing::info!(target: WEBVIEW_TARGET, source, "{message}"),
        WebviewLevel::Debug => tracing::debug!(target: WEBVIEW_TARGET, source, "{message}"),
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------- priority

    /// **The rule, in order.** Nothing: the default. A setting: the setting. The
    /// environment: the environment, whatever else there is.
    #[test]
    fn env_wins_then_setting_then_default() {
        assert_eq!(resolve(None, None).level, Some(DEFAULT_LEVEL));
        assert!(!resolve(None, None).forced);

        assert_eq!(
            resolve(None, Some(LogLevel::Debug)).level,
            Some(LogLevel::Debug)
        );
        assert!(!resolve(None, Some(LogLevel::Debug)).forced);

        let forced = resolve(Some("trace"), Some(LogLevel::Debug));
        assert_eq!(forced.level, Some(LogLevel::Trace));
        assert!(forced.forced, "the setting had the last word");
    }

    /// Typed by hand in a terminal: upper case and spaces are forgiven.
    #[test]
    fn env_level_is_read_whatever_its_case() {
        assert_eq!(resolve(Some("  WARN "), None).level, Some(LogLevel::Warn));
    }

    /// `CANDEO_LOG=` is what a shell writes once it has "cleared" it. Reading it
    /// as an empty directive would cut off the whole log without anyone asking
    /// for it.
    #[test]
    fn empty_variable_counts_as_unset() {
        let r = resolve(Some("   "), Some(LogLevel::Warn));
        assert_eq!(r.level, Some(LogLevel::Warn));
        assert!(!r.forced);
    }

    /// A fine-grained directive passes as is, and **no level sums it up**: the
    /// interface must be able to say so rather than invent one.
    #[test]
    fn full_directive_passes_without_being_named() {
        let r = resolve(Some("candeo_desktop_lib::runtime=trace,warn"), None);
        assert_eq!(r.directives, "candeo_desktop_lib::runtime=trace,warn");
        assert_eq!(r.level, None);
        assert!(r.forced);
    }

    // -------------------------------------------------------- directives

    /// Raising our level must not raise that of third-party libraries: it is what
    /// keeps the file readable when chasing a render loop.
    #[test]
    fn only_our_crates_follow_requested_level() {
        let d = directives(LogLevel::Trace);
        assert!(d.starts_with("warn,"), "third parties are not capped: {d}");
        for target in OUR_CRATES {
            assert!(
                d.contains(&format!("{target}=trace")),
                "{target} missing from {d}"
            );
        }
        // And what we write must be accepted by the filter, otherwise the
        // failure would only show when the app launches.
        EnvFilter::try_new(&d).expect("directives rejected by the filter");
    }

    /// Asking for `error` is asking for silence: leaving third parties at `warn`
    /// would make the log chattier than what we asked for ourselves.
    #[test]
    fn asking_for_silence_quiets_third_parties_too() {
        let d = directives(LogLevel::Error);
        assert!(d.starts_with("error,"), "{d}");
    }

    #[test]
    fn every_level_produces_valid_directives() {
        for level in [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            EnvFilter::try_new(directives(level)).unwrap_or_else(|e| panic!("\"{level}\": {e}"));
            assert_eq!(LogLevel::from_name(level.name()), Some(level));
        }
    }

    /// The default is not verbose, and `debug` is: that is the boundary the
    /// interface announces.
    #[test]
    fn per_frame_starts_at_debug() {
        assert!(!DEFAULT_LEVEL.is_verbose());
        assert!(!LogLevel::Warn.is_verbose());
        assert!(LogLevel::Debug.is_verbose());
        assert!(LogLevel::Trace.is_verbose());
    }

    // -------------------------------------------------------- file

    /// Only the daily files beyond the most recent ones go, and `0` keeps them all;
    /// anything else in the folder is left alone.
    #[test]
    fn only_the_oldest_daily_files_beyond_the_limit_are_deleted() {
        let names = || {
            [
                "candeo.2026-09-12.log",
                "candeo.2026-09-14.log",
                "candeo.2026-09-13.log",
                "candeo.2026-09-11.log",
                "notes.txt",
                "candeo.log",
                "candeo.2026-9-1.log",
            ]
            .map(String::from)
        };
        assert_eq!(
            files_beyond(names(), 2),
            ["candeo.2026-09-12.log", "candeo.2026-09-11.log"]
        );
        assert!(files_beyond(names(), 4).is_empty());
        assert!(files_beyond(names(), 10).is_empty());
        assert!(files_beyond(names(), 0).is_empty());
    }

    /// The rolling file opens, gets written, and has the expected name.
    ///
    /// The rotation configuration is rejected at build time when it is
    /// inconsistent (an empty prefix, a zero cap), and that rejection would
    /// otherwise only show when the app launches, as a silent log. The directory
    /// does not exist yet at the start: that is the first-launch case, and it
    /// must not be a failure.
    #[test]
    fn rolling_file_opens_with_expected_name() {
        use std::io::Write;

        let tmp = tempfile::tempdir().expect("temporary directory");
        let dir = tmp.path().join("logs");
        assert!(!dir.exists(), "the test would no longer check creation");

        let mut appender = rolling_appender_in(&dir).expect("log not opened");
        appender.write_all(b"une ligne\n").expect("write");
        appender.flush().expect("flush");

        let files: Vec<String> = std::fs::read_dir(&dir)
            .expect("log directory")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();

        assert_eq!(files.len(), 1, "files: {files:?}");
        // `candeo.YYYY-MM-DD.log`: the prefix makes the file recognizable in a
        // directory opened from the app, the date makes it sortable, and the
        // suffix makes it open on a double-click.
        let name = &files[0];
        assert!(name.starts_with(&format!("{FILE_PREFIX}.")), "name: {name}");
        assert!(name.ends_with(&format!(".{FILE_SUFFIX}")), "name: {name}");
        assert!(std::fs::read_to_string(dir.join(name))
            .expect("read")
            .contains("une ligne"));
    }

    // -------------------------------------------------------- reload

    /// A buffer that keeps what the subscriber writes, so it can be read back.
    #[derive(Clone, Default)]
    struct CaptureBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CaptureBuffer {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for CaptureBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl fmt_layer::MakeWriter<'_> for CaptureBuffer {
        type Writer = Self;
        fn make_writer(&self) -> Self {
            self.clone()
        }
    }

    /// **The level changes live, without restarting.**
    ///
    /// This is the requirement that shapes the whole module, and it cannot be
    /// checked by reading the code: reloading depends on where the filter sits in
    /// the layer stack and on `tracing` rebuilding its interest cache. A badly
    /// assembled stack would compile, and the setting would simply have no
    /// effect: the hardest failure to notice, since it only shows the day it is
    /// needed.
    ///
    /// The subscriber is assembled here as [`init`] assembles it, but set on the
    /// test thread (`set_default` rather than `try_init`): a global subscriber can
    /// only be installed once per process, and the other tests do not have to
    /// depend on this one.
    #[test]
    fn level_changes_live_without_restart() {
        let buffer = CaptureBuffer::default();
        let (reload_layer, handle) = reload::Layer::new(EnvFilter::new(directives(LogLevel::Info)));
        let subscriber = tracing_subscriber::registry().with(reload_layer).with(
            fmt_layer::layer()
                .with_ansi(false)
                .with_writer(buffer.clone()),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::debug!("marker-before");
        tracing::info!("marker-during");
        assert!(
            !buffer.text().contains("marker-before"),
            "per-frame records pass while the level is \"info\": {}",
            buffer.text()
        );
        assert!(buffer.text().contains("marker-during"));

        handle
            .reload(EnvFilter::new(directives(LogLevel::Debug)))
            .expect("reload rejected");

        tracing::debug!("marker-after");
        assert!(
            buffer.text().contains("marker-after"),
            "the level did not change without a restart: {}",
            buffer.text()
        );

        // And the other way round: we must be able to turn the tap off without
        // relaunching either, otherwise a forgotten capture would fill the disk
        // until the app is next closed.
        handle
            .reload(EnvFilter::new(directives(LogLevel::Info)))
            .expect("reload rejected");
        tracing::debug!("marker-closed");
        assert!(
            !buffer.text().contains("marker-closed"),
            "the level does not go back down: {}",
            buffer.text()
        );
    }

    // -------------------------------------------------------- transitions

    #[test]
    fn the_distribution_is_read_from_os_release() {
        let file = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(pretty_name(file).as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(
            pretty_name("PRETTY_NAME=Arch Linux"),
            Some("Arch Linux".into())
        );
        assert_eq!(pretty_name("ID=debian\n"), None);
    }

    /// **Transitions, never occurrences.** At 30 frames per second, a failing
    /// write would produce thirty lines per second.
    #[test]
    fn only_a_state_change_is_logged() {
        assert_eq!(transition(None, Some("refused")), Transition::Started);
        assert_eq!(transition(Some("refused"), None), Transition::Recovered);
        assert_eq!(transition::<str>(None, None), Transition::Unchanged);
        assert_eq!(
            transition(Some("refused"), Some("refused")),
            Transition::Unchanged
        );
    }

    /// The failure persists and the reason changes: this is not a new incident. A
    /// message carrying a counter would vary on every frame, and logging it would
    /// reopen the flood we just closed.
    #[test]
    fn reason_changing_during_failure_says_nothing_new() {
        assert_eq!(
            transition(Some("frame 1 refused"), Some("frame 2 refused")),
            Transition::Unchanged
        );
    }

    // -------------------------------------------------------- serial

    /// What a log pasted into a bug report must **never** contain. The chosen
    /// serial cannot be written in hexadecimal, otherwise the test could pass by
    /// accident.
    #[test]
    fn fingerprint_does_not_disclose_serial_number() {
        let serial = "XYZW-KLM-9921";
        let e = fingerprint(serial);
        assert!(!e.contains(serial), "the serial is in the fingerprint: {e}");
        for piece in ["XYZW", "KLM", "9921"] {
            assert!(!e.contains(piece), "\"{piece}\" leaked into {e}");
        }
    }

    /// Stable from one call to the next (otherwise two logs of the same device
    /// would not match) and distinct from one unit to another, otherwise it would
    /// be useless.
    #[test]
    fn fingerprint_is_stable_and_tells_two_units_apart() {
        assert_eq!(fingerprint("XY01"), fingerprint("XY01"));
        assert_ne!(fingerprint("XY01"), fingerprint("XY02"));
        assert_eq!(fingerprint("XY01").len(), 16);
    }

    /// "Plugged in without a declared serial" is not a degenerate case: it is
    /// hidraw without a udev rule, and confusing it with a serial would send
    /// someone hunting for a device failure where there is only a missing
    /// permission.
    #[test]
    fn silent_enumeration_reads_differently_from_a_fingerprint() {
        assert_eq!(fingerprint_of(None), "none");
        assert_eq!(fingerprint_of(Some("XY01")), fingerprint("XY01"));
    }

    // -------------------------------------------------------- firmware

    /// The status byte validates no argument: the diagnostic must never let
    /// "compatible" or "understood" be read where the device only said
    /// that it knew the command.
    #[test]
    fn diagnostic_does_not_oversell_a_known_command() {
        use candeo_device::Verdict;
        let text = verdict(&Verdict::Understood);
        assert!(text.contains("known"), "{text}");
        for word in ["compatible", "understood"] {
            assert!(!text.contains(word), "\"{word}\" in \"{text}\"");
        }
        assert!(verdict(&Verdict::Unsupported).contains("no longer sent"));
    }

    // -------------------------------------------------------- serialization

    /// Levels cross the IPC: their spelling is the interface's and the one in
    /// `settings.json`, and it must not change.
    #[test]
    fn levels_serialize_as_they_are_written() {
        assert_eq!(serde_json::to_string(&LogLevel::Warn).unwrap(), r#""warn""#);
        assert_eq!(
            serde_json::from_str::<LogLevel>(r#""trace""#).unwrap(),
            LogLevel::Trace
        );
    }
}
