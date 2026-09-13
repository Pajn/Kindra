//! Create private state files with their final ACL already installed. Opening
//! first and tightening permissions later would let an inherited reader acquire
//! a handle before the file became private, then read subsequently written data.
use std::fs::{self, File};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};

pub(crate) fn create_private_file(path: &Path) -> io::Result<File> {
    // Canonicalizing the existing parent also supplies the extended-length
    // Windows prefix, preserving std::fs support for long and UNC paths.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing temporary filename"))?;
    let mut filename: Vec<u16> = fs::canonicalize(parent)?
        .join(name)
        .as_os_str()
        .encode_wide()
        .collect();
    if filename.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in temporary filename",
        ));
    }
    filename.push(0);
    // P disables inherited ACEs; the sole ACE grants full file access to the
    // owner (OW). No inherited reader can open the file, even while it is empty.
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)".encode_utf16().chain([0]).collect();
    let mut descriptor = null_mut();
    // SAFETY: sddl is NUL-terminated; descriptor points to writable storage.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    // SAFETY: All pointers remain valid through the call. CREATE_NEW never
    // opens an existing file; security is supplied at creation, before writing.
    let handle = unsafe {
        CreateFileW(
            filename.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    // Capture the OS error before LocalFree can overwrite the last-error value.
    let result = if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: CreateFileW returned a new owned handle; File closes it.
        Ok(unsafe { File::from_raw_handle(handle) })
    };
    // SAFETY: Conversion above allocated descriptor with LocalAlloc.
    unsafe {
        LocalFree(descriptor);
    }
    result
}
