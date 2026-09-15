//! 把 packaging\app.ico 编进 exe 的图标资源。
//!
//! 为什么不用 winres / embed-resource：本项目的硬要求是「不加依赖」。Windows SDK
//! 自带的 rc.exe 随 MSVC 工具链本来就在，直接调它，把产出的 .res 交给链接器即可
//! （cargo 的 `rustc-link-arg`），一个 build-dependency 都不需要。
//!
//! 找不到 rc.exe 或 app.ico 时**只警告、不中断**：换台没装 SDK 的机器照样能编，
//! 只是 exe 不带图标。

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let ico = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("packaging")
        .join("app.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    if std::env::var("CARGO_CFG_WINDOWS").is_err() {
        return; // 图标资源是 Windows 专属，别的目标直接跳过
    }
    if !ico.is_file() {
        println!("cargo:warning=packaging/app.ico not found, building without an icon");
        return;
    }
    let Some(rc) = find_rc() else {
        println!("cargo:warning=rc.exe not found (Windows SDK), building without an icon");
        return;
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // 现场生成 .rc（而不是把 .rc 放进仓库）：里面写绝对路径，rc.exe 从哪个工作目录
    // 被调用都找得到图标
    let rc_file = out_dir.join("app_icon.rc");
    // .rc 里的 \ 是转义字符（实测 \T 会变成制表符，rc 报 RC2135 file not found），
    // 所以路径里的反斜杠必须写成两个
    let path = ico.display().to_string().replace('\\', "\\\\");
    let body = format!("1 ICON \"{path}\"\n");
    if let Err(e) = std::fs::write(&rc_file, body) {
        println!("cargo:warning=cannot write app_icon.rc ({e}), building without an icon");
        return;
    }

    let res = out_dir.join("app_icon.res");
    let _ = std::fs::remove_file(&res);
    match Command::new(&rc)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res)
        .arg(&rc_file)
        .status()
    {
        Ok(s) if s.success() && res.is_file() => {
            println!("cargo:rustc-link-arg={}", res.display());
        }
        Ok(s) => println!("cargo:warning=rc.exe failed ({s}), building without an icon"),
        Err(e) => println!("cargo:warning=cannot run rc.exe ({e}), building without an icon"),
    }
}

/// 在 Windows SDK 的 bin 目录里找 rc.exe：取版本号最大的那个 x64 版。
/// 找不到就返回 None（调用方只警告）。
fn find_rc() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(base) = std::env::var(var) {
            roots.push(PathBuf::from(base).join("Windows Kits").join("10").join("bin"));
        }
    }
    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path().join("x64").join("rc.exe");
            if !path.is_file() {
                continue;
            }
            let ver: Vec<u32> = e
                .file_name()
                .to_string_lossy()
                .split('.')
                .filter_map(|p| p.parse().ok())
                .collect();
            if best.as_ref().map(|(v, _)| ver > *v).unwrap_or(true) {
                best = Some((ver, path));
            }
        }
    }
    best.map(|(_, p)| p)
}
