use std::fs::{Metadata, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};

#[derive(PartialEq, Eq, Hash)]
pub(crate) enum FileIdentity {
    Extended(u64, [u8; 16]),
    Legacy(u32, u32, u32),
}

/// Read the stamp and the identity from one attribute-only handle, allowing
/// SQLite writes and library replacement while the handle exists.
pub(crate) fn metadata(path: &Path) -> io::Result<(Metadata, FileIdentity)> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)?;
    let metadata = file.metadata()?;
    let handle = file.as_raw_handle();
    let mut info = FILE_ID_INFO::default();
    // SAFETY: the file remains open and the buffer is a writable FILE_ID_INFO.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok != 0 {
        return Ok((
            metadata,
            FileIdentity::Extended(info.VolumeSerialNumber, info.FileId.Identifier),
        ));
    }

    // Some filesystems only support the older 64-bit file index.
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file remains open and the buffer is writable for the call.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        metadata,
        FileIdentity::Legacy(
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ),
    ))
}
