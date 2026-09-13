//! 从 EXE 提取图标。
//!
//! 走 Windows Shell + GDI，而不是自己解析 PE 资源区：
//! Shell 会替我们处理各种图标格式（32bpp DIB、PNG 压缩、8bpp 调色板、掩码透明），
//! 代码量小、覆盖所有游戏 —— 不管来自 Steam、Epic 还是手动指定。

use std::path::Path;

use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

/// 提取出来的图标：RGBA8 像素，可直接喂给 egui
#[derive(Clone)]
pub struct IconImage {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for IconImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IconImage({}x{})", self.width, self.height)
    }
}

fn wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut v: Vec<u16> = path.as_os_str().encode_wide().collect();
    // 实测坑：SHGetFileInfoW 遇到混合分隔符的路径（d:/steamsteamapps...）
    // 会直接失败。Steam 注册表里的 SteamPath 就是带正斜杠的，所以这里统一成反斜杠。
    for c in v.iter_mut() {
        if *c == b'/' as u16 {
            *c = b'\\' as u16;
        }
    }
    v.push(0);
    v
}

/// 取某个文件的图标。任何失败都返回 None，不 panic。
pub fn icon_of(path: &Path) -> Option<IconImage> {
    if !path.is_file() {
        return None;
    }
    let w = wide(path);

    unsafe {
        let mut shfi: SHFILEINFOW = std::mem::zeroed();
        let ok = SHGetFileInfoW(
            w.as_ptr(),
            0,
            &mut shfi,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_LARGEICON,
        );
        if ok == 0 || shfi.hIcon.is_null() {
            return None;
        }

        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(shfi.hIcon, &mut ii) == 0 {
            DestroyIcon(shfi.hIcon);
            return None;
        }

        let out = read_icon_bitmaps(ii.hbmColor, ii.hbmMask);

        if !ii.hbmColor.is_null() {
            DeleteObject(ii.hbmColor as HGDIOBJ);
        }
        if !ii.hbmMask.is_null() {
            DeleteObject(ii.hbmMask as HGDIOBJ);
        }
        DestroyIcon(shfi.hIcon);
        out
    }
}

/// 用 GetDIBits 把位图读成 32bpp 自顶向下的 BGRA，再转成 RGBA。
unsafe fn dib_bits(bitmap: HBITMAP, w: usize, h: usize, bit_count: u16) -> Option<Vec<u8>> {
    let hdc: HDC = CreateCompatibleDC(std::ptr::null_mut());
    if hdc.is_null() {
        return None;
    }

    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w as i32;
    bi.bmiHeader.biHeight = -(h as i32); // 负数 = 从上到下，省得翻转
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = bit_count;
    bi.bmiHeader.biCompression = BI_RGB;

    let bytes = (w * bit_count as usize).div_ceil(32) * 4 * h;
    let mut buf = vec![0u8; bytes];
    let got = GetDIBits(
        hdc,
        bitmap,
        0,
        h as u32,
        buf.as_mut_ptr() as *mut _,
        &mut bi,
        DIB_RGB_COLORS,
    );
    DeleteDC(hdc);

    if got == 0 {
        None
    } else {
        Some(buf)
    }
}

unsafe fn bitmap_size(bitmap: HBITMAP) -> Option<(usize, usize)> {
    let mut bm: BITMAP = std::mem::zeroed();
    let ok = GetObjectW(
        bitmap as HGDIOBJ,
        std::mem::size_of::<BITMAP>() as i32,
        &mut bm as *mut _ as *mut _,
    );
    if ok == 0 {
        return None;
    }
    let w = bm.bmWidth.max(0) as usize;
    let h = bm.bmHeight.max(0) as usize;
    if w == 0 || h == 0 || w > 1024 || h > 1024 {
        return None;
    }
    Some((w, h))
}

unsafe fn read_icon_bitmaps(hbm_color: HBITMAP, hbm_mask: HBITMAP) -> Option<IconImage> {
    if hbm_color.is_null() {
        return None;
    }
    let (w, h) = bitmap_size(hbm_color)?;
    let mut buf = dib_bits(hbm_color, w, h, 32)?;

    // BGRA -> RGBA，顺便看有没有 alpha 通道
    let mut any_alpha = false;
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
        if px[3] != 0 {
            any_alpha = true;
        }
    }

    if !any_alpha {
        // 老式图标没有 alpha 通道，透明信息在 1bpp 掩码位图里：
        // 掩码位为 1 表示透明。
        if let Some(mask) = mask_alpha(hbm_mask, w, h) {
            for (i, px) in buf.chunks_exact_mut(4).enumerate() {
                px[3] = mask[i];
            }
        } else {
            for px in buf.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
    }

    Some(IconImage {
        width: w,
        height: h,
        rgba: buf,
    })
}

unsafe fn mask_alpha(hbm_mask: HBITMAP, w: usize, h: usize) -> Option<Vec<u8>> {
    if hbm_mask.is_null() {
        return None;
    }
    let (mw, mh) = bitmap_size(hbm_mask)?;
    if mw < w || mh < h {
        return None;
    }
    let mask = dib_bits(hbm_mask, mw, mh, 1)?;

    // 1bpp：每字节 8 像素，每行按 4 字节对齐
    let stride = mw.div_ceil(32) * 4;
    let mut out = vec![255u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let idx = y * stride + x / 8;
            if idx >= mask.len() {
                continue;
            }
            let bit = (mask[idx] >> (7 - (x % 8))) & 1;
            if bit == 1 {
                out[y * w + x] = 0;
            }
        }
    }
    Some(out)
}
