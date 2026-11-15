//! The Win32 calls. Every `unsafe` block is small and says why it is sound. Order of cleanup in
//! `run` matters: job first (kills leftovers that could hold files), then the ACEs, then the profile.
#![allow(unsafe_code)]

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicPtr, Ordering};

use serde::{Deserialize, Serialize};

use windows_sys::Win32::Foundation::{
    CloseHandle, FALSE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree, S_OK,
    SetHandleInformation, TRUE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS,
    SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
    DeleteAce, EqualSid, FreeSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, INHERIT_ONLY_ACE, INHERITED_ACE, NO_INHERITANCE,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES,
    SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_UILIMIT_DESKTOP,
    JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicUIRestrictions, JobObjectExtendedLimitInformation, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateMutexW, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcessId,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, ReleaseMutex, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::{Access, Grant, Spec};

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

/// An owned kernel handle.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this value and closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A `LocalAlloc`ed block (ACLs, security descriptors, SIDs from `ConvertStringSidToSidW`).
struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the system with LocalAlloc, freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The throw-away AppContainer: deleted on drop.
struct Profile {
    text: String,
    name: Vec<u16>,
    sid: PSID,
}

impl Profile {
    fn create() -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // 64 characters at most; unique per run, so every run has its own SID.
        // SAFETY: GetCurrentProcessId has no preconditions.
        let name = format!("synt.sb.{}.{:x}", unsafe { GetCurrentProcessId() }, nanos);
        let text_name = name[..name.len().min(63)].to_string();
        let name = wide(OsStr::new(&text_name));
        let text = wide(OsStr::new("Synthesis sandbox"));
        let mut sid: PSID = null_mut();
        // SAFETY: all strings are NUL-terminated and outlive the call; no capabilities (null, 0);
        // `sid` is a valid out pointer.
        let hr = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                text.as_ptr(),
                text.as_ptr(),
                null(),
                0,
                &mut sid,
            )
        };
        if hr != S_OK {
            return Err(format!(
                "CreateAppContainerProfile failed (HRESULT {:#010x})",
                hr as u32
            ));
        }
        Ok(Self {
            text: text_name,
            name,
            sid,
        })
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        // SAFETY: `sid` came from CreateAppContainerProfile (freed with FreeSid, once); `name` is
        // NUL-terminated.
        unsafe {
            FreeSid(self.sid);
            DeleteAppContainerProfile(self.name.as_ptr());
        }
    }
}

pub fn probe() -> Result<(), String> {
    Profile::create().map(drop)
}

fn mask(a: Access) -> u32 {
    match a {
        // `ReadOnly` is a cut (see `Island`) that keeps this much
        Access::Read | Access::ReadOnly => FILE_GENERIC_READ | FILE_GENERIC_EXECUTE,
        // generic write (no WRITE_DAC/WRITE_OWNER, no FILE_DELETE_CHILD) plus delete of the object
        Access::Modify => FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
        Access::Hide | Access::Guard => 0,
    }
}

/// An explicit ACE for `sid` (or, with `mode` `REVOKE_ACCESS`, removal of every explicit ACE of
/// that SID) merged into `old`; the result is a new ACL owned by the caller.
fn acl_with(old: *mut ACL, sid: PSID, mask: u32, inherit: u32, mode: i32) -> Result<Local, String> {
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inherit,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    };
    let mut new: *mut ACL = null_mut();
    // SAFETY: `ea` is one valid entry whose SID outlives the call; `old` is valid (or null).
    let r = unsafe { SetEntriesInAclW(1, &ea, old, &mut new) };
    if r != 0 {
        return Err(format!("build ACL: error {r}"));
    }
    Ok(Local(new.cast()))
}

