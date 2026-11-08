//! Thin, safe wrappers over the Win32 calls GameTime needs.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS,
    ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HANDLE,
    SYSTEMTIME, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_READ,
    KEY_SET_VALUE, KEY_WOW64_64KEY, REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, OpenEventW, OpenMutexW, OpenProcess,
    QueryFullProcessImageNameW, ResetEvent, SetEvent, SetProcessWorkingSetSize,
    WaitForSingleObject, EVENT_MODIFY_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
use windows_sys::Win32::UI::Shell::{FOLDERID_RoamingAppData, SHGetKnownFolderPath};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// RAII wrapper closing a kernel handle.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is valid and owned by this wrapper.
        unsafe { CloseHandle(self.0) };
    }
}

// ---------------------------------------------------------------- foreground

/// What the foreground window's process looks like to us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Foreground {
    /// No foreground window (lock screen, desktop switch, ...).
    None,
    /// The process could not be opened: access denied (protected process).
    Denied(u32),
    /// Opening or querying failed for another reason (e.g. the process exited).
    Unknown(u32),
    Exe {
        pid: u32,
        path: PathBuf,
    },
}

/// Foreground window's process: pid and full image path, if obtainable.
pub fn foreground() -> Foreground {
    // SAFETY: plain Win32 calls with valid out-pointers; the process handle is
    // owned by `OwnedHandle` and closed on drop.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return Foreground::None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return Foreground::None;
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return if GetLastError() == ERROR_ACCESS_DENIED {
                Foreground::Denied(pid)
            } else {
                Foreground::Unknown(pid)
            };
        }
        let h = OwnedHandle(h);
        // Common case: the path fits a stack buffer, no allocation.
        let mut stack = [0u16; 1024];
        let mut size = stack.len() as u32;
        if QueryFullProcessImageNameW(h.0, 0, stack.as_mut_ptr(), &mut size) != 0 {
            let path = PathBuf::from(OsString::from_wide(&stack[..size as usize]));
            return Foreground::Exe { pid, path };
        }
        let mut cap = 4096usize;
        while GetLastError() == ERROR_INSUFFICIENT_BUFFER && cap <= 32768 {
            let mut buf = vec![0u16; cap];
            let mut size = cap as u32;
            if QueryFullProcessImageNameW(h.0, 0, buf.as_mut_ptr(), &mut size) != 0 {
                buf.truncate(size as usize);
                let path = PathBuf::from(OsString::from_wide(&buf));
                return Foreground::Exe { pid, path };
            }
            cap *= 2;
        }
        Foreground::Unknown(pid)
    }
}

