//! Devices plugged in and unplugged, from the system's own notifications (#81).
//!
//! Nothing is enumerated on a timer. Windows reports HID interfaces arriving and
//! leaving (`CM_Register_Notification`); Linux sends a kernel uevent for each
//! `hidraw` node. A notification only says "look again", and
//! [`crate::reconcile_devices`] compares what is plugged in with what is open.
//!
//! # Nothing here depends on how fast a machine is
//!
//! - **No notification is lost.** Those arriving during a pass stay queued and
//!   cause another one: a late notification is handled late, never missed.
//! - **Gathering a burst is only an economy.** A keyboard exposes several HID
//!   interfaces; [`GATHER`] folds their notifications into one pass. Too short
//!   somewhere, it costs a second pass, not a missed device.
//! - **What does depend on the device is retried.** An interface can be listed
//!   before it opens: firmware still starting, or on Linux a `hidraw` node udev
//!   has not given its permissions yet. No further notification comes then, so an
//!   adopted device plugged in but not open is tried again on [`RETRIES`].

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use tauri::AppHandle;

/// Quiet time that ends a burst of notifications.
///
/// Measured on 14/09/2026, DeathStalker V2 Pro on Windows 11: 11 notifications
/// within 15 ms on unplug, within 38 ms on replug.
const GATHER: Duration = Duration::from_millis(150);