/// The DACL of `path`: the security descriptor that owns it (keep it alive) and the ACL pointer.
fn read_dacl(wpath: &[u16], path: &Path) -> Result<(Local, *mut ACL), String> {
    let mut acl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `wpath` is NUL-terminated; the out pointers are valid; on success `sd` owns `acl`.
    let r = unsafe {
        GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut acl,
            null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(format!("read ACL of {}: error {r}", path.display()));
    }
    let sd = Local(sd);
    if acl.is_null() {
        // a NULL DACL means "everyone"; editing it would turn it into a restrictive one
        return Err(format!(
            "{} has a NULL DACL, refusing to edit it",
            path.display()
        ));
    }
    Ok((sd, acl))
}

/// `flags` is `0` (leave the protection as it is), `PROTECTED_...` or `UNPROTECTED_...`. Setting a
/// DACL also propagates the inheritable ACEs to the existing children.
fn write_dacl(wpath: &[u16], path: &Path, acl: *const ACL, flags: u32) -> Result<(), String> {
    // SAFETY: `wpath` is NUL-terminated and `acl` is a valid ACL.
    let r = unsafe {
        SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | flags,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    };
    if r != 0 {
        return Err(format!("set ACL of {}: error {r}", path.display()));
    }
    Ok(())
}

/// A well-known SID from its text form (owned).
fn sid_from_text(t: &str) -> Result<Local, String> {
    let w = wide(OsStr::new(t));
    let mut sid: PSID = null_mut();
    // SAFETY: NUL-terminated string, valid out pointer; freed by `Local`.
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == FALSE {
        return Err(last_error("ConvertStringSidToSid"));
    }
    Ok(Local(sid))
}

fn sid_text(sid: PSID) -> Result<String, String> {
    let mut p: *mut u16 = null_mut();
    // SAFETY: `sid` is valid; `p` receives a LocalAlloc'd NUL-terminated string.
    if unsafe { ConvertSidToStringSidW(sid, &mut p) } == FALSE {
        return Err(last_error("ConvertSidToStringSid"));
    }
    let g = Local(p.cast());
    // SAFETY: `p` is NUL-terminated (and kept alive by `g`).
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    // SAFETY: `len` units are readable.
    let t = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
    drop(g);
    Ok(t)
}

/// Clears the "inherited" flag of every ACE in place, so that the ACEs survive protecting the DACL
/// (a protected DACL no longer gets them from the parent) as ordinary explicit ones. With
/// `strip_packages`, the allow ACEs for ALL APPLICATION PACKAGES and ALL RESTRICTED APPLICATION
/// PACKAGES are dropped (a hidden path must not stay readable for every container).
fn make_explicit(acl: *mut ACL, strip_packages: bool) -> Result<(), String> {
    let packages = if strip_packages {
        vec![sid_from_text("S-1-15-2-1")?, sid_from_text("S-1-15-2-2")?]
    } else {
        vec![]
    };
    let mut info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `acl` is valid; `info` has the size of the class asked for.
    let ok = unsafe {
        GetAclInformation(
            acl,
            (&raw mut info).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == FALSE {
        return Err(last_error("GetAclInformation"));
    }
    for i in (0..info.AceCount).rev() {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: `i` is below the ACE count of `acl`; `ace` points into `acl` on success.
        if unsafe { GetAce(acl, i, &mut ace) } == FALSE {
            return Err(last_error("GetAce"));
        }
        // SAFETY: every ACE starts with an ACE_HEADER, and the ACL is our own private copy.
        let (kind, drop_it) = unsafe {
            let h = ace.cast::<ACE_HEADER>();
            (*h).AceFlags &= !(INHERITED_ACE as u8);
            // an ACCESS_ALLOWED_ACE (type 0): header, mask, then the SID
            let sid = ace.cast::<u8>().add(8).cast::<c_void>();
            let hit = (*h).AceType == 0 && packages.iter().any(|p| EqualSid(p.0, sid) != FALSE);
            ((*h).AceType, hit)
        };
        let _ = kind;
        // SAFETY: deleting by an index below the count; later indexes are not used (reverse loop).
        if drop_it && unsafe { DeleteAce(acl, i) } == FALSE {
            return Err(last_error("DeleteAce"));
        }
    }
    Ok(())
}

/// How to undo one change; written to the journal BEFORE the change is made.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UndoRec {
    /// Remove the explicit ACEs of the container SID.
    Revoke,
    /// The path was cut off from inheritance: put the original DACL back (SDDL, which carries the
    /// protected and auto-inherited flags).
    Restore { sddl: String },
}

#[derive(Serialize, Deserialize)]
struct JournalFile {
    profile: String,
    sid: String,
    entries: Vec<(PathBuf, UndoRec)>,
}

fn sddl_of(sd: PSECURITY_DESCRIPTOR) -> Result<String, String> {
    let mut p: *mut u16 = null_mut();
    // SAFETY: `sd` is a valid descriptor; `p` receives a LocalAlloc'd NUL-terminated string.
    let ok = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            1,
            DACL_SECURITY_INFORMATION,
            &mut p,
            null_mut(),
        )
    };
    if ok == FALSE {
        return Err(last_error("ConvertSecurityDescriptorToString"));
    }
    let g = Local(p.cast());
    // SAFETY: NUL-terminated, kept alive by `g`.
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    // SAFETY: `len` units are readable.
    let t = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) });
    drop(g);
    Ok(t)
}

