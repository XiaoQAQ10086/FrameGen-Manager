//! 内置资产（安装包里带的那批文件）与 core 期望的一致性，以及 DLSS5 资产报告的行为。
//!
//! 为什么需要这些测试：安装包内置资产是「装完即可部署」的前提。一旦 core 期望的文件名变了
//! 而 apps/electron/bundled-*/ 忘了跟着更新，用户拿到的就是缺文件的安装包 —— 这类错编译器
//! 发现不了，只会在用户点「部署」时才炸。这里把两份清单钉在一起，改名时立刻红。
//!
//! 第三个小节是行为测试：把 FGM_DATA_DIR 指到临时目录，先验证「空目录会报缺三件套」，
//! 再把内置的三件套拷进去，验证「齐了就不再报缺」—— 替代随退役界面删掉的那几个资产状态断言。

use framegen_core::{deploy, dlss5, scan, update, util};
use std::path::{Path, PathBuf};

fn bundle(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("apps")
        .join("electron")
        .join(sub)
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("读不到 {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

/// 帧生成资产目录里 core 期望的文件名（顶层）：6 个代理入口 + INI。
/// 0.4.0 起运行库/后端/内核都在 dlssg_sm86\\ 子目录里，由 ASSET_INVENTORY 管。
fn expected_framegen_assets() -> Vec<String> {
    let mut want: Vec<String> = scan::PROXY_PRIORITY
        .iter()
        .map(|p| (*p).to_owned())
        .collect();
    want.push(update::INI_REPO_PATH.to_owned());
    want.sort();
    want
}

/// 内置资产必须和 core 的分发表**逐字节**对得上。
///
/// 这条以前只比文件名；0.4.0 之后资产变成"一整个目录 + 由清单校验哈希"，
/// 所以测试也升级成：拿 deploy::ASSET_INVENTORY 挨个核大小与 SHA256。
/// 上游换版本、或打包时漏了一个文件，这里立刻红 —— 不用等用户在游戏里试。
#[test]
fn bundled_framegen_assets_match_core_expectations() {
    let dir = bundle("bundled-assets");
    let have = names_in(&dir);
    let want = expected_framegen_assets();

    let missing: Vec<&String> = want.iter().filter(|n| !have.contains(n)).collect();
    assert!(
        missing.is_empty(),
        "bundled-assets 缺少 core 期望的文件 {missing:?}\\n实际内容: {have:?}"
    );

    for (rel, size, sha) in deploy::ASSET_INVENTORY {
        let p = dir.join(rel);
        assert!(p.is_file(), "分发表里的 {rel} 不在内置资产里");
        assert_eq!(
            std::fs::metadata(&p).expect("读元数据").len(),
            *size,
            "{rel} 大小与分发表不符"
        );
        assert_eq!(
            util::sha256_file(&p).expect("算哈希"),
            *sha,
            "{rel} 的 SHA256 与分发表不符（上游换版本后忘了更新这张表？）"
        );
    }

    // 顶层不该有运行库了：0.4.0 起它们在 dlssg_sm86\runtime\ 里
    for n in deploy::LEGACY_ROOT_LIBS {
        assert!(
            !have.iter().any(|h| h == n),
            "顶层还留着 {n} —— 0.4.0 起运行库在 dlssg_sm86\\ 里，顶层这份不该再内置"
        );
    }
    // 反向确认：6 代理 + INI
    assert_eq!(want.len(), scan::PROXY_PRIORITY.len() + 1);
}

#[test]
fn bundled_dlss5_matches_core_expectations() {
    let dir = bundle("bundled-dlss5");
    let have = names_in(&dir);

    let is_addon = |n: &String| n.starts_with(dlss5::ADDON_PREFIX) && n.ends_with(dlss5::ADDON_EXT);
    let is_setup = |n: &String| n.starts_with(dlss5::SETUP_PREFIX) && n.ends_with(".exe");

    assert!(
        have.iter().any(|n| n == dlss5::MODEL_NAME),
        "bundled-dlss5 里没有 {}（core 按文件名精确查找）\n实际内容: {have:?}",
        dlss5::MODEL_NAME
    );
    assert!(
        have.iter().any(is_addon),
        "bundled-dlss5 里没有 {}*{}（core 按前缀+后缀查找）\n实际内容: {have:?}",
        dlss5::ADDON_PREFIX,
        dlss5::ADDON_EXT
    );
    assert!(
        have.iter().any(is_setup),
        "bundled-dlss5 里没有 {}*.exe（官方 ReShade 安装器）\n实际内容: {have:?}",
        dlss5::SETUP_PREFIX
    );

    let extra: Vec<&String> = have
        .iter()
        .filter(|n| {
            !(*n == dlss5::MODEL_NAME)
                && !is_addon(n)
                && !is_setup(n)
                && *n != dlss5::ADDON32_NAME
                && *n != dlss5::HELPER_NAME
                && *n != "effects"
                && !dlss5::FX_NAMES.iter().any(|f| f == n)
        })
        .collect();
    assert!(extra.is_empty(), "bundled-dlss5 里有多余文件 {extra:?}");
}

#[test]
fn dlss5_assets_report_detects_missing_then_present() {
    let root = std::env::temp_dir().join("fgm-bundled-assets-test");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("建临时数据根");
    // 本测试是这条二进制里唯一碰资产目录的，所以在这里设环境变量是安全的。
    std::env::set_var("FGM_DATA_DIR", &root);

    let d5 = util::assets_dir()
        .expect("资产目录应能解析")
        .join(dlss5::DIR_NAME);
    std::fs::create_dir_all(&d5).expect("建 DLSS5 资产目录");

    let before = dlss5::assets_report();
    assert!(
        !before.ready && before.missing.len() == before.items.len() && before.missing.len() >= 7,
        "空目录时应当把每一件资产都报缺（GITC 套共 7 件），实际: ready={} missing={:?}",
        before.ready,
        before.missing
    );

    for e in std::fs::read_dir(bundle("bundled-dlss5")).expect("读内置 DLSS5 资产") {
        let e = e.expect("目录项");
        // effects 是子目录（内置效果包），本测试只关心报告所列的顶层资产
        if e.path().is_dir() {
            continue;
        }
        std::fs::copy(e.path(), d5.join(e.file_name())).expect("拷贝内置资产");
    }

    let after = dlss5::assets_report();
    assert!(
        after.ready && after.missing.is_empty(),
        "放齐三件套后不该再报缺，实际: ready={} missing={:?}",
        after.ready,
        after.missing
    );

    let _ = std::fs::remove_dir_all(&root);
}
