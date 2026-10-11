//! Drive enumeration and free space via the Win32 API.

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub root: String,
    pub label: String,
    pub fs: String,
    pub removable: bool,
    pub free: u64,
    pub total: u64,
}

impl DriveInfo {
    pub fn describe(&self) -> String {
        format!(
            "{} {}  [{}{}]  {} free of {}",
            self.root,
            if self.label.is_empty() { "(no label)" } else { &self.label },
            self.fs,
            if self.removable { ", removable" } else { "" },
            crate::fsutil::human_bytes(self.free),
            crate::fsutil::human_bytes(self.total)
        )
    }
}

#[cfg(windows)]
mod imp {
    use super::DriveInfo;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };

    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// `(free_to_caller, total)` for the volume containing `path`.
    pub fn space(path: &str) -> Option<(u64, u64)> {
        let w = wide(path);
        let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut total_free) };
        (ok != 0).then_some((free, total))
    }

    pub fn list() -> Vec<DriveInfo> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let root = format!("{}:\\", (b'A' + i as u8) as char);
            let w = wide(&root);
            let kind = unsafe { GetDriveTypeW(w.as_ptr()) };
            if kind != DRIVE_REMOVABLE && kind != DRIVE_FIXED {
                continue;
            }
            let (mut name, mut fs) = ([0u16; 261], [0u16; 261]);
            let ok = unsafe {
                GetVolumeInformationW(
                    w.as_ptr(),
                    name.as_mut_ptr(),
                    name.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                )
            };
            if ok == 0 {
                continue; // e.g. empty card reader
            }
            let (free, total) = space(&root).unwrap_or((0, 0));
            out.push(DriveInfo {
                root,
                label: from_wide(&name),
                fs: from_wide(&fs),
                removable: kind == DRIVE_REMOVABLE,
                free,
                total,
            });
        }
        out
    }
}

#[cfg(not(windows))]
mod imp {
    use super::DriveInfo;
    pub fn space(_: &str) -> Option<(u64, u64)> {
        None
    }
    pub fn list() -> Vec<DriveInfo> {
        Vec::new()
    }
}

pub use imp::list;

/// `(free, total)` bytes of the volume containing `path` (any existing path or volume root).
pub fn space(path: &std::path::Path) -> Option<(u64, u64)> {
    imp::space(&crate::paths::volume_root(path)?)
}

/// Free bytes on the volume containing `path`.
pub fn free_space(path: &std::path::Path) -> Option<u64> {
    space(path).map(|(free, _)| free)
}