/// Trim the process working set (drops pages that can be re-faulted on demand).
pub fn trim_working_set() {
    // SAFETY: -1/-1 is the documented way to empty the current process's set.
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

/// Pid and full image path of the foreground window's process.
pub fn foreground_exe() -> Option<(u32, PathBuf)> {
    match foreground() {
        Foreground::Exe { pid, path } => Some((pid, path)),
        _ => None,
    }
}

// ------------------------------------------------------------ single instance

/// Named mutex guard; the handle is closed on drop.
pub struct SingleInstance(#[allow(dead_code)] OwnedHandle);

impl SingleInstance {
    /// `None` if another holder already owns the mutex (or creation failed).
    pub fn acquire(name: &str) -> Option<SingleInstance> {
        let n = wide(name);
        // SAFETY: `n` is a NUL-terminated wide string that outlives the call.
        unsafe {
            let h = CreateMutexW(null(), 0, n.as_ptr());
            if h.is_null() {
                return None;
            }
            let h = OwnedHandle(h);
            if GetLastError() == ERROR_ALREADY_EXISTS {
                return None;
            }
            Some(SingleInstance(h))
        }
    }
}

/// True if a named mutex exists (someone holds or has created it).
pub fn is_running(name: &str) -> bool {
    let n = wide(name);
    // SAFETY: `n` is a NUL-terminated wide string that outlives the call.
    unsafe {
        let h = OpenMutexW(SYNCHRONIZE, 0, n.as_ptr());
        if h.is_null() {
            // Access denied means the mutex exists (e.g. elevated tracker).
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        drop(OwnedHandle(h));
        true
    }
}

// ----------------------------------------------------------------- stop event

/// Manual-reset named event used to ask the tracker to stop.
pub struct StopEvent(OwnedHandle);

impl StopEvent {
    pub fn create(name: &str) -> Option<StopEvent> {
        let n = wide(name);
        // SAFETY: `n` is a NUL-terminated wide string that outlives the call.
        unsafe {
            let h = CreateEventW(null(), 1, 0, n.as_ptr());
            if h.is_null() {
                None
            } else {
                let ev = StopEvent(OwnedHandle(h));
                // A stale signal from a previous owner must not stop us.
                ResetEvent(ev.0 .0);
                Some(ev)
            }
        }
    }

    /// Wait up to `timeout_ms`; true if the event was signaled. If the wait
    /// itself fails, sleeps for the timeout so callers never spin.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        // SAFETY: the handle is valid for the lifetime of `self`.
        let rc = unsafe { WaitForSingleObject(self.0 .0, timeout_ms) };
        if rc == WAIT_FAILED {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
        }
        rc == WAIT_OBJECT_0
    }
}

/// Signal an existing stop event; false if none exists (tracker not running).
pub fn signal_stop(name: &str) -> bool {
    let n = wide(name);
    // SAFETY: `n` is a NUL-terminated wide string that outlives the call.
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, n.as_ptr());
        if h.is_null() {
            return false;
        }
        let h = OwnedHandle(h);
        SetEvent(h.0) != 0
    }
}

// ------------------------------------------------------------------- registry

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hive {
    CurrentUser,
    LocalMachine,
}

impl Hive {
    fn root(self) -> HKEY {
        match self {
            Hive::CurrentUser => HKEY_CURRENT_USER,
            Hive::LocalMachine => HKEY_LOCAL_MACHINE,
        }
    }
}

struct RegKey(HKEY);

impl Drop for RegKey {
    fn drop(&mut self) {
        // SAFETY: the key handle is valid and owned by this wrapper.
        unsafe { RegCloseKey(self.0) };
    }
}

