#![cfg(windows)]

// Exercise the shared writer directly, including the temporary file retained
// when rename fails; do not depend on Git or apply-hook behavior for ACL checks.
#[allow(dead_code)]
#[path = "../src/state_io.rs"]
mod state_io;

use std::fs::{self, File};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;

fn assert_owner_only_protected(path: &Path) {
    let file = File::open(path).unwrap();
    assert_private_handle(&file);
}

fn assert_private_handle(file: &File) {
    let mut descriptor = null_mut();
    // SAFETY: The handle is live and all output pointers refer to local storage.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(result, 0, "GetSecurityInfo failed: {result}");
    let mut text = null_mut();
    let mut len = 0;
    // SAFETY: GetSecurityInfo returned a valid descriptor; conversion allocates
    // a UTF-16 string of the returned length, including its terminating NUL.
    let converted = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            &mut len,
        )
    };
    let error = std::io::Error::last_os_error();
    let sddl = if converted != 0 {
        // SAFETY: Successful conversion initialized text and len above.
        Some(unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(text, len as usize)) })
    } else {
        None
    };
    // SAFETY: Both allocations were returned by the Windows security APIs.
    unsafe {
        LocalFree(descriptor);
        if !text.is_null() {
            LocalFree(text.cast());
        }
    }
    let sddl = sddl.unwrap_or_else(|| panic!("Security descriptor conversion failed: {error}"));
    assert_eq!(sddl.trim_end_matches('\0'), "D:P(A;;FA;;;OW)");
}

#[test]
fn private_atomic_writer_preserves_owner_only_dacl_after_rename_and_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, "previous non-private contents").unwrap();
    state_io::write_atomic_private(&path, "private snapshot").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "private snapshot");
    assert_owner_only_protected(&path);
    state_io::write_atomic_private(&path, "replacement snapshot").unwrap();
    assert_owner_only_protected(&path);
}

#[test]
fn private_atomic_writer_protects_temp_file_before_rename() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("keep"), "block rename").unwrap();
    state_io::write_atomic_private(&path, "private snapshot").unwrap_err();
    let temp = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_file())
        .expect("temporary snapshot retained after failed rename");
    assert_eq!(fs::read_to_string(&temp).unwrap(), "private snapshot");
    assert_owner_only_protected(&temp);
}

#[test]
fn private_temp_file_is_protected_before_any_contents_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let file = state_io::windows::create_private_file(&dir.path().join("empty.tmp")).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
    assert_private_handle(&file);
}
