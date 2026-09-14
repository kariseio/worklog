//! 고정 디스크(물리 볼륨) 열거 — git 저장소 "모든 드라이브 스캔" 루트.
//!
//! Windows: DRIVE_FIXED 이면서 심볼릭 링크가 실제 물리 볼륨(`\Device\HarddiskVolumeN`)을
//! 가리키는 것만. 가상/클라우드 드라이브(Dokan/WinFsp 계열 등)는 제외한다.
//! 그 외 OS: `/` 하나.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveInfo {
    /// 예: `C:\`
    pub path: String,
    /// 볼륨 레이블(없으면 빈 문자열).
    pub label: String,
}

/// 물리 볼륨 목록. 실패하면 존재하는 고정 드라이브 전부(레이블 없이).
#[cfg(windows)]
pub fn drives_info() -> Vec<DriveInfo> {
    use std::ptr::null_mut;

    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetVolumeInformationW, QueryDosDeviceW,
    };
    use windows_sys::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SetErrorMode};

    /// `GetDriveTypeW` 의 고정 디스크 값 (winbase.h DRIVE_FIXED).
    const DRIVE_FIXED: u32 = 3;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    let mut out = Vec::new();
    // SAFETY: 모든 호출은 문서화된 Win32 API 이고, 넘기는 버퍼는 이 스코프 안에서 살아 있으며
    // 길이를 함께 전달한다. SetErrorMode 는 "미디어 없음" 대화상자를 억제하고 원래 값으로 되돌린다.
    unsafe {
        let old_mode = SetErrorMode(SEM_FAILCRITICALERRORS);
        for c in b'A'..=b'Z' {
            let letter = c as char;
            let root = format!("{letter}:\\");
            let root_w = wide(&root);
            if GetDriveTypeW(root_w.as_ptr()) != DRIVE_FIXED {
                continue;
            }
            let dev_name = wide(&format!("{letter}:"));
            let mut dev = vec![0u16; 1024];
            if QueryDosDeviceW(dev_name.as_ptr(), dev.as_mut_ptr(), dev.len() as u32) == 0 {
                continue;
            }
            if !from_wide(&dev).starts_with("\\Device\\HarddiskVolume") {
                continue; // 가상/클라우드 드라이브 제외
            }
            let mut name = vec![0u16; 261];
            let ok = GetVolumeInformationW(
                root_w.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            );
            let label = if ok != 0 {
                from_wide(&name)
            } else {
                String::new()
            };
            out.push(DriveInfo { path: root, label });
        }
        SetErrorMode(old_mode);
    }
    if out.is_empty() {
        out = (b'A'..=b'Z')
            .map(|c| format!("{}:\\", c as char))
            .filter(|r| std::path::Path::new(r).exists())
            .map(|path| DriveInfo {
                path,
                label: String::new(),
            })
            .collect();
    }
    out
}

#[cfg(not(windows))]
pub fn drives_info() -> Vec<DriveInfo> {
    vec![DriveInfo {
        path: "/".into(),
        label: String::new(),
    }]
}

/// 물리 볼륨 루트 경로 목록. 예: `["C:\\", "D:\\"]`.
pub fn fixed_drives() -> Vec<String> {
    drives_info().into_iter().map(|d| d.path).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drives_are_existing_roots() {
        let ds = drives_info();
        assert!(!ds.is_empty());
        for d in &ds {
            assert!(std::path::Path::new(&d.path).exists(), "{}", d.path);
        }
        assert_eq!(
            fixed_drives(),
            ds.iter().map(|d| d.path.clone()).collect::<Vec<_>>()
        );
    }
}
