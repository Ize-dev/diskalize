//! Explorer integration: "Mit Diskalize analysieren" in the right-click menu.
//!
//! Everything is written under `HKCU\Software\Classes`, so no elevation is needed
//! and uninstalling is a single key delete.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::win::wide;

pub const VERB: &str = "Diskalize";
pub const TITLE: &str = "Mit Diskalize öffnen";

/// Where Explorer looks for folder verbs.
///
/// `Directory` rather than the more general `Folder`: entries registered under
/// `Folder` render without their icon, which is why ours stayed blank while
/// WizTree — registered under `Directory` — showed its own in the same menu.
/// `Network` still covers the namespace items under "Netzwerk", `Drive` the
/// drive letters and `Directory\Background` the empty space of an open window.
fn keys() -> [(&'static str, &'static str); 4] {
    [
        ("Directory", "%1"),
        ("Drive", "%1"),
        ("Network", "%1"),
        ("Directory\\Background", "%V"),
    ]
}

/// Keys an earlier version wrote that would now show a duplicate entry.
fn legacy_keys() -> [&'static str; 1] {
    ["Folder"]
}

fn set_string(key: HKEY, name: Option<&str>, value: &str) -> bool {
    let wname = name.map(wide);
    let wval = wide(value);
    let bytes = unsafe {
        std::slice::from_raw_parts(wval.as_ptr() as *const u8, wval.len() * 2)
    };
    let r = unsafe {
        RegSetValueExW(
            key,
            wname.as_ref().map_or(ptr::null(), |v| v.as_ptr()),
            0,
            REG_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    r == ERROR_SUCCESS
}

fn create(path: &str) -> Option<HKEY> {
    let mut h: HKEY = ptr::null_mut();
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            ptr::null(),
            &mut h,
            ptr::null_mut(),
        )
    };
    (r == ERROR_SUCCESS).then_some(h)
}

pub fn is_installed() -> bool {
    let path = format!(r"Software\Classes\Directory\shell\{VERB}\command");
    let mut h: HKEY = ptr::null_mut();
    let r = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(&path).as_ptr(),
            0,
            KEY_READ,
            &mut h,
        )
    };
    if r == ERROR_SUCCESS {
        unsafe { RegCloseKey(h) };
        true
    } else {
        false
    }
}

/// Repairs the damage an earlier build did.
///
/// That version created `HKCU\...\<class>\shell` containing only our verb, which
/// Explorer then treated as the *default* action — clicking a folder opened
/// Diskalize instead of the folder. Called on every start so an existing broken
/// registration heals itself without the user having to know about it.
pub fn repair_defaults() {
    // An earlier build registered under `Folder`. Migrate it rather than
    // leaving the user with an entry that never shows its icon.
    if !is_installed() {
        let legacy = legacy_keys().iter().any(|c| {
            let path = format!(r"Software\Classes\{c}\shell\{VERB}\command");
            let mut h: HKEY = ptr::null_mut();
            let ok = unsafe {
                RegOpenKeyExW(HKEY_CURRENT_USER, wide(&path).as_ptr(), 0, KEY_READ, &mut h)
            } == ERROR_SUCCESS;
            if ok {
                unsafe { RegCloseKey(h) };
            }
            ok
        });
        if legacy {
            let _ = install();
        }
        return;
    }
    let icon = std::env::current_exe()
        .map(|p| icon_path(&p.to_string_lossy()))
        .unwrap_or_default();
    for (class, _) in keys() {
        let path = format!(r"Software\Classes\{class}\shell");
        if let Some(k) = create(&path) {
            set_string(k, None, "open");
            unsafe { RegCloseKey(k) };
        }
        // Rewrite the icon too: older builds quoted the path, which Explorer
        // could not parse, so the menu entry came up without one.
        if !icon.is_empty() {
            let verb = format!(r"Software\Classes\{class}\shell\{VERB}");
            if let Some(k) = create(&verb) {
                set_string(k, Some("Icon"), &icon);
                unsafe { RegCloseKey(k) };
            }
        }
    }
    // The old `Directory` registration would now show up twice.
    for class in legacy_keys() {
        let path = format!(r"Software\Classes\{class}\shell\{VERB}");
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&path).as_ptr()) };
    }
}

pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .to_string_lossy()
        .into_owned();
    let icon = icon_path(&exe);

    // `Directory` inherits from `Folder`; leaving both would show the entry twice.
    for class in legacy_keys() {
        let path = format!(r"Software\Classes\{class}\shell\{VERB}");
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&path).as_ptr()) };
    }

    for (class, arg) in keys() {
        // Explorer takes the verb named by the `shell` key's default value as the
        // default action. Our HKCU `shell` key would be the only one there, which
        // silently promoted "Diskalize" to the default — a left click on a folder
        // then opened Diskalize instead of the folder. Pinning the value to
        // "open" keeps the normal action normal.
        let shell_key = format!(r"Software\Classes\{class}\shell");
        if let Some(s) = create(&shell_key) {
            set_string(s, None, "open");
            unsafe { RegCloseKey(s) };
        }

        let base = format!(r"Software\Classes\{class}\shell\{VERB}");
        let Some(k) = create(&base) else {
            return Err(format!("Registry-Schlüssel {base} nicht schreibbar"));
        };
        set_string(k, None, TITLE);
        set_string(k, Some("Icon"), &icon);
        unsafe { RegCloseKey(k) };

        let cmd_path = format!("{base}\\command");
        let Some(c) = create(&cmd_path) else {
            return Err(format!("Registry-Schlüssel {cmd_path} nicht schreibbar"));
        };
        set_string(c, None, &format!("\"{exe}\" \"{arg}\""));
        unsafe { RegCloseKey(c) };
    }
    Ok(())
}

