//! # COM Port Watcher
//!
//! Watches for COM port attach/detach events on Windows using a hidden
//! message-only window with [`RegisterDeviceNotificationW`], then diffs
//! a registry snapshot to produce friendly port names like `"COM3"`.
//!

#![cfg(target_os = "windows")]
#![windows_subsystem = "windows"]

use std::collections::HashSet;
use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::ptr;
use tokio::sync::mpsc;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE,
    DEV_BROADCAST_DEVICEINTERFACE_W,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{RegisterClassExW, WNDCLASSEXW};
use windows_sys::{
    Win32::{
        Foundation::{GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            Registry::{
                HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey, RegEnumValueW, RegOpenKeyExW,
            },
        },
        UI::WindowsAndMessaging::{
            CW_USEDEFAULT, CreateWindowExW, DEVICE_NOTIFY_WINDOW_HANDLE, DefWindowProcW,
            DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, MSG,
            RegisterDeviceNotificationW, SetWindowLongPtrW, UnregisterDeviceNotification,
            WM_DEVICECHANGE, WS_OVERLAPPED,
        },
    },
    core::GUID,
};

/// `GUID_DEVINTERFACE_COMPORT` — `{86E0D1E0-8089-11D0-9CE4-08003E301F73}`
const GUID_DEVINTERFACE_COMPORT: GUID = GUID {
    data1: 0x86E0D1E0,
    data2: 0x8089,
    data3: 0x11D0,
    data4: [0x9C, 0xE4, 0x08, 0x00, 0x3E, 0x30, 0x1F, 0x73],
};

/// Null-terminated UTF-16 class name for our hidden watcher window.
const WATCHER_CLASS_NAME: &[u16] = &[
    b'C' as u16,
    b'o' as u16,
    b'm' as u16,
    b'P' as u16,
    b'o' as u16,
    b'r' as u16,
    b't' as u16,
    b'W' as u16,
    b'a' as u16,
    b't' as u16,
    b'c' as u16,
    b'h' as u16,
    b'e' as u16,
    b'r' as u16,
    0u16,
];

/// An event reported by [`watch_com_ports`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComPortEvent {
    /// A COM port was plugged in (e.g. `"COM3"`).
    Attached(String),
    /// A COM port was removed (e.g. `"COM3"`).
    Detached(String),
}

/// Spawn a background OS thread that watches for COM port plug/unplug events
/// and forwards them through the returned async [`mpsc::Receiver`].
///
/// The watcher runs for the lifetime of the process. Dropping the receiver
/// does not stop the thread, but subsequent events are silently discarded.
///
/// # Arguments
/// * `channel_capacity` – Backlog depth of the tokio channel (e.g. `64`).
///
/// # Panics
/// Panics on the watcher thread if `CreateWindowExW` or
/// `RegisterDeviceNotificationW` fails (logged before the panic).
pub fn watch_com_ports(channel_capacity: usize) -> mpsc::Receiver<ComPortEvent> {
    let (tx, rx) = mpsc::channel(channel_capacity);

    std::thread::Builder::new()
        .name("com-port-watcher".into())
        .spawn(move || {
            // SAFETY: all Win32 calls follow their documented contracts.
            unsafe { run_watcher(tx) };
        })
        .expect("failed to spawn com-port-watcher thread");

    rx
}

/// State owned by the watcher thread; a raw pointer is stored in the hidden
/// window's `GWLP_USERDATA` so the `WndProc` callback can reach it.
struct WatcherState {
    tx: mpsc::Sender<ComPortEvent>,
    /// Snapshot of port names visible at the last event.
    known_ports: HashSet<String>,
}

/// Snapshot every COM port currently present in the Windows registry.
///
/// `HKLM\HARDWARE\DEVICEMAP\SERIALCOMM` contains one `REG_SZ` value per port;
/// the *data* (not the name) is the friendly label, e.g. `"COM3"`.
///
/// Returns an empty set when no ports are present or the key does not yet
/// exist (normal on systems with no serial hardware).
fn snapshot_com_ports() -> HashSet<String> {
    let mut ports = HashSet::new();
    let subkey = wide_nul("HARDWARE\\DEVICEMAP\\SERIALCOMM");

    unsafe {
        let mut hkey = 0isize;
        let rc = RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut hkey);
        if rc != 0 {
            return ports; // key absent → no COM ports
        }

        let mut idx = 0u32;
        loop {
            // Buffers for value name (unused) and value data (port name).
            let mut name_buf = [0u16; 256];
            let mut name_len = name_buf.len() as u32;
            let mut data_buf = [0u8; 128];
            let mut data_len = data_buf.len() as u32;
            let mut value_type = 0u32;

            let rc = RegEnumValueW(
                hkey,
                idx,
                name_buf.as_mut_ptr(),
                &mut name_len,
                ptr::null(), // reserved
                &mut value_type,
                data_buf.as_mut_ptr(),
                &mut data_len,
            );

            match rc {
                259 => break, // ERROR_NO_MORE_ITEMS — done
                0 if value_type == REG_SZ && data_len >= 2 => {
                    // REG_SZ data is null-terminated UTF-16; data_len is in bytes.
                    let char_count = data_len as usize / size_of::<u16>();
                    let wide =
                        std::slice::from_raw_parts(data_buf.as_ptr() as *const u16, char_count);
                    // Strip the trailing NUL, if present.
                    let wide = match wide.iter().position(|&c| c == 0) {
                        Some(pos) => &wide[..pos],
                        None => wide,
                    };
                    let port = OsString::from_wide(wide).to_string_lossy().into_owned();
                    if !port.is_empty() {
                        ports.insert(port);
                    }
                }
                _ => {}
            }

            idx += 1;
        }

        RegCloseKey(hkey);
    }

    ports
}