/// Put the DACL described by `sddl` back, protected or not as it was.
fn restore_sddl(path: &Path, sddl: &str) -> Result<(), String> {
    let w = wide(OsStr::new(sddl));
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: NUL-terminated string, valid out pointer; freed by `Local`.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), 1, &mut sd, null_mut())
    };
    if ok == FALSE {
        return Err(last_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptor",
        ));
    }
    let _sd = Local(sd);
    let (mut present, mut defaulted) = (0i32, 0i32);
    let mut acl: *mut ACL = null_mut();
    let (mut ctrl, mut rev) = (0u16, 0u32);
    // SAFETY: `sd` is valid; the out pointers are valid.
    let ok = unsafe {
        GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) != FALSE
            && GetSecurityDescriptorControl(sd, &mut ctrl, &mut rev) != FALSE
    };
    if !ok || present == 0 || acl.is_null() {
        return Err(format!("cannot read the saved DACL of {}", path.display()));
    }
    let flag = if ctrl & 0x1000 != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };
    write_dacl(&wide(path.as_os_str()), path, acl, flag)
}

fn undo_rec(path: &Path, sid: PSID, rec: &UndoRec) -> Result<(), String> {
    if !path.exists() {
        return Ok(()); // renamed or deleted since: nothing left to restore there
    }
    match rec {
        UndoRec::Revoke => {
            let wpath = wide(path.as_os_str());
            let (_sd, old) = read_dacl(&wpath, path)?;
            let new = acl_with(old, sid, 0, NO_INHERITANCE, REVOKE_ACCESS)?;
            write_dacl(&wpath, path, new.0.cast(), 0)
        }
        UndoRec::Restore { sddl } => restore_sddl(path, sddl),
    }
}

/// Change `path` for the container SID (the original state is already journaled).
fn change(path: &Path, sid: PSID, access: Access) -> Result<(), String> {
    let wpath = wide(path.as_os_str());
    let inherit = if path.is_dir() {
        SUB_CONTAINERS_AND_OBJECTS_INHERIT
    } else {
        NO_INHERITANCE
    };
    if matches!(access, Access::Read | Access::Modify) {
        let (_sd, old) = read_dacl(&wpath, path)?;
        let new = acl_with(old, sid, mask(access), inherit, GRANT_ACCESS)?;
        return write_dacl(&wpath, path, new.0.cast(), 0);
    }
    // The container has no access unless an ACE names it, so "no access below here" means "no ACE
    // for the container below here": cut the path off from the grants of its parents. (Deny ACEs
    // for the container SID were measured on CI and did not stop a read, so none is used.)
    let (_sd, work) = read_dacl(&wpath, path)?;
    make_explicit(work, access == Access::Hide)?;
    let mut keep: Vec<Local> = vec![];
    let mut acl = work;
    let mut add = |m: u32, inh: u32| -> Result<(), String> {
        let n = acl_with(acl, sid, m, inh, GRANT_ACCESS)?;
        acl = n.0.cast();
        keep.push(n);
        Ok(())
    };
    match access {
        Access::ReadOnly => add(mask(access), inherit)?,
        // `.git` and the like: the directory itself cannot be renamed or deleted (no DELETE on the
        // object, no delete-child on its parent), while everything below it is `Modify`.
        Access::Guard => {
            add(mask(Access::Modify) & !DELETE, NO_INHERITANCE)?;
            add(
                mask(Access::Modify),
                SUB_CONTAINERS_AND_OBJECTS_INHERIT | INHERIT_ONLY_ACE,
            )?;
        }
        _ => {}
    }
    write_dacl(&wpath, path, acl, PROTECTED_DACL_SECURITY_INFORMATION)
}