fn reg_err(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

fn open_key(hive: Hive, subkey: &str, access: u32) -> Result<RegKey, u32> {
    let s = wide(subkey);
    let mut h: HKEY = null_mut();
    // SAFETY: `s` is NUL-terminated and `h` a valid out-pointer.
    let rc = unsafe { RegOpenKeyExW(hive.root(), s.as_ptr(), 0, access | KEY_WOW64_64KEY, &mut h) };
    if rc == ERROR_SUCCESS {
        Ok(RegKey(h))
    } else {
        Err(rc)
    }
}

fn reg_get_raw(hive: Hive, subkey: &str, value: &str, flags: u32) -> Option<Vec<u8>> {
    let key = open_key(hive, subkey, KEY_READ).ok()?;
    let v = wide(value);
    let mut size = 0u32;
    // SAFETY: size query only; null data pointer with a valid size out-pointer.
    let mut rc = unsafe {
        RegGetValueW(
            key.0,
            null(),
            v.as_ptr(),
            flags,
            null_mut(),
            null_mut(),
            &mut size,
        )
    };
    // The value can grow between calls; retry a few times.
    for _ in 0..4 {
        if rc != ERROR_SUCCESS && rc != ERROR_MORE_DATA {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        // SAFETY: `buf` is `size` bytes long and outlives the call.
        rc = unsafe {
            RegGetValueW(
                key.0,
                null(),
                v.as_ptr(),
                flags,
                null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if rc == ERROR_SUCCESS {
            buf.truncate(size as usize);
            return Some(buf);
        }
    }
    None
}

/// REG_SZ / REG_EXPAND_SZ value, with environment variables expanded for
/// REG_EXPAND_SZ.
pub fn reg_get_string(hive: Hive, subkey: &str, value: &str) -> Option<String> {
    let bytes = reg_get_raw(hive, subkey, value, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ)?;
    let mut w: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    while w.last() == Some(&0) {
        w.pop();
    }
    Some(String::from_utf16_lossy(&w))
}

pub fn reg_get_binary(hive: Hive, subkey: &str, value: &str) -> Option<Vec<u8>> {
    reg_get_raw(hive, subkey, value, RRF_RT_REG_BINARY)
}

/// Names of the direct subkeys of `subkey` (empty if it doesn't exist).
pub fn reg_subkeys(hive: Hive, subkey: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(key) = open_key(hive, subkey, KEY_ENUMERATE_SUB_KEYS) else {
        return out;
    };
    let mut index = 0u32;
    loop {
        let mut buf = [0u16; 256]; // key names are at most 255 chars
        let mut len = buf.len() as u32;
        // SAFETY: `buf` holds `len` u16s; other out-pointers are null (allowed).
        let rc = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                buf.as_mut_ptr(),
                &mut len,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            if rc != ERROR_NO_MORE_ITEMS {
                crate::log::log_line(&format!("reg_subkeys({subkey}): error {rc} at {index}"));
            }
            break;
        }
        out.push(String::from_utf16_lossy(&buf[..len as usize]));
        index += 1;
    }
    out
}

/// Set a REG_SZ value, creating the key if missing.
pub fn reg_set_string(hive: Hive, subkey: &str, value: &str, data: &str) -> io::Result<()> {
    let s = wide(subkey);
    let mut h: HKEY = null_mut();
    // SAFETY: all string pointers are NUL-terminated; `h` is a valid out-pointer.
    let rc = unsafe {
        RegCreateKeyExW(
            hive.root(),
            s.as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE | KEY_WOW64_64KEY,
            null(),
            &mut h,
            null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(reg_err(rc));
    }
    let key = RegKey(h);
    let v = wide(value);
    let d = wide(data);
    // SAFETY: `d` is a NUL-terminated wide string; the byte length covers it.
    let rc = unsafe {
        RegSetValueExW(
            key.0,
            v.as_ptr(),
            0,
            REG_SZ,
            d.as_ptr().cast(),
            (d.len() * 2) as u32,
        )
    };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(reg_err(rc))
    }
}

/// Delete a value; a missing key or value yields `ErrorKind::NotFound`.
pub fn reg_delete_value(hive: Hive, subkey: &str, value: &str) -> io::Result<()> {
    let key = open_key(hive, subkey, KEY_SET_VALUE).map_err(reg_err)?;
    let v = wide(value);
    // SAFETY: the key is open and `v` is NUL-terminated.
    let rc = unsafe { RegDeleteValueW(key.0, v.as_ptr()) };
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(reg_err(rc))
    }
}

// ----------------------------------------------------------------- known dirs

/// `FOLDERID_RoamingAppData` via the shell, independent of `%APPDATA%`.
pub fn roaming_appdata() -> Option<PathBuf> {
    // SAFETY: on success the shell returns a NUL-terminated string that we copy
    // and then free with CoTaskMemFree.
    unsafe {
        let mut p: *mut u16 = null_mut();
        let hr = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, 0, null_mut(), &mut p);
        if hr < 0 || p.is_null() {
            if !p.is_null() {
                CoTaskMemFree(p.cast());
            }
            return None;
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        let path = PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(p, len)));
        CoTaskMemFree(p.cast());
        Some(path)
    }
}

// ----------------------------------------------------------------------- time

