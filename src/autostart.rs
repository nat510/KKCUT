//! "Start when I sign in": the HKCU Run value, started with --tray so
//! KKCUT starts in the notification area without its window.

use crate::wide;
use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "KKCUT";
/// Where Task Manager's Startup apps page records a disabled entry.
const STARTUP_APPROVED: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";

fn startup_command(exe: &std::path::Path) -> std::io::Result<String> {
    let command = format!("\"{}\" --tray", exe.display());
    if command.encode_utf16().count() > 260 {
        return Err(std::io::Error::other(
            "Windows 的啟動命令最多 260 個字元，請把 KKCUT 移到較短的路徑",
        ));
    }
    Ok(command)
}

/// The program of a Run command: the quoted part, or the whole command.
fn program(command: &str) -> &str {
    match command.strip_prefix('"').and_then(|r| r.split_once('"')) {
        Some((path, _)) => path,
        None => command.trim(),
    }
}

fn startup_approved(bytes: &[u8]) -> bool {
    // StartupApproved is an undocumented Explorer format; its known enabled
    // states start with 2 or 6.
    bytes.is_empty() || matches!(bytes.first(), Some(2 | 6))
}

/// The command registered under the Run value, if any.
fn registered_command() -> Option<String> {
    let mut command = [0u16; 512];
    let mut len = size_of_val(&command) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            wide(RUN_VALUE).as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            command.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result != 0 {
        return None;
    }
    let end = command
        .iter()
        .position(|&v| v == 0)
        .unwrap_or(command.len());
    Some(String::from_utf16_lossy(&command[..end]))
}

/// True only for an existing program whose startup entry Windows has not disabled.
pub fn enabled() -> bool {
    let Some(command) = registered_command() else {
        return false;
    };
    if !std::path::Path::new(program(&command)).is_file() {
        return false;
    }
    let mut approved = [0u8; 12];
    let mut size = approved.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(STARTUP_APPROVED).as_ptr(),
            wide(RUN_VALUE).as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            approved.as_mut_ptr().cast(),
            &mut size,
        )
    };
    status == ERROR_FILE_NOT_FOUND
        || (status == 0 && startup_approved(&approved[..(size as usize).min(approved.len())]))
}

/// Turning it on points the entry at this copy of KKCUT and clears a
/// "disabled" left by Task Manager.
pub fn set(on: bool) -> std::io::Result<()> {
    let check = |status: u32| {
        if status == 0 || status == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32))
        }
    };
    let remove = |key: &str| unsafe {
        RegDeleteKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), wide(RUN_VALUE).as_ptr())
    };
    if on {
        let command = wide(&startup_command(&std::env::current_exe()?)?);
        check(unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                wide(RUN_KEY).as_ptr(),
                wide(RUN_VALUE).as_ptr(),
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * 2) as u32,
            )
        })?;
    } else {
        check(remove(RUN_KEY))?;
    }
    check(remove(STARTUP_APPROVED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_quote_the_program_and_start_hidden() {
        let command = startup_command(std::path::Path::new(r"C:\工具 Folder\KKCUT_x64.exe")).unwrap();
        assert_eq!(command, r#""C:\工具 Folder\KKCUT_x64.exe" --tray"#);
        assert_eq!(program(&command), r"C:\工具 Folder\KKCUT_x64.exe");
        assert_eq!(program(r"C:\KKCUT.exe"), r"C:\KKCUT.exe");
        let long = format!("C:\\{}\\KKCUT.exe", "a".repeat(260));
        assert!(startup_command(std::path::Path::new(&long)).is_err());
    }

    #[test]
    fn windows_disabled_state_is_not_reported_as_enabled() {
        assert!(startup_approved(&[]));
        assert!(startup_approved(&[2, 0, 0, 0]));
        assert!(startup_approved(&[6, 0, 0, 0]));
        assert!(!startup_approved(&[3, 0, 0, 0]));
        assert!(!startup_approved(&[7, 0, 0, 0]));
    }
}