/// Win32 window procedure for the hidden message-only watcher window.
///
/// Handles `WM_DEVICECHANGE`:
/// - `DBT_DEVICEARRIVAL`       → diffs registry snapshot, sends `Attached` events
/// - `DBT_DEVICEREMOVECOMPLETE`→ diffs registry snapshot, sends `Detached` events
///
/// All other messages are forwarded to `DefWindowProcW`.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        if msg == WM_DEVICECHANGE {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WatcherState;

            // Guard: user-data is set to 0 until we call SetWindowLongPtrW after
            // CreateWindowExW returns, and lparam may be 0 for some WM_DEVICECHANGE
            // sub-events (e.g. DBT_DEVNODES_CHANGED).
            if !state_ptr.is_null() && lparam != 0 {
                match wparam as u32 {
                    DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE => {
                        // Read the broadcast header to confirm device type.
                        // DEV_BROADCAST_DEVICEINTERFACE_W starts with the same
                        // three DWORD fields as DEV_BROADCAST_HDR, so this cast is safe.
                        let hdr = &*(lparam as *const DEV_BROADCAST_DEVICEINTERFACE_W);

                        if hdr.dbcc_devicetype == DBT_DEVTYP_DEVICEINTERFACE {
                            let state = &mut *state_ptr;
                            let current = snapshot_com_ports();

                            if wparam as u32 == DBT_DEVICEARRIVAL {
                                // Newly appeared ports
                                for port in current.difference(&state.known_ports) {
                                    let _ = state.tx.try_send(ComPortEvent::Attached(port.clone()));
                                }
                            } else {
                                // Ports that have vanished
                                for port in state.known_ports.difference(&current) {
                                    let _ = state.tx.try_send(ComPortEvent::Detached(port.clone()));
                                }
                            }

                            state.known_ports = current;
                        }
                    }
                    _ => {} // DBT_DEVNODES_CHANGED etc. — not interesting here
                }
            }
        }

        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// Inner loop executed on the dedicated OS watcher thread.