pub fn now_utc_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Split epoch seconds into (y, m, d, hh, mm, ss).
fn split_secs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400) as u32;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Local UTC offset in seconds in effect at the UTC instant `utc_secs`.
pub fn local_offset_secs(utc_secs: u64) -> i32 {
    let secs = utc_secs as i64;
    let (y, m, d, hh, mi, ss) = split_secs(secs);
    if !(1601..=30827).contains(&y) {
        return 0; // out of SYSTEMTIME range: no meaningful offset
    }
    let utc = SYSTEMTIME {
        wYear: y as u16,
        wMonth: m as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: hh as u16,
        wMinute: mi as u16,
        wSecond: ss as u16,
        wMilliseconds: 0,
    };
    let mut local = utc;
    // SAFETY: both SYSTEMTIME pointers are valid; null zone means the current one.
    if unsafe { SystemTimeToTzSpecificLocalTime(null(), &utc, &mut local) } == 0 {
        return 0;
    }
    let local_secs = days_from_civil(local.wYear as i64, local.wMonth as u32, local.wDay as u32)
        * 86400
        + local.wHour as i64 * 3600
        + local.wMinute as i64 * 60
        + local.wSecond as i64;
    (local_secs - secs) as i32
}

fn format_offset(off: i32) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.unsigned_abs();
    format!("{sign}{:02}:{:02}", a / 3600, a % 3600 / 60)
}

fn format_iso(secs: i64, off: i32) -> String {
    let (y, m, d, hh, mi, ss) = split_secs(secs + off as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{hh:02}:{mi:02}:{ss:02}{}",
        format_offset(off)
    )
}

/// e.g. `2026-11-07T22:31:05+01:00`
pub fn format_iso_local(utc_secs: u64) -> String {
    format_iso(utc_secs as i64, local_offset_secs(utc_secs))
}

/// e.g. `2026-11-07 21:31:05`
pub fn format_utc(utc_secs: u64) -> String {
    let (y, m, d, hh, mi, ss) = split_secs(utc_secs as i64);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mi:02}:{ss:02}")
}

