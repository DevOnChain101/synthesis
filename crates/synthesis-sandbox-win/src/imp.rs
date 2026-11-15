//! The Win32 calls. Every `unsafe` block is small and says why it is sound. Order of cleanup in
//! `run` matters: job first (kills leftovers that could hold files), then the ACEs, then the profile.
#![allow(unsafe_code)]

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{
    CloseHandle, FALSE, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree, S_OK,
    SetHandleInformation, TRUE, WAIT_FAILED,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
    NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION, FreeSid,
    GetAce, GetAclInformation, INHERITED_ACE, NO_INHERITANCE, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
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
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcessId, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
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
        let name = wide(OsStr::new(&name[..name.len().min(63)]));
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
        Ok(Self { name, sid })
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
        Access::Hide => 0,
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

/// Clears the "inherited" flag of every ACE in place, so that the ACEs survive protecting the DACL
/// (a protected DACL no longer gets them from the parent) as ordinary explicit ones.
fn make_explicit(acl: *mut ACL) -> Result<(), String> {
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
    for i in 0..info.AceCount {
        let mut ace: *mut c_void = null_mut();
        // SAFETY: `i` is below the ACE count of `acl`; `ace` points into `acl` on success.
        if unsafe { GetAce(acl, i, &mut ace) } == FALSE {
            return Err(last_error("GetAce"));
        }
        // SAFETY: every ACE starts with an ACE_HEADER, and the ACL is our own private copy.
        unsafe {
            let h = ace.cast::<ACE_HEADER>();
            (*h).AceFlags &= !(INHERITED_ACE as u8);
        }
    }
    Ok(())
}

/// What was changed on one path, to be undone by `Drop`.
enum Undo {
    /// Remove the explicit ACEs of the container SID.
    Revoke,
    /// The path was cut off from inheritance; put its original DACL back and let it inherit again.
    /// `_sd` owns `acl`.
    Restore { _sd: Local, acl: *mut ACL },
}

/// A path was changed to `access` for `sid`; returns how to undo it.
fn apply_one(path: &Path, sid: PSID, access: Access) -> Result<Undo, String> {
    let wpath = wide(path.as_os_str());
    let dir = path.is_dir();
    let inherit = if dir {
        SUB_CONTAINERS_AND_OBJECTS_INHERIT
    } else {
        NO_INHERITANCE
    };
    match access {
        Access::Read | Access::Modify => {
            let (_sd, old) = read_dacl(&wpath, path)?;
            let new = acl_with(old, sid, mask(access), inherit, GRANT_ACCESS)?;
            write_dacl(&wpath, path, new.0.cast(), 0)?;
            Ok(Undo::Revoke)
        }
        Access::Hide | Access::ReadOnly => {
            // The container has no access unless an ACE names it, so "no access below here" means
            // "no ACE for the container below here": cut the path off from the grants of its
            // parents. (Deny ACEs for the container SID were measured on CI and did not stop a
            // read, so none is used.) The original DACL is kept to restore it exactly.
            let (sd0, original) = read_dacl(&wpath, path)?;
            let (_sd1, work) = read_dacl(&wpath, path)?;
            make_explicit(work)?;
            let new = if access == Access::ReadOnly {
                Some(acl_with(work, sid, mask(access), inherit, GRANT_ACCESS)?)
            } else {
                None
            };
            let acl = new.as_ref().map_or(work, |n| n.0.cast());
            write_dacl(&wpath, path, acl, PROTECTED_DACL_SECURITY_INFORMATION)?;
            Ok(Undo::Restore {
                _sd: sd0,
                acl: original,
            })
        }
    }
}

fn undo_one(path: &Path, sid: PSID, undo: &Undo) -> Result<(), String> {
    let wpath = wide(path.as_os_str());
    match undo {
        Undo::Revoke => {
            let (_sd, old) = read_dacl(&wpath, path)?;
            let new = acl_with(old, sid, 0, NO_INHERITANCE, REVOKE_ACCESS)?;
            write_dacl(&wpath, path, new.0.cast(), 0)
        }
        Undo::Restore { acl, .. } => {
            write_dacl(&wpath, path, *acl, UNPROTECTED_DACL_SECURITY_INFORMATION)
        }
    }
}

/// What this run changed; undone in reverse on drop (also when `run` fails half way).
struct Applied<'a> {
    sid: PSID,
    done: Vec<(&'a Path, Undo)>,
}

impl Drop for Applied<'_> {
    fn drop(&mut self) {
        for (p, u) in self.done.iter().rev() {
            if let Err(e) = undo_one(p, self.sid, u) {
                eprintln!("synthesis-sandbox-exec: cleanup: {e}");
            }
        }
    }
}

fn apply<'a>(sid: PSID, grants: &'a [Grant]) -> Result<Applied<'a>, String> {
    let mut a = Applied { sid, done: vec![] };
    for g in grants {
        let undo = apply_one(&g.path, sid, g.access)?;
        a.done.push((&g.path, undo));
    }
    Ok(a)
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
    for (k, v) in &spec.launcher_env {
        if std::env::var_os(k).is_none() {
            // SAFETY: the launcher is single-threaded here (nothing else reads or writes the
            // environment before the process is created).
            unsafe { std::env::set_var(k, v) };
        }
    }
    let profile = Profile::create()?;
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
    let acl = apply(profile.sid, &spec.grants)?;
    let jobh = job(spec)?;

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
    // SAFETY: every pointer is valid and NUL-terminated for the call; the environment and the
    // current directory are inherited from this process; only the listed handles are inherited.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            TRUE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            null(),
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
    drop(jobh); // kill leftovers before the ACEs and the profile go
    drop(acl);
    drop(profile);
    Ok(code)
}