/// Delays before trying again to open an adopted device that is plugged in but
/// did not open; about 8 s in all. Past them its error stays in **Devices**, and
/// the next notification starts over.
const RETRIES: [Duration; 5] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// Starts listening. Without notifications — refused by the system — Candeo
/// works as before: a replugged device is reconnected from **Devices**.
pub(crate) fn watch(app: &AppHandle) {
    let (tx, rx) = mpsc::channel::<()>();
    if let Err(e) = imp::watch(tx) {
        tracing::warn!("device plug notifications unavailable: {e}");
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("candeo-hotplug".into())
        .spawn(move || {
            let mut pending: Option<usize> = None;
            loop {
                let notified = match pending.and_then(|i| RETRIES.get(i)) {
                    Some(&delay) => match rx.recv_timeout(delay) {
                        Ok(()) => true,
                        Err(RecvTimeoutError::Timeout) => false,
                        Err(RecvTimeoutError::Disconnected) => break,
                    },
                    None => match rx.recv() {
                        Ok(()) => true,
                        Err(_) => break,
                    },
                };
                if notified {
                    let first = Instant::now();
                    let mut count = 1;
                    while rx.recv_timeout(GATHER).is_ok() {
                        count += 1;
                    }
                    tracing::debug!(
                        count,
                        burst_ms = (first.elapsed() - GATHER).as_millis() as u64,
                        "device notifications gathered"
                    );
                }
                let retry = crate::reconcile_devices(&app);
                pending = next_retry(retry, notified, pending);
                if pending == Some(RETRIES.len()) {
                    tracing::warn!(
                        "adopted device still not opened, waiting for it to be plugged in again"
                    );
                }
            }
        });
    match spawned {
        Ok(_) => tracing::info!("listening for devices plugged in and unplugged"),
        Err(e) => tracing::warn!("device plug notifications unavailable: {e}"),
    }
}

/// The retry to wait for after a pass: none when nothing needs one, the first
/// after a notification, the next after a retry. `RETRIES.len()` means they are
/// used up, and only a notification starts them again.
fn next_retry(retry: bool, notified: bool, pending: Option<usize>) -> Option<usize> {
    match (retry, notified, pending) {
        (false, _, _) => None,
        (true, true, _) | (true, false, None) => Some(0),
        (true, false, Some(i)) => Some((i + 1).min(RETRIES.len())),
    }
}

/// True for a kernel uevent adding or removing a `hidraw` node: a header
/// `action@devpath`, then `KEY=value` fields, all NUL-separated.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_hidraw_event(message: &[u8]) -> bool {
    let mut fields = message
        .split(|&b| b == 0)
        .filter_map(|f| std::str::from_utf8(f).ok());
    let Some(header) = fields.next() else {
        return false;
    };
    (header.starts_with("add@") || header.starts_with("remove@"))
        && fields.any(|f| f == "SUBSYSTEM=hidraw")
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::mpsc::Sender;

    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Register_Notification, CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
        CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER,
        CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CR_SUCCESS, HCMNOTIFICATION,
    };
    use windows_sys::Win32::Devices::HumanInterfaceDevice::GUID_DEVINTERFACE_HID;

    /// Registers for HID interfaces. The registration and its sender live as long
    /// as the process: nothing unregisters them.
    pub fn watch(tx: Sender<()>) -> Result<(), String> {
        // SAFETY: a plain C struct, valid zeroed; the fields that matter are set below.
        let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
        filter.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
        filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
        filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_HID;

        let context = Box::into_raw(Box::new(tx));
        let mut handle: HCMNOTIFICATION = std::ptr::null_mut();
        // SAFETY: `filter` and `handle` outlive the call; `context` stays valid
        // for the process's lifetime, as the callback requires.
        let result = unsafe {
            CM_Register_Notification(&filter, context.cast(), Some(notified), &mut handle)
        };
        if result != CR_SUCCESS {
            // SAFETY: not registered, so the callback will never read it.
            drop(unsafe { Box::from_raw(context) });
            return Err(format!("CM_Register_Notification returned {result}"));
        }
        Ok(())
    }

    /// Called on a system thread; only forwards the wake-up.
    unsafe extern "system" fn notified(
        _handle: HCMNOTIFICATION,
        context: *const c_void,
        action: CM_NOTIFY_ACTION,
        _data: *const CM_NOTIFY_EVENT_DATA,
        _size: u32,
    ) -> u32 {
        if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
            || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
        {
            // SAFETY: the `Sender` given at registration, never freed.
            let tx = unsafe { &*context.cast::<Sender<()>>() };
            let _ = tx.send(());
        }
        0
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;
    use std::sync::mpsc::Sender;

    /// Kernel uevents, not udev's: they need no udev library. They can arrive
    /// before udev applies the permission rule, which the retries cover.
    const KERNEL_EVENTS: u32 = 1;

    pub fn watch(tx: Sender<()>) -> Result<(), String> {
        // SAFETY: plain socket calls; the descriptor is checked before use.
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
                libc::NETLINK_KOBJECT_UEVENT,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: `sockaddr_nl` is valid zeroed.
        let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        address.nl_groups = KERNEL_EVENTS;
        // SAFETY: `address` is a `sockaddr_nl` of the length given.
        let bound = unsafe {
            libc::bind(
                fd,
                std::ptr::addr_of!(address).cast(),
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if bound < 0 {
            let e = io::Error::last_os_error();
            // SAFETY: `fd` is ours and not used afterwards.
            unsafe { libc::close(fd) };
            return Err(e.to_string());
        }

        std::thread::Builder::new()
            .name("candeo-uevents".into())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    // SAFETY: `buffer` is writable for its whole length.
                    let n = unsafe { libc::recv(fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
                    if n < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        tracing::warn!("device plug notifications stopped: {e}");
                        break;
                    }
                    if super::is_hidraw_event(&buffer[..n as usize]) && tx.send(()).is_err() {
                        break;
                    }
                }
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn watch(_tx: std::sync::mpsc::Sender<()>) -> Result<(), String> {
        Err("not supported on this system".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hidraw_nodes_coming_and_going_count() {
        let event = |header: &str, subsystem: &str| {
            format!("{header}\0ACTION=x\0DEVPATH=/devices/x\0SUBSYSTEM={subsystem}\0SEQNUM=1\0")
                .into_bytes()
        };
        assert!(is_hidraw_event(&event(
            "add@/devices/x/hidraw/hidraw3",
            "hidraw"
        )));
        assert!(is_hidraw_event(&event(
            "remove@/devices/x/hidraw/hidraw3",
            "hidraw"
        )));
        assert!(!is_hidraw_event(&event(
            "change@/devices/x/hidraw/hidraw3",
            "hidraw"
        )));
        assert!(!is_hidraw_event(&event(
            "add@/devices/x/input/input9",
            "input"
        )));
        assert!(!is_hidraw_event(b""));
    }

    #[test]
    fn a_device_that_does_not_open_is_retried_a_bounded_number_of_times() {
        assert_eq!(next_retry(false, true, None), None);
        assert_eq!(next_retry(false, false, Some(2)), None);
        assert_eq!(next_retry(true, true, None), Some(0));
        // A notification starts the delays over, even when they were used up.
        assert_eq!(next_retry(true, true, Some(RETRIES.len())), Some(0));
        assert_eq!(next_retry(true, false, Some(0)), Some(1));
        let last = RETRIES.len() - 1;
        assert_eq!(next_retry(true, false, Some(last)), Some(RETRIES.len()));
        assert_eq!(
            next_retry(true, false, Some(RETRIES.len())),
            Some(RETRIES.len())
        );
    }
}