// Mutexes (`Global\` namespace: `Local\` is per logon session, and a run started from an ssh or
// service session next to a console one must be seen as alive): `synt-sb-run-<profile>` is held by the launcher for the whole run, so a journal whose
// mutex can be taken belongs to a dead launcher; `synt-sb-setup` serialises start and sweep.
fn mutex_name(tail: &str) -> Vec<u16> {
    wide(OsStr::new(&format!("Global\\synt-sb-{tail}")))
}

fn create_mutex(tail: &str, own: bool) -> Result<(Handle, bool), String> {
    let n = mutex_name(tail);
    // SAFETY: NUL-terminated name, default security.
    let h = unsafe { CreateMutexW(null(), i32::from(own), n.as_ptr()) };
    let existed = std::io::Error::last_os_error().raw_os_error() == Some(183);
    if h.is_null() {
        return Err(last_error("CreateMutex"));
    }
    Ok((Handle(h), existed))
}

/// Holds the setup mutex; released on drop.
struct Setup(Handle);

impl Setup {
    fn take() -> Result<Self, String> {
        let (h, _) = create_mutex("setup", false)?;
        // SAFETY: our own handle.
        match unsafe { WaitForSingleObject(h.0, 20_000) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(h)),
            _ => Err("another sandbox launcher holds the setup lock".into()),
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        // SAFETY: owned by this thread since `take`.
        unsafe { ReleaseMutex((self.0).0) };
    }
}

/// `true` when the launcher of `profile` is gone (its run mutex can be taken or does not exist).
fn launcher_dead(profile: &str) -> bool {
    let Ok((h, existed)) = create_mutex(&format!("run-{profile}"), false) else {
        return false;
    };
    // SAFETY: our own handle; a taken mutex is released before the handle closes.
    unsafe {
        if !existed {
            return true;
        }
        match WaitForSingleObject(h.0, 0) {
            WAIT_OBJECT_0 | WAIT_ABANDONED => {
                ReleaseMutex(h.0);
                true
            }
            _ => false,
        }
    }
}

/// A journal is trusted input only after this: it was written by us, but it sits on disk, and a
/// recovery writes ACLs on the paths it names.
fn validate(stem: &str, j: &JournalFile) -> Result<(), String> {
    let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit() || b == b'-');
    if j.profile != stem
        || !j.profile.starts_with("synt.sb.")
        || !j
            .profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.')
    {
        return Err(format!("bad profile name {:?}", j.profile));
    }
    if !j.sid.strip_prefix("S-1-15-2-").is_some_and(digits) {
        return Err(format!("bad container SID {:?}", j.sid));
    }
    for (p, rec) in &j.entries {
        let t = p.to_string_lossy();
        let b = t.as_bytes();
        let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
        if !drive || t.contains("..") || t.contains('\0') {
            return Err(format!("path {t:?} is not an absolute local drive path"));
        }
        if let UndoRec::Restore { sddl } = rec
            && (!sddl.starts_with("D:")
                || sddl.len() > 65_536
                || ["O:", "G:", "S:"].iter().any(|m| sddl.contains(m)))
        {
            return Err("saved descriptor is not a DACL-only SDDL".into());
        }
    }
    Ok(())
}