/// Value for the verb's `Icon` entry.
///
/// Deliberately unquoted: Explorer parses this as `path,index` and a quoted path
/// makes it give up and show no icon at all — which is why the entry stayed
/// blank while other tools in the same menu had theirs.
fn icon_path(exe: &str) -> String {
    let ico = std::path::Path::new(exe).with_extension("ico");
    if ico.exists() {
        format!("{},0", ico.to_string_lossy())
    } else {
        format!("{exe},0")
    }
}

pub fn uninstall() -> Result<(), String> {
    let mut err = None;
    let classes = keys()
        .iter()
        .map(|(c, _)| *c)
        .chain(legacy_keys())
        .collect::<Vec<_>>();
    for class in classes {
        let path = format!(r"Software\Classes\{class}\shell\{VERB}");
        let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&path).as_ptr()) };
        // ERROR_FILE_NOT_FOUND just means it was never installed.
        if r != ERROR_SUCCESS && r != 2 {
            err = Some(format!("{path} konnte nicht entfernt werden (Code {r})"));
        }
        // Also drop the `shell` key we pinned "open" into. RegDeleteKeyW refuses
        // when subkeys remain, which is exactly the guard we want: if another
        // program registered a verb there, we leave everything alone.
        let shell_key = format!(r"Software\Classes\{class}\shell");
        unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, wide(&shell_key).as_ptr()) };
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Opens Explorer with the given path selected.
pub fn reveal(path: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let args = format!("/select,\"{path}\"");
    unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide("open").as_ptr(),
            wide("explorer.exe").as_ptr(),
            wide(&args).as_ptr(),
            ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL as i32,
        )
    };
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn autostart_enabled() -> bool {
    let mut h: HKEY = ptr::null_mut();
    let ok = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_READ, &mut h)
    } == ERROR_SUCCESS;
    if !ok {
        return false;
    }
    let mut kind = 0u32;
    let mut len = 0u32;
    let found = unsafe {
        windows_sys::Win32::System::Registry::RegQueryValueExW(
            h,
            wide("Diskalize").as_ptr(),
            ptr::null_mut(),
            &mut kind,
            ptr::null_mut(),
            &mut len,
        )
    } == ERROR_SUCCESS;
    unsafe { RegCloseKey(h) };
    found
}

/// Starts the window with Windows. The service has its own autostart; this is
/// only about the user interface.
pub fn set_autostart(on: bool) -> Result<(), String> {
    let Some(k) = create(RUN_KEY) else {
        return Err("Autostart-Schlüssel nicht schreibbar".into());
    };
    let res = if on {
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        // Started with Windows means started out of the way: the window comes
        // up hidden and only the notification icon appears.
        if set_string(k, Some("Diskalize"), &format!("\"{exe}\" --tray")) {
            Ok(())
        } else {
            Err("Autostart konnte nicht gesetzt werden".into())
        }
    } else {
        unsafe {
            windows_sys::Win32::System::Registry::RegDeleteValueW(k, wide("Diskalize").as_ptr())
        };
        Ok(())
    };
    unsafe { RegCloseKey(k) };
    res
}

/// Opens a folder (or a file with its default app).
pub fn open(path: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            wide("open").as_ptr(),
            wide(path).as_ptr(),
            ptr::null(),
            ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL as i32,
        )
    };
}

const _: Option<*const c_void> = None;

/// Reads the current autostart command line, if there is one.
fn autostart_command() -> Option<String> {
    use windows_sys::Win32::System::Registry::RegQueryValueExW;
    let mut h: HKEY = ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, wide(RUN_KEY).as_ptr(), 0, KEY_READ, &mut h) }
        != ERROR_SUCCESS
    {
        return None;
    }
    let name = wide("Diskalize");
    let mut kind = 0u32;
    let mut len = 0u32;
    let ok = unsafe {
        RegQueryValueExW(h, name.as_ptr(), ptr::null_mut(), &mut kind, ptr::null_mut(), &mut len)
    } == ERROR_SUCCESS;
    let mut out = None;
    if ok && len > 0 {
        let mut buf = vec![0u16; (len as usize).div_ceil(2) + 1];
        let mut size = (buf.len() * 2) as u32;
        if unsafe {
            RegQueryValueExW(
                h,
                name.as_ptr(),
                ptr::null_mut(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        } == ERROR_SUCCESS
        {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            out = Some(crate::win::from_wide(&buf[..n]));
        }
    }
    unsafe { RegCloseKey(h) };
    out
}

/// Brings an autostart entry written by an older build up to date.
///
/// Autostart used to launch a normal window, which threw it in the user's face
/// on every login. The entry now carries `--tray`; an existing one has to be
/// rewritten, otherwise the change would only take effect if the user happened
/// to toggle the setting off and on again.
pub fn repair_autostart() {
    let Some(cmd) = autostart_command() else {
        return;
    };
    if cmd.contains("--tray") {
        return;
    }
    let _ = set_autostart(true);
}