/// Local calendar date (year, month, day) of a UTC instant.
pub fn local_date(utc_secs: u64) -> (i32, u32, u32) {
    let off = local_offset_secs(utc_secs) as i64;
    let (y, m, d) = civil_from_days((utc_secs as i64 + off).div_euclid(86400));
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::RegDeleteTreeW;

    #[test]
    fn civil_epoch_and_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2026, 11, 7), 20764);
        assert_eq!(civil_from_days(20764), (2026, 11, 7));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn leap_years() {
        // 2000 and 2024 are leap, 2100 is not.
        for (y, leap) in [(2000, true), (2024, true), (2100, false), (2023, false)] {
            let feb28 = days_from_civil(y, 2, 28);
            let next = if leap { (y, 2, 29) } else { (y, 3, 1) };
            assert_eq!(civil_from_days(feb28 + 1), next);
        }
        for z in (-800_000..800_000).step_by(97) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00");
        assert_eq!(
            format_utc(20764 * 86400 + 21 * 3600 + 31 * 60 + 5),
            "2026-11-07 21:31:05"
        );
    }

    #[test]
    fn offset_formatting() {
        assert_eq!(format_offset(0), "+00:00");
        assert_eq!(format_offset(3600), "+01:00");
        assert_eq!(format_offset(-5 * 3600), "-05:00");
        assert_eq!(format_offset(5 * 3600 + 1800), "+05:30");
        assert_eq!(format_offset(5 * 3600 + 2700), "+05:45");
        assert_eq!(format_offset(-(9 * 3600 + 1800)), "-09:30");
    }

    #[test]
    fn iso_formatting() {
        let t = 20764 * 86400 + 21 * 3600 + 31 * 60 + 5;
        assert_eq!(format_iso(t, 3600), "2026-11-07T22:31:05+01:00");
        assert_eq!(format_iso(0, -5 * 3600), "1969-12-31T19:00:00-05:00");
        assert_eq!(format_iso(0, 5 * 3600 + 1800), "1970-01-01T05:30:00+05:30");
        assert_eq!(format_iso(0, 5 * 3600 + 2700), "1970-01-01T05:45:00+05:45");
    }

    #[test]
    fn local_offset_sane() {
        let now = now_utc_secs();
        for t in [now, 1_784_000_000u64, 1_768_000_000] {
            let o = local_offset_secs(t);
            assert!(o.abs() <= 14 * 3600 && o % 900 == 0);
        }
        let (y, _, _) = local_date(now);
        assert!(y >= 2025);
        assert_eq!(format_iso_local(now).len(), 25);
    }

    #[test]
    fn mutex_second_acquire_fails() {
        let name = format!("Local\\GameTimeTest-{}", std::process::id());
        let first = SingleInstance::acquire(&name);
        assert!(first.is_some());
        assert!(SingleInstance::acquire(&name).is_none());
        drop(first);
        assert!(SingleInstance::acquire(&name).is_some());
    }

    #[test]
    fn is_running_sees_mutex() {
        let name = format!("Local\\GameTimeIsRunningTest-{}", std::process::id());
        assert!(!is_running(&name));
        let held = SingleInstance::acquire(&name);
        assert!(held.is_some());
        assert!(is_running(&name));
        drop(held);
        assert!(!is_running(&name));
    }

    #[test]
    fn stop_event_signal() {
        let name = format!("Local\\GameTimeStopTest-{}", std::process::id());
        assert!(!signal_stop(&name));
        let ev = StopEvent::create(&name).unwrap();
        assert!(!ev.wait(10));
        assert!(signal_stop(&name));
        assert!(ev.wait(1000));
        assert!(ev.wait(0)); // manual reset stays signaled
    }

    #[test]
    fn registry_read_hklm() {
        let v = reg_get_string(
            Hive::LocalMachine,
            "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion",
            "ProductName",
        );
        assert!(v.unwrap().contains("Windows"));
        assert!(!reg_subkeys(Hive::LocalMachine, "SOFTWARE\\Microsoft").is_empty());
        assert!(reg_get_string(Hive::LocalMachine, "SOFTWARE\\Nope-xyz", "x").is_none());
    }

    #[test]
    fn registry_round_trip() {
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let s = wide(&self.0);
                // SAFETY: `s` is NUL-terminated.
                unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, s.as_ptr()) };
            }
        }
        let sub = format!("Software\\GameTimeTest-{}", std::process::id());
        let _cleanup = Cleanup(sub.clone());
        assert!(reg_get_string(Hive::CurrentUser, &sub, "v").is_none());
        reg_set_string(Hive::CurrentUser, &sub, "v", "hello").unwrap();
        assert_eq!(
            reg_get_string(Hive::CurrentUser, &sub, "v").as_deref(),
            Some("hello")
        );
        assert!(reg_get_binary(Hive::CurrentUser, &sub, "v").is_none());
        reg_set_string(Hive::CurrentUser, &format!("{sub}\\child"), "c", "1").unwrap();
        assert_eq!(
            reg_subkeys(Hive::CurrentUser, &sub),
            vec!["child".to_string()]
        );
        reg_delete_value(Hive::CurrentUser, &sub, "v").unwrap();
        let err = reg_delete_value(Hive::CurrentUser, &sub, "v").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(reg_get_string(Hive::CurrentUser, &sub, "v").is_none());
    }

    #[test]
    fn roaming_appdata_exists() {
        let p = roaming_appdata().unwrap();
        assert!(p.is_absolute() && p.is_dir());
    }

    #[test]
    fn trim_working_set_runs() {
        trim_working_set();
    }

    #[test]
    fn foreground_does_not_panic() {
        let _ = foreground();
        let _ = foreground_exe();
    }

    #[test]
    fn local_offset_zero_out_of_range() {
        // Year > 30827 cannot be represented in SYSTEMTIME.
        assert_eq!(local_offset_secs(u64::MAX / 2), 0);
    }

    #[test]
    fn stop_event_create_resets_stale_signal() {
        let name = format!("Local\\GameTimeStaleTest-{}", std::process::id());
        let first = StopEvent::create(&name).unwrap();
        assert!(signal_stop(&name));
        // Second handle to the same (still signaled) event: create resets it.
        let second = StopEvent::create(&name).unwrap();
        assert!(!second.wait(0));
        drop(first);
    }
}