/// Every `*.json` journal; one that cannot be read, parsed or validated is an `Err` (never
/// dropped silently: callers treat it as a conflict and keep it).
fn journals(dir: &Path) -> Vec<(PathBuf, Result<JournalFile, String>)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let stem = p
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let j = std::fs::read(&p)
                .map_err(|e| e.to_string())
                .and_then(|b| serde_json::from_slice::<JournalFile>(&b).map_err(|e| e.to_string()))
                .and_then(|j| validate(&stem, &j).map(|()| j));
            (p, j)
        })
        .collect()
}

/// Undo what a dead launcher left (caller holds the setup mutex). A journal whose undo failed in
/// any step is KEPT (and so is the profile) to be retried; an invalid one is left alone.
fn recover(dir: &Path) -> usize {
    let mut n = 0;
    for (file, j) in journals(dir) {
        let j = match j {
            Ok(j) => j,
            Err(e) => {
                eprintln!(
                    "synthesis-sandbox-exec: recovery: ignoring {} ({e})",
                    file.display()
                );
                continue;
            }
        };
        if !launcher_dead(&j.profile) {
            continue;
        }
        let mut ok = false;
        if let Ok(sid) = sid_from_text(&j.sid) {
            ok = true;
            for (p, rec) in j.entries.iter().rev() {
                if let Err(e) = undo_rec(p, sid.0, rec) {
                    ok = false;
                    eprintln!("synthesis-sandbox-exec: recovery: {e}");
                }
            }
        }
        if ok {
            let w = wide(OsStr::new(&j.profile));
            // SAFETY: NUL-terminated name.
            unsafe { DeleteAppContainerProfile(w.as_ptr()) };
            let _ = std::fs::remove_file(file);
            n += 1;
        }
    }
    n
}

/// Restore what launchers that were killed left behind; returns how many runs were recovered.
pub fn sweep(dir: &Path) -> Result<usize, String> {
    let _setup = Setup::take()?;
    Ok(recover(dir))
}

fn fold(p: &Path) -> String {
    p.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase()
}

fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(&format!("{b}\\")) || b.starts_with(&format!("{a}\\"))
}

/// The paths this run is going to cut off must not be cut by a live run: two runs would snapshot
/// each other's cut as the "original" and the first to leave would lift the other's protection.
fn check_conflicts(dir: &Path, grants: &[Grant]) -> Result<(), String> {
    let ours: Vec<String> = grants
        .iter()
        .filter(|g| !matches!(g.access, Access::Read | Access::Modify))
        .map(|g| fold(&g.path))
        .collect();
    for (file, j) in journals(dir) {
        let j = match j {
            Ok(j) => j,
            Err(e) if ours.is_empty() => {
                let _ = e;
                continue;
            }
            Err(e) => {
                return Err(format!(
                    "unreadable or invalid sandbox journal {} ({e}): it may record protections that are still in place, so no path can be cut until it is dealt with; remove it by hand once you have checked what it records",
                    file.display()
                ));
            }
        };
        if launcher_dead(&j.profile) {
            continue;
        }
        for (p, rec) in &j.entries {
            if matches!(rec, UndoRec::Restore { .. })
                && let Some(mine) = ours.iter().find(|o| overlaps(o, &fold(p)))
            {
                return Err(format!(
                    "another sandboxed command in this workspace is running and already protects {mine} (or a path inside or above it); \
                     the protections are per path and cannot be shared: run one sandboxed command at a time here"
                ));
            }
        }
    }
    Ok(())
}

/// What this run changed; undone in reverse on drop (also when `run` fails half way), then the
/// journal is removed.
struct Applied {
    sid: PSID,
    journal: Option<PathBuf>,
    file: JournalFile,
    done: Vec<(PathBuf, UndoRec)>,
}