///
/// 1. Registers a Win32 window class with [`wnd_proc`] as the callback.
/// 2. Creates a **message-only** window (`HWND_MESSAGE` parent) — it is
///    never shown, has no Z-order position, and uses zero system resources.
/// 3. Calls [`RegisterDeviceNotificationW`] for `GUID_DEVINTERFACE_COMPORT`
///    so only COM-port events wake the message loop.
/// 4. Pumps messages until the sender end is closed (receiver dropped) or
///    `WM_QUIT` is posted.
///
/// # Safety
/// All raw Win32 calls follow their documented contracts.
unsafe fn run_watcher(tx: mpsc::Sender<ComPortEvent>) {
    unsafe {
        let hinstance = GetModuleHandleW(ptr::null());

        // ── 1. Register window class ─────────────────────────────────────────
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinstance;
        wc.lpszClassName = WATCHER_CLASS_NAME.as_ptr();

        // ERROR_CLASS_ALREADY_EXISTS (1410) is harmless — the class is reused.
        RegisterClassExW(&wc);

        // ── 2. Create a message-only window ──────────────────────────────────
        let hwnd = CreateWindowExW(
            0, // dwExStyle
            WATCHER_CLASS_NAME.as_ptr(),
            ptr::null(),   // window title (none)
            WS_OVERLAPPED, // dwStyle (0 — invisible, no chrome)
            CW_USEDEFAULT, // x
            CW_USEDEFAULT, // y
            CW_USEDEFAULT, // width
            CW_USEDEFAULT, // height
            HWND_MESSAGE,  // parent = message-only sink
            0,             // hMenu
            hinstance,
            ptr::null(), // lpParam
        );

        assert!(
            hwnd != 0,
            "CreateWindowExW failed (error {})",
            GetLastError()
        );

        // ── 3. Attach state to the window's user-data slot ───────────────────
        //
        // We Box the state, leak it into a raw pointer, and store that pointer
        // in GWLP_USERDATA so `wnd_proc` can retrieve it on every callback.
        // The Box is reconstructed and dropped after the message loop ends.
        let state = Box::new(WatcherState {
            tx,
            known_ports: snapshot_com_ports(),
        });
        let state_ptr = Box::into_raw(state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        // ── 4. Register for COM-port device-interface notifications ──────────
        let mut filter: DEV_BROADCAST_DEVICEINTERFACE_W = zeroed();
        filter.dbcc_size = size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32;
        filter.dbcc_devicetype = DBT_DEVTYP_DEVICEINTERFACE;
        filter.dbcc_classguid = GUID_DEVINTERFACE_COMPORT;

        let hnotify = RegisterDeviceNotificationW(
            hwnd as HANDLE,
            // The filter struct must be passed as *const c_void.
            &filter as *const DEV_BROADCAST_DEVICEINTERFACE_W as *const _,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );

        assert!(
            !hnotify.is_null(),
            "RegisterDeviceNotificationW failed (error {})",
            GetLastError()
        );

        // ── 5. Message loop ───────────────────────────────────────────────────
        //
        // GetMessageW blocks until a message arrives:
        //   0  → WM_QUIT  (clean exit)
        //  -1  → error
        //   n  → normal message, dispatch and continue
        let mut msg: MSG = zeroed();
        loop {
            match GetMessageW(&mut msg, 0, 0, 0) {
                0 | -1 => break,
                _ => {
                    DispatchMessageW(&msg);
                }
            }

            // Exit cleanly once the consumer has dropped the receiver.
            if (*state_ptr).tx.is_closed() {
                break;
            }
        }

        // ── 6. Cleanup ────────────────────────────────────────────────────────
        UnregisterDeviceNotification(hnotify);

        // Zero out GWLP_USERDATA before reclaiming the Box so that any stray
        // WM_DEVICECHANGE fired during teardown sees a null pointer and bails.
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(state_ptr));
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Encode a UTF-8 `&str` as a null-terminated UTF-16 `Vec<u16>`.
fn wide_nul(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ── Example main ──────────────────────────────────────────────────────────────

use std::process::Command;
#[derive(Debug)]
enum Action {
    OpenPutty { port: String, baud: u32 },
}

fn parse_activation_args(arg: &str) -> Option<Action> {
    let parts: Vec<&str> = arg.split('/').collect();

    match parts.as_slice() {
        ["comport:", _, port, _] => {
            // let baud = baud.parse().ok()?;
            Some(Action::OpenPutty {
                port: port.to_string(),
                baud: 9600,
            })
        }
        _ => None,
    }
}

fn launch_putty(port: &str) {
    Command::new("C:\\Program Files\\PuTTY\\putty.exe")
        .args(["-serial", port])
        .spawn()
        .expect("failed to launch PuTTY");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 {
        if let Some(action) = parse_activation_args(&args[1]) {
            match action {
                Action::OpenPutty { port, baud: _ } => {
                    launch_putty(&port);
                }
            }
        }

        std::process::exit(0);
    }

    let mut rx = watch_com_ports(64);

    // show_toast("Running!", "COM Port Watcher is running!").expect("No toast for you");
    println!("Watching for COM port changes… (plug/unplug a USB serial adapter)");
    let _ = show_toast("COM Notifier is Running!", None);

    while let Some(event) = rx.recv().await {
        match event {
            ComPortEvent::Attached(port) => {
                println!("[+] {port} attached");
                let _ = show_toast(format!("{port} Attached").as_str(), Some(port.as_str()));
            }
            ComPortEvent::Detached(port) => println!("[-] {port} detached"),
        }
    }
}

use windows::{
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    core::HSTRING,
};

/// Shows a simple Windows toast notification.
pub fn show_toast(message: &str, port: Option<&str>) -> windows::core::Result<()> {
    let action = if let Some(port) = port {
        format!(
            r#"
            <actions>
                <action
                content="Open PuTTY"
                activationType="protocol"
                arguments="comport://{port}" />
              </actions>
            "#
        )
    } else {
        String::new()
    };

    let toast_xml = format!(
        r#"
<toast duration="short">
<audio silent="true"/>
    <visual>
        <binding template="ToastGeneric">
            <text>{}</text>
        </binding>
    </visual>
{action}
</toast>
"#,
        message
    );

    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(toast_xml))?;

    let toast = ToastNotification::CreateToastNotification(&xml)?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
        "dev.0xa54.com-notifier",
    ))?;
    notifier.Show(&toast)?;

    Ok(())
}
