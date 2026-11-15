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
    ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
    NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW,
    TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, FreeSid, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC,
    WRITE_OWNER,
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

fn mask_and_inherit(a: Access, dir: bool) -> (u32, u32, i32) {
    let write = FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | DELETE
        | FILE_DELETE_CHILD
        | WRITE_DAC
        | WRITE_OWNER;
    let inherit = if dir {
        SUB_CONTAINERS_AND_OBJECTS_INHERIT
    } else {
        NO_INHERITANCE
    };
    let (mask, mode) = match a {
        Access::Read => (FILE_GENERIC_READ | FILE_GENERIC_EXECUTE, GRANT_ACCESS),
        // generic write without WRITE_DAC/WRITE_OWNER (not part of it), plus delete
        Access::Modify => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE,
            GRANT_ACCESS,
        ),
        Access::DenyAll => (0x1F01FF, DENY_ACCESS), // FILE_ALL_ACCESS
        Access::DenyWrite => (write, DENY_ACCESS),
        Access::DenyDeleteChild => (FILE_DELETE_CHILD, DENY_ACCESS),
    };
    let inherit = if a == Access::DenyDeleteChild {
        NO_INHERITANCE
    } else {
        inherit
    };
    (mask, inherit, mode)
}

/// Edit the DACL of `path` for `sid`: add an ACE (`mode` from `mask_and_inherit`) or, with
/// `revoke`, remove every explicit ACE of that SID. Setting a DACL through
/// `SetNamedSecurityInfoW` also propagates inheritable ACEs to existing children.
fn edit_dacl(path: &Path, sid: PSID, g: Option<Access>, dir: bool) -> Result<(), String> {
    let (mask, inherit, mode) = match g {
        Some(a) => mask_and_inherit(a, dir),
        None => (0, NO_INHERITANCE, REVOKE_ACCESS),
    };
    let wpath = wide(path.as_os_str());
    let mut old: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `wpath` is NUL-terminated; the out pointers are valid; on success `sd` owns `old`.
    let r = unsafe {
        GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut old,
            null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(format!("read ACL of {}: error {r}", path.display()));
    }
    let _sd = Local(sd);
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
        return Err(format!("build ACL for {}: error {r}", path.display()));
    }
    let _new = Local(new.cast());
    // SAFETY: `wpath` is NUL-terminated and `new` is a valid ACL; the DACL is left unprotected
    // so inheritance keeps working.
    let r = unsafe {
        SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new,
            null(),
        )
    };
    if r != 0 {
        return Err(format!("set ACL of {}: error {r}", path.display()));
    }
    Ok(())
}

/// The ACEs this run added; removed on drop (also when `run` fails half way).
struct Applied<'a> {
    sid: PSID,
    paths: Vec<&'a Path>,
}

impl Drop for Applied<'_> {
    fn drop(&mut self) {
        for p in self.paths.iter().rev() {
            if let Err(e) = edit_dacl(p, self.sid, None, false) {
                eprintln!("synthesis-sandbox-exec: cleanup: {e}");
            }
        }
    }
}

fn apply<'a>(sid: PSID, grants: &'a [Grant]) -> Result<Applied<'a>, String> {
    let mut done = Applied { sid, paths: vec![] };
    for g in grants {
        let dir = g.path.is_dir();
        // recorded first: a half-applied edit is still revoked
        done.paths.push(&g.path);
        edit_dacl(&g.path, sid, Some(g.access), dir)?;
    }
    Ok(done)
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
        return Err(last_error(&format!(
            "start {} in the AppContainer",
            spec.program.display()
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