impl Applied {
    fn save(&mut self) -> Result<(), String> {
        self.file.entries.clone_from(&self.done);
        if let Some(p) = &self.journal {
            let t = serde_json::to_vec(&self.file).map_err(|e| e.to_string())?;
            // temp file + rename: a crash never leaves a half-written journal
            let tmp = p.with_extension("tmp");
            std::fs::write(&tmp, t).map_err(|e| format!("journal {}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, p).map_err(|e| format!("journal {}: {e}", p.display()))?;
        }
        Ok(())
    }
}

impl Drop for Applied {
    fn drop(&mut self) {
        let mut failed = false;
        for (p, rec) in self.done.iter().rev() {
            if let Err(e) = undo_rec(p, self.sid, rec) {
                failed = true;
                eprintln!("synthesis-sandbox-exec: cleanup: {e}");
            }
        }
        // a journal of an undo that failed is the only record of what is still changed: keep it
        // (the launcher is gone afterwards, so the next run retries it)
        if let (Some(p), false) = (&self.journal, failed) {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn apply(profile: &Profile, spec: &Spec) -> Result<Applied, String> {
    let mut a = Applied {
        sid: profile.sid,
        journal: None,
        file: JournalFile {
            profile: profile.text.clone(),
            sid: sid_text(profile.sid)?,
            entries: vec![],
        },
        done: vec![],
    };
    let _setup = match &spec.journal_dir {
        Some(d) => {
            std::fs::create_dir_all(d).map_err(|e| format!("journal dir {}: {e}", d.display()))?;
            let setup = Setup::take()?;
            // owner and SYSTEM only, no inheritance from the parent: the journal decides what a
            // later recovery writes, so no container (or other user) may touch it
            restore_sddl(d, "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)")?;
            recover(d);
            check_conflicts(d, &spec.grants)?;
            a.journal = Some(d.join(format!("{}.json", profile.text)));
            a.save()?;
            Some(setup)
        }
        None => None,
    };
    for g in &spec.grants {
        let rec = if matches!(g.access, Access::Read | Access::Modify) {
            UndoRec::Revoke
        } else {
            let wpath = wide(g.path.as_os_str());
            let (sd, _) = read_dacl(&wpath, &g.path)?;
            UndoRec::Restore {
                sddl: sddl_of(sd.0)?,
            }
        };
        // journaled first: a half-applied change is still undone, also after a crash
        a.done.push((g.path.clone(), rec));
        a.save()?;
        change(&g.path, profile.sid, g.access)?;
    }
    Ok(a)
}

/// The job to terminate on Ctrl-C / console close, so the normal cleanup runs.
static JOB: AtomicPtr<c_void> = AtomicPtr::new(null_mut());

/// Publishes the job to the console handler and withdraws it on drop, on every path out of `run`.
struct JobSlot;

impl JobSlot {
    fn set(h: HANDLE) -> Self {
        JOB.store(h, Ordering::SeqCst);
        Self
    }
}

impl Drop for JobSlot {
    fn drop(&mut self) {
        JOB.store(null_mut(), Ordering::SeqCst);
    }
}

unsafe extern "system" fn on_ctrl(_event: u32) -> i32 {
    let j = JOB.load(Ordering::SeqCst);
    if !j.is_null() {
        // SAFETY: `JobSlot` withdraws the pointer before the job handle is closed (it is declared
        // after the job, so it drops first, also on early returns). A handler running at that exact
        // instant could still see a just-closed handle: terminating a stale handle fails, harmlessly.
        unsafe { TerminateJobObject(j, 1) };
    }
    TRUE
}

/// Environment block of this launcher plus the variables it lacks (`Spec::launcher_env`): passed
/// explicitly, so the launcher's own environment is never modified.
fn env_block(extra: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    for (k, v) in extra {
        if !vars.iter().any(|(n, _)| n.eq_ignore_ascii_case(k)) {
            vars.push((k.into(), v.into()));
        }
    }
    vars.sort_by_key(|(k, _)| k.to_string_lossy().to_lowercase());
    let mut b: Vec<u16> = vec![];
    for (k, v) in vars {
        b.extend(k.encode_wide());
        b.push(u16::from(b'='));
        b.extend(v.encode_wide());
        b.push(0);
    }
    b.push(0);
    if b.len() == 1 {
        b.push(0);
    }
    b
}

fn job(spec: &Spec) -> Result<Handle, String> {
    // SAFETY: anonymous job; null attributes and name.
    let h = unsafe { CreateJobObjectW(null(), null()) };
    if h.is_null() {
        return Err(last_error("CreateJobObject"));
    }
    let h = Handle(h);
    // SAFETY: zeroed is a valid value of this plain C struct.
    let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let lim = &mut ext.BasicLimitInformation;
    lim.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if let Some(t) = spec.tasks {
        lim.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        lim.ActiveProcessLimit = t;
    }
    if let Some(m) = spec.memory {
        ext.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        ext.JobMemoryLimit = usize::try_from(m).unwrap_or(usize::MAX);
    }
    // SAFETY: `ext` is a fully initialised struct of the size passed with the matching class.
    let ok = unsafe {
        SetInformationJobObject(
            h.0,
            JobObjectExtendedLimitInformation,
            (&raw const ext).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == FALSE {
        return Err(last_error("SetInformationJobObject(limits)"));
    }
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
            | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
            | JOB_OBJECT_UILIMIT_EXITWINDOWS
            | JOB_OBJECT_UILIMIT_GLOBALATOMS
            | JOB_OBJECT_UILIMIT_HANDLES
            | JOB_OBJECT_UILIMIT_READCLIPBOARD
            | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
            | JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
    };
    // SAFETY: as above for the UI restriction class.
    let ok = unsafe {
        SetInformationJobObject(
            h.0,
            JobObjectBasicUIRestrictions,
            (&raw const ui).cast(),
            size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        )
    };
    if ok == FALSE {
        return Err(last_error("SetInformationJobObject(ui)"));
    }
    Ok(h)
}

/// The three standard handles, made inheritable, deduplicated; only these reach the child.
fn std_handles() -> ([HANDLE; 3], Vec<HANDLE>) {
    let mut std3 = [null_mut(); 3];
    let mut list: Vec<HANDLE> = vec![];
    for (i, id) in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
        .into_iter()
        .enumerate()
    {
        // SAFETY: plain queries on this process's own handles.
        let h = unsafe { GetStdHandle(id) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: `h` is a handle of this process.
        if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == FALSE {
            continue;
        }
        std3[i] = h;
        if !list.contains(&h) {
            list.push(h);
        }
    }
    (std3, list)
}

pub fn run(spec: &Spec) -> Result<u32, String> {
    if spec.program.to_string_lossy().contains('\0') || spec.command_line.contains('\0') {
        return Err("bad spec: NUL in the program or the command line".into());
    }
    let profile = Profile::create()?;
    // held for the whole run: tells other launchers (and the sweep) that this one is alive
    let (run_lock, _) = create_mutex(&format!("run-{}", profile.text), true)?;
    // network capabilities: SIDs owned by `_caps`
    let mut _caps: Vec<Local> = vec![];
    let mut caps: Vec<SID_AND_ATTRIBUTES> = vec![];
    if spec.network {
        for s in ["S-1-15-3-1", "S-1-15-3-2", "S-1-15-3-3"] {
            let w = wide(OsStr::new(s));
            let mut sid: PSID = null_mut();
            // SAFETY: NUL-terminated string, valid out pointer; freed by `Local`.
            if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == FALSE {
                return Err(last_error("ConvertStringSidToSid"));
            }
            _caps.push(Local(sid));
            caps.push(SID_AND_ATTRIBUTES {
                Sid: sid,
                Attributes: 4, // SE_GROUP_ENABLED
            });
        }
    }

    // Declaration order is drop order on every early return: job, then ACEs, then profile.
    let acl = apply(&profile, spec)?;
    let jobh = job(spec)?;
    let jobslot = JobSlot::set(jobh.0);
    // SAFETY: `on_ctrl` is a valid handler for the life of the process.
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), TRUE) };

    let mut sc = SECURITY_CAPABILITIES {
        AppContainerSid: profile.sid,
        Capabilities: if caps.is_empty() {
            null_mut()
        } else {
            caps.as_mut_ptr()
        },
        CapabilityCount: caps.len() as u32,
        Reserved: 0,
    };
    let (std3, mut handles) = std_handles();

    let mut size = 0usize;
    // SAFETY: the documented size query: null list, expected to fail with the needed size.
    unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size) };
    let mut buf = vec![0u64; size.div_ceil(8)];
    let list: LPPROC_THREAD_ATTRIBUTE_LIST = buf.as_mut_ptr().cast();
    // SAFETY: `list` points to `size` bytes, 8-aligned.
    if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut size) } == FALSE {
        return Err(last_error("InitializeProcThreadAttributeList"));
    }
    struct ListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for ListGuard {
        fn drop(&mut self) {
            // SAFETY: initialised above, deleted once.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _guard = ListGuard(list);
    // SAFETY: `sc` and `handles` outlive CreateProcessW; sizes match the pointed-to data.
    let ok = unsafe {
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            (&raw mut sc).cast(),
            size_of::<SECURITY_CAPABILITIES>(),
            null_mut(),
            null(),
        )
    };
    if ok == FALSE {
        return Err(last_error("UpdateProcThreadAttribute(capabilities)"));
    }
    if !handles.is_empty() {
        // SAFETY: as above; the list holds only inheritable handles of this process.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_mut_ptr().cast(),
                handles.len() * size_of::<HANDLE>(),
                null_mut(),
                null(),
            )
        };
        if ok == FALSE {
            return Err(last_error("UpdateProcThreadAttribute(handles)"));
        }
    }

    // SAFETY: zeroed is valid for these plain C structs.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = std3[0];
    si.StartupInfo.hStdOutput = std3[1];
    si.StartupInfo.hStdError = std3[2];
    si.lpAttributeList = list;
    // SAFETY: as above.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let app = wide(spec.program.as_os_str());
    let mut cmd = wide(OsStr::new(&spec.command_line)); // CreateProcessW may write into it
    let env = env_block(&spec.launcher_env);
    // SAFETY: every pointer is valid and NUL-terminated for the call; the environment and the
    // current directory are inherited from this process; only the listed handles are inherited.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            TRUE,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW,
            env.as_ptr().cast(),
            null(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == FALSE {
        let mut names: Vec<_> = std::env::vars_os()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        names.sort();
        return Err(last_error(&format!(
            "start {} in the AppContainer (launcher environment names: {})",
            spec.program.display(),
            names.join(", ")
        )));
    }
    let (process, thread) = (Handle(pi.hProcess), Handle(pi.hThread));
    // SAFETY: both handles are ours. The child has not run yet: if it cannot be put in the job
    // it is killed instead of resumed (fail closed).
    unsafe {
        if AssignProcessToJobObject(jobh.0, process.0) == FALSE {
            let e = last_error("AssignProcessToJobObject");
            TerminateProcess(process.0, 126);
            return Err(e);
        }
        if ResumeThread(thread.0) == u32::MAX {
            let e = last_error("ResumeThread");
            TerminateProcess(process.0, 126);
            return Err(e);
        }
    }
    drop(thread);
    // SAFETY: waiting on our own process handle.
    if unsafe { WaitForSingleObject(process.0, INFINITE) } == WAIT_FAILED {
        return Err(last_error("WaitForSingleObject"));
    }
    let mut code = 0u32;
    // SAFETY: valid handle, valid out pointer.
    if unsafe { GetExitCodeProcess(process.0, &mut code) } == FALSE {
        return Err(last_error("GetExitCodeProcess"));
    }
    drop(process);
    drop(jobslot);
    drop(jobh); // kill leftovers before the ACEs and the profile go
    drop(acl);
    drop(profile);
    // SAFETY: owned by this thread since it was created.
    unsafe { ReleaseMutex(run_lock.0) };
    Ok(code)
}
