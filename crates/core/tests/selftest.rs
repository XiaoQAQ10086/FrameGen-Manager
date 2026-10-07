//! core 的无界面自测。
//!
//! 历史：这一套原来住在退役的 egui 版 `src/main.rs` 里（那个包同时是根包），
//! 2026-10 把测试搬进 core、把 egui 界面整个删掉 —— 自测从此跟着被测代码走，
//! 而且不再需要 eframe/egui 那一大串依赖。
//!
//! 搬迁时**丢掉了 3 个耦合退役界面的小节**（图标 / 手动导入校验 / 旧缓存兼容）：
//! 它们测的是 egui 版的资产行状态机（`App::asset_state_of` 那一套），那套代码已随界面删除。
//! 另外丢掉了联网类自测（cancel/download/speed）—— 它们本来就不在 CI 里跑。

use framegen_core::anticheat::AcTier;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
fn ck(fails: &mut Vec<String>, ok: bool, what: &str) {
    println!("  [{}] {what}", if ok { "PASS" } else { "FAIL" });
    if !ok {
        fails.push(what.to_owned());
    }
}

fn spawn_http_server(bytes: u64, chunk: u64, delay_ms: u64) -> (u16, Arc<AtomicBool>) {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口失败");
    let port = listener.local_addr().expect("拿本地端口失败").port();
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if s2.load(Ordering::Relaxed) {
                break;
            }
            let Ok(mut sock) = stream else { continue };
            // 客户端中途放弃时不能让这里一直阻塞
            let _ = sock.set_write_timeout(Some(std::time::Duration::from_secs(2)));
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req);
            let head = format!(
                "HTTP/1.1 200 OK
Content-Length: {bytes}
Content-Type: application/octet-stream

"
            );
            if sock.write_all(head.as_bytes()).is_err() {
                continue;
            }
            let buf = vec![0u8; chunk as usize];
            for _ in 0..bytes.div_ceil(chunk.max(1)) {
                if s2.load(Ordering::Relaxed) || sock.write_all(&buf).is_err() {
                    break;
                }
                let _ = sock.flush();
                if delay_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
            }
        }
    });
    (port, stop)
}

fn sourcetest() -> usize {
    println!("===== 选源 + 下载 自测 =====");
    let mut fails: Vec<String> = Vec::new();

    println!("-- 源名字 --");
    ck(&mut fails, framegen_core::update::source_label("") == "官方源", "空前缀认成「官方源」");
    ck(
        &mut fails,
        framegen_core::update::source_label("https://gh-proxy.com/") == "gh-proxy.com",
        "镜像前缀转成人能看的名字",
    );

    println!("-- 排序（快的在前、没测过居中、太慢的垫底）--");
    let fast = "https://fast.example/".to_owned();
    let mid = "https://mid.example/".to_owned();
    let dead = "https://dead.example/".to_owned();
    let items = vec![
        (dead.clone(), Some(50u64)),
        (mid.clone(), None),
        (fast.clone(), Some(5000)),
    ];
    let ranked = framegen_core::update::rank_by_scores(&items);
    ck(&mut fails, ranked.first() == Some(&fast), "实测 5000 KB/s 的排最前");
    ck(&mut fails, ranked.get(1) == Some(&mid), "没测过的排中间");
    ck(&mut fails, ranked.get(2) == Some(&dead), "实测 50 KB/s 的垫底");

    let c = match framegen_core::update::client() {
        Ok(c) => c,
        Err(e) => {
            println!("[FAIL] 建客户端失败: {e}");
            return 1;
        }
    };
    let cancel = AtomicBool::new(false);
    let tmp = std::env::temp_dir();

    // 回归：慢源必须能慢慢下完，不能因为「平均速度低」就掐掉它去换下一个源 ——
    // 那样线路慢的用户永远换不到一个「够快」的源，下到一半就断了。
    // 这里用 4 MB 的响应、每 110ms 只给 32 KB（约 220 KB/s）来复现慢线路。
    let (slow_port, slow_stop) = spawn_http_server(4 * 1024 * 1024, 32 * 1024, 110);
    let d1 = tmp.join("fgm-slow-source-test.bin");
    let _ = std::fs::remove_file(&d1);
    let u1 = format!("http://127.0.0.1:{slow_port}/slow");
    println!("-- 慢源（约 220 KB/s）--");
    let t0 = std::time::Instant::now();
    // 顺手记下进度回调里报给界面的那几段文字 —— 用户能不能看懂就靠它
    let mut notes: Vec<String> = Vec::new();
    let r1 = framegen_core::update::download(
        &c,
        "slow-source-test",
        framegen_core::update::Sink {
            dest: &d1,
            cancel: &cancel,
            progress: &mut |_, _, note| {
                if notes.last().map(|n| n != note).unwrap_or(true) {
                    notes.push(note.to_owned());
                }
            },
        },
        &u1,
        None,
        "本地慢源",
    );
    let el = t0.elapsed().as_secs_f64();
    let msg1 = match &r1 {
        Ok(_) => {
            // download() 只写到 .part：校验通过后才落盘（和线上路径一致）
            let _ = framegen_core::update::commit_download(&d1);
            "成功".to_owned()
        }
        Err(e) => e.to_string(),
    };
    ck(&mut fails, r1.is_ok(), &format!("慢源照样下完（{el:.1} 秒）：{msg1}"));
    ck(
        &mut fails,
        std::fs::metadata(&d1).map(|m| m.len() == 4 * 1024 * 1024).unwrap_or(false),
        "慢源下到的字节数完整（4 MB）",
    );
    ck(
        &mut fails,
        notes.first().map(|n| n.starts_with("经 ")).unwrap_or(false),
        "进度里会说清楚用的是哪个源（经 xxx）",
    );
    ck(
        &mut fails,
        !notes.iter().any(|n| n.contains("速度不达标")),
        "不再出现「速度不达标，换下一个」",
    );

    // 小文件照旧
    let (small_port, small_stop) = spawn_http_server(512 * 1024, 64 * 1024, 0);
    let d2 = tmp.join("fgm-small-test.bin");
    let _ = std::fs::remove_file(&d2);
    let u2 = format!("http://127.0.0.1:{small_port}/small");
    let r2 = framegen_core::update::download(
        &c,
        "small-test",
        framegen_core::update::Sink {
            dest: &d2,
            cancel: &cancel,
            progress: &mut |_, _, _| {},
        },
        &u2,
        None,
        "本地小源",
    );
    println!("-- 小文件 --");
    if r2.is_ok() {
        let _ = framegen_core::update::commit_download(&d2);
    }
    ck(&mut fails, r2.is_ok(), "512 KB 的文件正常下完");
    ck(
        &mut fails,
        std::fs::metadata(&d2).map(|m| m.len() == 512 * 1024).unwrap_or(false),
        "小文件字节数正确",
    );

    for s in [&slow_stop, &small_stop] {
        s.store(true, Ordering::Relaxed);
    }
    for d in [&d1, &d2] {
        let _ = std::fs::remove_file(d);
    }

    println!("
===== 结果: {} 项失败 =====", fails.len());
    for f in &fails {
        println!("  - {f}");
    }
    fails.len()
}


fn selftest() -> usize {
    println!("===== FrameGen Manager 自检 =====");

    match framegen_core::util::app_data_dir() {
        Ok(d) => println!("[OK]   数据目录: {}", d.display()),
        Err(e) => println!("[FAIL] 数据目录: {e}"),
    }

    println!("\n--- Steam ---");
    let roots = framegen_core::scan::steam_roots();
    println!("根目录: {:?}", roots.iter().map(|p| p.display().to_string()).collect::<Vec<_>>());
    for r in &roots {
        for l in framegen_core::scan::steam_libraries(r) {
            println!("  库: {}", l.display());
        }
    }

    println!("\n--- 游戏扫描 ---");
    let games = framegen_core::scan::scan_all();
    println!("共 {} 个", games.len());
    for g in &games {
        let exe = framegen_core::scan::find_render_exe(&g.install_dir);
        let icon_info = exe
            .as_deref()
            .and_then(framegen_core::icon::icon_of)
            .map(|ic| {
                let total = ic.width * ic.height;
                let transparent = ic.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] == 0).count();
                format!(
                    "{}x{}（透明 {:.0}%）",
                    ic.width,
                    ic.height,
                    transparent as f64 / total as f64 * 100.0
                )
            })
            .unwrap_or_else(|| "未取到".to_owned());
        // 部署目标 = 渲染 EXE 所在目录（界面里「用作部署目录」用的也是它）
        let target = exe
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| g.install_dir.clone());
        let st = framegen_core::deploy::state_of(&target);
        println!(
            "  [{}] {}\n       dir    = {}\n       exe    = {}\n       icon   = {}\n       target = {}\n       状态   = {}",
            g.source.label(),
            g.name,
            g.install_dir.display(),
            exe.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "未找到".to_owned()),
            icon_info,
            target.display(),
            st.label()
        );
    }

    // WeGame：判定保守，只认「里面真能找到游戏程序」的目录。
    // 开发机上没装 WeGame，所以这里验的是**不变式**（扫出来的都必须是真的），不是数量。
    let wg = framegen_core::scan::scan_wegame();
    let wg_ok = wg
        .iter()
        .all(|g| g.install_dir.is_dir() && framegen_core::scan::find_render_exe(&g.install_dir).is_some());
    println!(
        "  [{}] WeGame 扫出 {} 条，每条都指向真实存在的游戏目录{}",
        if wg_ok { "PASS" } else { "FAIL" },
        wg.len(),
        if wg.is_empty() { "（本机没装 WeGame，属正常）" } else { "" }
    );
    for g in &wg {
        println!("      {} -> {}", g.name, g.install_dir.display());
    }
    println!(
        "  [{}] WeGame 没装就不列任何东西（本机装了 QQ，它的键也在 Tencent 下面）",
        if framegen_core::scan::wegame_installed() || wg.is_empty() { "PASS" } else { "FAIL" }
    );
    println!(
        "  [{}] 没把 QQNT 之类的客户端当成 WeGame 游戏",
        if wg.iter().all(|g| !g.name.eq_ignore_ascii_case("QQNT")) { "PASS" } else { "FAIL" }
    );
    println!(
        "  [{}] wegame_value_is_path_like：InstallPath=true / Name=false",
        if framegen_core::scan::wegame_value_is_path_like("InstallPath")
            && !framegen_core::scan::wegame_value_is_path_like("Name")
        {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "  [{}] wegame_name_from_key：带编号的后缀会被去掉",
        if framegen_core::scan::wegame_name_from_key("铁甲雄兵(2000806)") == "铁甲雄兵"
            && framegen_core::scan::wegame_name_from_key("DNF") == "DNF"
        {
            "PASS"
        } else {
            "FAIL"
        }
    );

    // Q1 回归：显卡型号怎么挑。本机只有一块卡，多卡和幽灵条目只能靠这个纯函数验。
    println!("  显卡型号挑选（nvidia-smi 优先 / 幽灵条目不参与）:");
    let mk = |sub: &str, name: &str, ver: &str| framegen_core::gpu::GpuAdapter {
        class_sub: sub.to_owned(),
        driver_name: name.to_owned(),
        driver_version: ver.to_owned(),
        enum_key: format!("ENUM/{sub}"),
        hardware_id: "PCI-VEN-10DE".to_owned(),
        device_desc: None,
    };
    // 自测用例表：(说明, 注册表枚举到的适配器, nvidia-smi 报的名字, 期望挑中的名字)
    type GpuCase<'a> = (&'a str, Vec<framegen_core::gpu::GpuAdapter>, Option<&'a str>, Option<&'a str>);
    let cases: [GpuCase<'_>; 4] = [
        (
            "能识别的 RTX 排在认不出来的 GT 1030 前面",
            vec![
                mk("0000", "NVIDIA GeForce GT 1030", "1"),
                mk("0001", "NVIDIA GeForce RTX 3050", "2"),
            ],
            None,
            Some("NVIDIA GeForce RTX 3050"),
        ),
        (
            "nvidia-smi 报的优先于注册表（注册表可能被别的工具改过）",
            vec![mk("0000", "NVIDIA GeForce GT 1030", "1")],
            Some("NVIDIA GeForce RTX 3050"),
            Some("NVIDIA GeForce RTX 3050"),
        ),
        (
            "两块都认得出来时按驱动版本从新到旧",
            vec![
                mk("0000", "NVIDIA GeForce RTX 3070", "32.0.16.1692"),
                mk("0001", "NVIDIA GeForce RTX 3080", "31.0.15.0000"),
            ],
            None,
            Some("NVIDIA GeForce RTX 3070"),
        ),
        ("一块卡都没有就返回空，不瞎猜", vec![], None, None),
    ];
    for (label, adapters, smi, want) in cases {
        let got = framegen_core::scan::pick_gpu_name(&adapters, smi.map(str::to_owned));
        println!(
            "    [{}] {label}（得到 {:?}）",
            if got.as_deref() == want { "PASS" } else { "FAIL" },
            got
        );
    }

    // 日志：用户反馈问题就靠它
    let log_file = framegen_core::log::path();
    println!(
        "  [{}] 日志文件已建立：{}",
        if log_file.map(|p| p.is_file()).unwrap_or(false) { "PASS" } else { "FAIL" },
        log_file
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "（没有）".to_owned())
    );

    println!("\n--- 系统反作弊（注册表服务/驱动） ---");
    let sys = framegen_core::anticheat::scan_system();
    for h in &sys.hits {
        println!("  [{:?}] {} <- {}", h.tier, h.name, h.evidence);
    }
    println!("系统结论: {}", sys.verdict().label());

    println!("\n--- 各游戏目录反作弊判定 ---");
    for g in &games {
        // 和界面里一致：根目录 + 渲染 EXE 所在目录一起看
        let mut r = framegen_core::anticheat::scan_deep(&g.install_dir);
        if let Some(dir) = framegen_core::scan::find_render_exe(&g.install_dir).and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            r.merge(framegen_core::anticheat::scan_game_dir(&dir));
        }
        if r.verdict() != AcTier::None {
            let names: Vec<String> = r.hits.iter().map(|h| format!("{} ({:?})", h.name, h.tier)).collect();
            println!("  {} -> {} : {}", g.name, r.verdict().label(), names.join(", "));
        }
    }

    println!("\n--- 上游更新检查（走 raw 的 HEAD + ETag，完全不占 API 配额） ---");
    match framegen_core::update::client() {
        Ok(c) => {
            println!("  README 版本: {:?}", framegen_core::update::fetch_version(&c));
            let st = framegen_core::update::load_state();
            // 和界面里一样：每个文件发一次 HEAD 拿 ETag，一次 API 都不调。
            // 名单：6 个代理入口 + 出厂 INI，都在仓库根目录那一套里（20/30 系通用）。
            // 上游 0.3.0 把 altnative/ 改成了 alternatives/，所以路径都从这里取。
            let mut specs: Vec<String> = framegen_core::scan::PROXY_PRIORITY
                .iter()
                .map(|p| framegen_core::update::proxy_repo_path(p).to_owned())
                .collect();
            specs.push(framegen_core::update::INI_REPO_PATH.to_owned());
            println!("  逐个 HEAD 取内容指纹（0 次 API 调用）：");
            // 直链探测走镜像优先、连接超时 8s，所以这一段很快：若按官方优先，第一次
            // 探测要先干等 github.com 的连接超时（15s）才轮到镜像。
            let t_head = std::time::Instant::now();
            for path in &specs {
                match framegen_core::update::probe_remote(&c, path) {
                    Ok(r) => {
                        let local = framegen_core::update::local_name(path);
                        println!(
                            "  {:<22} etag={} size={:>9} 需要下载={}",
                            local,
                            &r.etag[..r.etag.len().min(10)],
                            r.size,
                            st.needs_update(&local, &r)
                        );
                    }
                    Err(e) => println!("  {:<22} 取指纹失败: {e}", path),
                }
            }

            println!("
  DLSS 运行库（界面里会单独分组显示）：");
            for (_prefix, tag, zip_name, dll_name, _label) in framegen_core::update::DLSS_RUNTIME {
                let url = framegen_core::update::release_url(tag, zip_name);
                let dest = framegen_core::update::asset_path(dll_name).unwrap_or_default();
                let ready =
                    dest.is_file() && framegen_core::scan::identify_dll(&dest) == framegen_core::scan::FileIdentity::Nvidia;
                let size = framegen_core::update::probe_url(&c, &url).map(|(n, _)| n).unwrap_or(0);
                println!(
                    "  {:<22} {}  直链大小 {}  本地={}",
                    dll_name,
                    tag,
                    framegen_core::util::format_bytes(size),
                    if ready { "已就绪(NVIDIA 签名)" } else { "未下载" }
                );
            }
            println!(
                "  取指纹总耗时 {:.1}s（以前光等 github.com 连接超时就要 15s）",
                t_head.elapsed().as_secs_f64()
            );
        }
        Err(e) => println!("  客户端创建失败: {e}"),
    }

    println!("\n--- 显卡与路由 ---");
    let gpu = framegen_core::scan::detect_gpu();
    let route = gpu
        .as_deref()
        .map(framegen_core::scan::classify_gpu)
        .unwrap_or(framegen_core::scan::GpuRoute::Unknown);
    println!("  显卡: {:?}", gpu);
    println!("  路由: {} ({:?})", route.label(), route);

    println!("\n--- 代理入口推荐（按真实部署目录，也就是渲染 EXE 所在目录）---");
    for g in &games {
        let target = framegen_core::scan::find_render_exe(&g.install_dir)
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| g.install_dir.clone());
        let a = framegen_core::scan::advise_proxy(&target);
        println!(
            "  [{}]\n       游戏目录 = {}",
            g.name,
            target.display()
        );
        println!(
            "       推荐 {}  依据={}  已分析 {} 个模块  未判定={}",
            a.recommended, a.reason, a.scanned, a.undetermined
        );
        for e in &a.own_existing {
            println!(
                "       已装过本项目: {}（{}，{}）-> 可直接覆盖",
                e.name,
                framegen_core::util::format_bytes(e.bytes),
                e.identity.label()
            );
        }
        for e in &a.occupied {
            println!(
                "       被占用: {}（{}，{}）-> 已跳过",
                e.name,
                framegen_core::util::format_bytes(e.bytes),
                e.identity.label()
            );
        }
    }

    // 上游 0.3.0 换过文件名、路径和 README 写法，下面这两组断言就是防它再改一次
    let mut fails: Vec<String> = Vec::new();

    println!("\n--- 部署用的 INI 档位改写 ---");
    {
        let dir = std::env::temp_dir().join("fgm-ini-tiers");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("dlssg_sm86.ini");
        std::fs::write(&src, "; c\n[FrameGeneration]\nOptimized=1\nMaxGeneratedFrames=3\n").unwrap();
        let (p, n) = framegen_core::update::prepare_deploy_ini(
            &src,
            framegen_core::update::DEFAULT_OPTIMIZED,
            framegen_core::update::DEFAULT_MAX_FRAMES,
        )
        .unwrap();
        ck(&mut fails, p == src && n.is_empty(), "默认档位：直接用上游原文件，不做任何改写");
        let (p2, n2) = framegen_core::update::prepare_deploy_ini(&src, 2, 5).unwrap();
        let txt = std::fs::read_to_string(&p2).unwrap_or_default();
        ck(
            &mut fails,
            p2 != src
                && txt.contains("Optimized=2")
                && txt.contains("MaxGeneratedFrames=5")
                && !txt.contains("Optimized=1"),
            "换档位：只改这两个键，其余内容原样",
        );
        ck(&mut fails, n2.len() == 2, "两处改动都写进说明");
        ck(
            &mut fails,
            std::fs::read_to_string(&src)
                .map(|t| t.contains("Optimized=1"))
                .unwrap_or(false),
            "资产目录里那份原文件保持不动",
        );
        let slim = dir.join("slim.ini");
        std::fs::write(&slim, "; slim\n[General]\nEnabled=1\n").unwrap();
        ck(
            &mut fails,
            framegen_core::update::prepare_deploy_ini(&slim, 3, 5).is_ok(),
            "INI 里没有那两个键也不报错（照旧部署）",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    ck(
        &mut fails,
        framegen_core::util::AppConfig::default().fg_optimized == framegen_core::update::DEFAULT_OPTIMIZED
            && framegen_core::util::AppConfig::default().fg_frames == framegen_core::update::DEFAULT_MAX_FRAMES,
        "配置的默认档位与出厂默认一致（配置丢了也不会变成档位 0）",
    );

    println!("\n--- 显卡路由（哪张卡走哪条路）---");
    for (name, want) in [
        ("NVIDIA GeForce RTX 3050", framegen_core::scan::GpuRoute::Sm86),
        ("NVIDIA GeForce RTX 3070 Ti", framegen_core::scan::GpuRoute::Sm86),
        ("NVIDIA GeForce RTX 2080 SUPER", framegen_core::scan::GpuRoute::Sm75),
        ("NVIDIA GeForce RTX 2060", framegen_core::scan::GpuRoute::Sm75),
        ("NVIDIA GeForce GTX 1660 SUPER", framegen_core::scan::GpuRoute::Gtx16),
        ("NVIDIA GeForce GTX 1650", framegen_core::scan::GpuRoute::Gtx16),
        ("NVIDIA GeForce GTX 1630", framegen_core::scan::GpuRoute::Gtx16),
        ("NVIDIA GeForce RTX 4070", framegen_core::scan::GpuRoute::NotNeeded),
        ("AMD Radeon RX 6800 XT", framegen_core::scan::GpuRoute::Unsupported),
        // Intel 核显 / Arc：和 AMD 同一类（非 NVIDIA，驱动接口就不对）
        ("Intel(R) UHD Graphics 630", framegen_core::scan::GpuRoute::Unsupported),
        ("Intel(R) Arc(TM) A770 Graphics", framegen_core::scan::GpuRoute::Unsupported),
        // Pascal 及更早 / MX / Quadro：都没有 Tensor Core，装上也白装 ——
        // 以前这些落到 Unknown 会被闸门静默放行（用户按流程做完发现没效果）。
        ("NVIDIA GeForce GT 1030", framegen_core::scan::GpuRoute::NoTensorCore),
        ("NVIDIA GeForce GTX 1060", framegen_core::scan::GpuRoute::NoTensorCore),
        ("NVIDIA GeForce GTX 1080 Ti", framegen_core::scan::GpuRoute::NoTensorCore),
        ("NVIDIA GeForce MX150", framegen_core::scan::GpuRoute::NoTensorCore),
        ("NVIDIA Quadro P2000", framegen_core::scan::GpuRoute::NoTensorCore),
        // 认不出来的（比如以后的新型号）仍然是 Unknown：**不能**当成旧卡去拦，
        // 否则每出一代新卡用户都会被自己的工具挡住。
        ("NVIDIA GeForce RTX 9090", framegen_core::scan::GpuRoute::Unknown),
    ] {
        ck(
            &mut fails,
            framegen_core::scan::classify_gpu(name) == want,
            &format!("{name} → {}", want.label()),
        );
    }
    // GTX 16 系必须被挡在部署外面，而且绝不能和 RTX 20 系混成一条路 ——
    // 混了的话界面会给出一条根本无效的建议。
    ck(
        &mut fails,
        framegen_core::scan::classify_gpu("NVIDIA GeForce GTX 1660 Ti") == framegen_core::scan::GpuRoute::Gtx16,
        "GTX 16 系单独成一路（不会被当成 RTX 20 系）",
    );

    println!("\n--- 部署闸门（哪张卡被拦住、哪张能装）---");
    for route in [
        framegen_core::scan::GpuRoute::Sm86,
        framegen_core::scan::GpuRoute::Sm75,
        framegen_core::scan::GpuRoute::Unknown,
    ] {
        ck(
            &mut fails,
            route.gate().is_none(),
            &format!("{}：放行（不拦）", route.label()),
        );
    }
    for (route, must_contain) in [
        (framegen_core::scan::GpuRoute::Gtx16, "Tensor Core"),
        (framegen_core::scan::GpuRoute::NotNeeded, "40/50"),
        (framegen_core::scan::GpuRoute::Unsupported, "非 NVIDIA"),
    ] {
        ck(
            &mut fails,
            route.gate().map(|w| w.contains(must_contain)).unwrap_or(false),
            &format!("{}：禁止部署", route.label()),
        );
    }

    println!("\n--- 上游版本号解析（新旧两种写法都要认）---");
    let cases: [(&str, Option<&str>); 4] = [
        ("# DLSSG Native 0.2.4\n", Some("0.2.4")),
        ("# DLSSG for SM86（Proxy）- 0.3.0 版本\n", Some("0.3.0")),
        ("# DLSSG for SM86 (proxy) - 0.3.0 Version\n", Some("0.3.0")),
        ("这一行没有任何版本号\n", None),
    ];
    for (text, want) in cases {
        let got = framegen_core::update::extract_version(text);
        ck(
            &mut fails,
            got.as_deref() == want,
            &format!("{:?} -> {:?}", text.trim(), got),
        );
    }

    println!("\n--- 仓库路径（20/30 系同一套文件）---");
    ck(
        &mut fails,
        framegen_core::update::proxy_repo_path("version.dll") == "version.dll",
        "默认入口 version.dll 在仓库根目录",
    );
    ck(
        &mut fails,
        framegen_core::update::proxy_repo_path("winmm.dll") == "alternatives/winmm.dll",
        "备用入口在 alternatives/（以前叫 altnative/）",
    );
    ck(
        &mut fails,
        framegen_core::update::proxy_repo_path("dbghelp.dll") == "alternatives/dbghelp.dll"
            && framegen_core::update::proxy_repo_path("d3d12.dll") == "alternatives/d3d12.dll",
        "0.3.0 新增的 dbghelp / d3d12 也能找到",
    );
    ck(
        &mut fails,
        framegen_core::update::INI_REPO_PATH == "dlssg_sm86.ini",
        "配置就是根目录那份出厂 INI（20/30 系通用，不需要改写）",
    );
    ck(
        &mut fails,
        framegen_core::scan::PROXY_PRIORITY.len() == 6,
        "现在有 6 个代理入口",
    );
    ck(
        &mut fails,
        framegen_core::scan::is_known_proxy("winhttp.dll") && framegen_core::scan::is_known_proxy("d3d12.dll"),
        "历史上出现过的入口名都算代理入口",
    );
    ck(
        &mut fails,
        !framegen_core::scan::is_known_proxy("nvngx_dlssg.dll"),
        "DLSS 运行库不算代理入口",
    );

    // （这里原来有一组「SM75 改写 INI」的自测。上游 0.3.1 起 20/30 系共用同一份
    //  出厂 INI、一个键都不用改，所以整个改写子系统连同这些断言都删掉了。）

    println!("\n--- 部署时的 DLSS 运行库处理（已有的不覆盖）---");
    {
        let dir = std::env::temp_dir().join("fgm-runtime-plan-selftest");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let (have, need) = framegen_core::update::runtime_deploy_plan(&dir);
        ck(
            &mut fails,
            have.is_empty() && need.len() == 2,
            "游戏目录里两个都没有 → 两个都补",
        );

        std::fs::write(dir.join("nvngx_dlssg.dll"), b"x").unwrap();
        let (have, need) = framegen_core::update::runtime_deploy_plan(&dir);
        ck(
            &mut fails,
            have == vec!["nvngx_dlssg.dll"] && need == vec!["nvngx_dlss.dll"],
            "已有帧生成运行库 → 只补超分那个",
        );

        std::fs::write(dir.join("nvngx_dlss.dll"), b"x").unwrap();
        let (have, need) = framegen_core::update::runtime_deploy_plan(&dir);
        ck(
            &mut fails,
            have.len() == 2 && need.is_empty(),
            "两个都有 → 一个都不动（不覆盖游戏自带的，这是用户反馈的那条）",
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    println!("\n--- 日志滚动（目录里只留两个文件）---");
    {
        let dir = std::env::temp_dir().join("fgm-log-selftest");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let names = |d: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        };
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();

        // 情况一：老版本留下的多个带时间戳文件（用户升级上来就是这个样）
        for n in [
            "framegen-20260101010101UTC.log",
            "framegen-20260202020202UTC.log",
        ] {
            let _ = std::fs::write(dir.join(n), b"legacy");
        }
        let cur = framegen_core::log::rotate(&dir);
        ck(
            &mut fails,
            cur == dir.join(framegen_core::log::CUR_NAME),
            "本次写的是固定名字 framegen.log",
        );
        ck(
            &mut fails,
            names(&dir) == vec![framegen_core::log::PREV_NAME.to_owned()],
            &format!(
                "升级上来第一次滚动：旧的带时间戳文件全清掉，只留 prev（实际 {:?}）",
                names(&dir)
            ),
        );
        ck(
            &mut fails,
            read(&dir.join(framegen_core::log::PREV_NAME)) == "legacy",
            "老版本里最新那份被留成「上一次」，没白丢",
        );

        // 情况二：正常一轮滚动 —— 本次的变成「上一次」，更早的删掉
        std::fs::write(dir.join(framegen_core::log::CUR_NAME), b"run2").unwrap();
        framegen_core::log::rotate(&dir);
        ck(
            &mut fails,
            read(&dir.join(framegen_core::log::PREV_NAME)) == "run2",
            "上一次运行的内容被滚到 prev",
        );
        ck(
            &mut fails,
            !dir.join(framegen_core::log::CUR_NAME).is_file(),
            "本次的文件先不存在（由 init 新建，保证从干净文件开始写）",
        );
        ck(
            &mut fails,
            names(&dir) == vec![framegen_core::log::PREV_NAME.to_owned()],
            "第二轮之后目录里只剩 prev 一个（init 马上会建本次那个）",
        );

        // 情况三：再滚一轮，prev 被删、本次上位
        std::fs::write(dir.join(framegen_core::log::CUR_NAME), b"run3").unwrap();
        framegen_core::log::rotate(&dir);
        ck(
            &mut fails,
            read(&dir.join(framegen_core::log::PREV_NAME)) == "run3" && names(&dir).len() == 1,
            "继续滚动也不会堆积（永远最多 2 个文件）",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    println!("\n--- 无启动器游戏库扫描 ---");
    {
        let base = std::env::temp_dir().join("fgm-loose");
        let _ = std::fs::remove_dir_all(&base);
        let lib = base.join("Games");
        let big = vec![0u8; 128 * 1024];
        let g1 = lib.join("My Green Game");
        let _ = std::fs::create_dir_all(&g1);
        let _ = std::fs::write(g1.join("MyGame.exe"), &big);
        let g2 = lib.join("JustAnInstaller");
        let _ = std::fs::create_dir_all(&g2);
        let _ = std::fs::write(g2.join("unins000.exe"), &big);
        let g3 = lib.join("EmptyShell");
        let _ = std::fs::create_dir_all(&g3);
        let g4 = lib.join("Content");
        let _ = std::fs::create_dir_all(&g4);
        let _ = std::fs::write(g4.join("thing.exe"), &big);
        let mut notes2: Vec<String> = Vec::new();
        let found = framegen_core::scan::scan_loose_in(std::slice::from_ref(&lib), &mut notes2);
        ck(
            &mut fails,
            found.len() == 1 && found[0].name == "My Green Game",
            "库目录扫描：只认「有像样 exe」的目录，安装器 / 空壳 / Content 都排除",
        );
        ck(
            &mut fails,
            found.first().map(|g| g.source.label()) == Some("本地目录"),
            "扫到的游戏来源标成「本地目录」",
        );
        ck(
            &mut fails,
            notes2.iter().any(|n| n.contains("本地目录：")),
            "会写一条扫描说明，用户看得懂这些游戏是哪来的",
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- 图形 API 与引擎判定（用真实世界的例子构造数据）----
    println!("\n--- 图形 API 与引擎 ---");
    {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
        let api = |imp: &[&str], sib: &[&str]| {
            framegen_core::scan::api_from_names(&s(imp), &s(sib))
        };
        ck(
            &mut fails,
            api(&["d3d12.dll", "dxgi.dll"], &[]) == framegen_core::scan::GraphicsApi::Dx12,
            "导入 d3d12.dll -> DX12",
        );
        let facts = |exe: &str, imp: &[&str], sib: &[&str], wrap: bool| {
            framegen_core::scan::api_from_facts(&framegen_core::scan::ApiFacts {
                exe_name: exe,
                imports: &s(imp),
                siblings: &s(sib),
                wrapper_is_vulkan: wrap,
            })
        };
        ck(
            &mut fails,
            facts("farcry3_d3d11.exe", &[], &[], false) == framegen_core::scan::GraphicsApi::Dx11,
            "exe 名里带 d3d11 -> DX11（一个渲染器一个 exe 的游戏）",
        );
        ck(
            &mut fails,
            facts("game_vulkan.exe", &["d3d11.dll"], &[], false) == framegen_core::scan::GraphicsApi::Vulkan,
            "exe 名优先于导入表",
        );
        ck(
            &mut fails,
            facts("normal.exe", &["d3d12.dll"], &["dxgi.dll"], true) == framegen_core::scan::GraphicsApi::Vulkan,
            "同目录是 DXVK/vkd3d 包装时算 Vulkan（不是 DX12）",
        );
        ck(
            &mut fails,
            api(&["vulkan-1.dll"], &[]) == framegen_core::scan::GraphicsApi::Vulkan,
            "导入 vulkan-1.dll -> Vulkan",
        );
        ck(
            &mut fails,
            api(&["d3d11.dll", "dxgi.dll"], &[]) == framegen_core::scan::GraphicsApi::Dx11,
            "导入 d3d11.dll -> DX11",
        );
        ck(
            &mut fails,
            api(&["dxgi.dll"], &[]) == framegen_core::scan::GraphicsApi::Unknown,
            "只有 dxgi.dll 不算数（10/11/12 都用它）",
        );
        ck(
            &mut fails,
            api(&[], &["vulkan-1.dll"]) == framegen_core::scan::GraphicsApi::Vulkan,
            "导入表为空（运行时加载）时看同目录自带运行库",
        );
        ck(
            &mut fails,
            framegen_core::scan::GraphicsApi::Dx11.frame_gen_possible() == Some(false)
                && framegen_core::scan::GraphicsApi::Dx12.frame_gen_possible() == Some(true)
                && framegen_core::scan::GraphicsApi::Unknown.frame_gen_possible().is_none(),
            "帧生成只在 DX12 / Vulkan 下有意义，未知时不下结论",
        );
        let eng = |exe: &str, sib: &[&str], pak: &[&str]| {
            framegen_core::scan::engine_from_facts(&framegen_core::scan::LayoutFacts {
                exe_name: exe.to_lowercase(),
                siblings: s(sib),
                pak_kinds: s(pak),
                root_exts: Vec::new(),
            })
        };
        // 靠资源扩展名认的几家（RAGE / Anvil / Frostbite / Creation）
        let eng_ext = |exts: &[&str]| {
            framegen_core::scan::engine_from_facts(&framegen_core::scan::LayoutFacts {
                exe_name: "game.exe".to_owned(),
                siblings: Vec::new(),
                pak_kinds: Vec::new(),
                root_exts: s(exts),
            })
        };
        ck(
            &mut fails,
            eng("HogwartsLegacy-Win64-Shipping.exe", &[], &["pak"]) == framegen_core::scan::GameEngine::Unreal4,
            "*-Win64-Shipping.exe + .pak -> Unreal 4",
        );
        ck(
            &mut fails,
            eng("SomeGame-Win64-Shipping.exe", &[], &["utoc"]) == framegen_core::scan::GameEngine::Unreal5,
            "看到 .utoc（IoStore）-> Unreal 5",
        );
        ck(
            &mut fails,
            eng("MyGame.exe", &["UnityPlayer.dll", "MyGame_Data"], &[]) == framegen_core::scan::GameEngine::Unity,
            "UnityPlayer.dll -> Unity",
        );
        ck(
            &mut fails,
            eng("cs2.exe", &["engine2.dll", "tier0.dll"], &[]) == framegen_core::scan::GameEngine::Source2,
            "engine2.dll -> Source 2",
        );
        ck(
            &mut fails,
            eng("re4.exe", &["re_chunk_000.pak"], &[]) == framegen_core::scan::GameEngine::ReEngine,
            "re_chunk_*.pak -> RE Engine",
        );
        ck(
            &mut fails,
            eng("SomeIndie.exe", &["data.win"], &[]) == framegen_core::scan::GameEngine::GameMaker,
            "data.win -> GameMaker",
        );
        ck(
            &mut fails,
            eng("Mystery.exe", &["random.dll"], &[]) == framegen_core::scan::GameEngine::Unknown,
            "没有证据就报 Unknown，不猜",
        );
        ck(
            &mut fails,
            eng_ext(&["rpf", "dat"]) == framegen_core::scan::GameEngine::Rage,
            "根目录有 .rpf -> RAGE（GTA / 荒野大镖客）",
        );
        ck(
            &mut fails,
            eng_ext(&["forge"]) == framegen_core::scan::GameEngine::Anvil,
            "根目录有 .forge -> Anvil（刺客信条）",
        );
        ck(
            &mut fails,
            eng_ext(&["cas", "sb", "toc"]) == framegen_core::scan::GameEngine::Frostbite,
            ".cas + .sb + .toc -> Frostbite（战地 / FIFA）",
        );
        ck(
            &mut fails,
            eng_ext(&["cas"]) == framegen_core::scan::GameEngine::Unknown,
            "只有单个 .cas 不算 Frostbite（宁可报未知）",
        );
        ck(
            &mut fails,
            eng_ext(&["ba2"]) == framegen_core::scan::GameEngine::Creation,
            ".ba2 -> Creation Engine（上古卷轴 / 辐射）",
        );
        ck(
            &mut fails,
            eng_ext(&["pak", "dat", "bin"]) == framegen_core::scan::GameEngine::Unknown,
            "通用的 .pak/.dat 不算任何引擎",
        );
    }

    // ---- 路径归一化 / 长路径 / 版本号归一 ----
    println!("\n--- 路径与版本号工具 ---");
    {
        // 92 = 反斜杠，63 = '?'：用字节拼出来，源码里就不必写转义
        let verbatim = String::from_utf8(vec![92u8, 92, 63, 92]).unwrap();
        ck(
            &mut fails,
            framegen_core::scan::path_key(Path::new("D:/Games/X/")) == framegen_core::scan::path_key(Path::new("d:\\games\\x")),
            "同一个目录的不同写法归一化后相同（忽略清单/去重靠它）",
        );
        ck(
            &mut fails,
            framegen_core::util::long_path(Path::new("C:\\short\\x.dll")).as_path() == Path::new("C:\\short\\x.dll"),
            "短路径不加 verbatim 前缀（免得改变别的语义）",
        );
        let deep = format!("C:\\{}\\x.dll", "a".repeat(250));
        ck(
            &mut fails,
            framegen_core::util::long_path(Path::new(&deep))
                .to_string_lossy()
                .starts_with(&verbatim),
            "超长路径自动加 verbatim 前缀（绕开 260 字符上限）",
        );
        ck(
            &mut fails,
            framegen_core::update::same_version_pub("0.3.5", "v0.3.5"),
            "版本号 0.3.5 和 v0.3.5 算同一个版本（镜像回退时要求两家一致）",
        );
    }

    // ---- 导入时的「版本对照」 ----
    // 上游源码包里根目录和 archive/0.2.4/ 各有一份 README，版本号不同：挑错会出现
    // 「装的明明是当前版、界面却说这是 0.2.4」。
    println!("\n--- 导入的版本对照 ---");
    {
        let both = [
            (2u8, Some("0.2.4".to_owned())),
            (0u8, Some("0.3.5".to_owned())),
        ];
        ck(
            &mut fails,
            framegen_core::importer::pick_pack_version(&both).as_deref() == Some("0.3.5"),
            "包里同时有新旧两套 README 时取最新那套的版本",
        );
        ck(
            &mut fails,
            framegen_core::importer::pick_pack_version(&[(2u8, Some("0.2.4".to_owned()))]).as_deref() == Some("0.2.4"),
            "只有老包时取 0.2.4",
        );
        ck(
            &mut fails,
            framegen_core::importer::pick_pack_version(&[(0u8, None), (2u8, Some("0.2.4".to_owned()))]).as_deref()
                == Some("0.2.4"),
            "读不出版本的条目不影响挑选",
        );
        ck(
            &mut fails,
            framegen_core::importer::pick_pack_version(&[]).is_none(),
            "什么都没有时不编一个版本号出来",
        );

        let old = framegen_core::importer::Versions {
            pack: Some("0.3.0".to_owned()),
            local: Some("0.2.4".to_owned()),
            upstream: Some("0.3.5".to_owned()),
            upstream_cached: false,
        };
        ck(&mut fails, old.pack_is_old(), "0.3.0 的包 < 上游 0.3.5：要提示更新");
        ck(
            &mut fails,
            old.report_lines().iter().any(|l| l.contains("比上游旧")),
            "对照表里写明了「比上游旧」",
        );
        ck(
            &mut fails,
            old.report_lines().iter().any(|l| l.contains("0.2.4")),
            "对照表里也写出了本机已装资产版本",
        );

        let same = framegen_core::importer::Versions {
            pack: Some("0.3.5".to_owned()),
            upstream: Some("0.3.5".to_owned()),
            ..Default::default()
        };
        ck(&mut fails, !same.pack_is_old(), "和上游同版本：不提示更新");
        ck(
            &mut fails,
            !same.report_lines().iter().any(|l| l.contains("比上游旧")),
            "同版本时对照表不写「比上游旧」",
        );

        let unknown = framegen_core::importer::Versions::default();
        ck(&mut fails, !unknown.pack_is_old(), "版本都未知时不乱提示");
        ck(&mut fails, !unknown.worth_showing(), "三个号全未知时不占地方");
        ck(
            &mut fails,
            framegen_core::importer::Versions {
                upstream: Some("0.3.5".to_owned()),
                upstream_cached: true,
                ..Default::default()
            }
            .report_lines()
            .iter()
            .any(|l| l.contains("上次检查")),
            "上游版本是缓存来的时候要注明来源",
        );
    }

    if !fails.is_empty() {
        println!("\n  ★ 有 {} 项断言失败", fails.len());
        for f in &fails {
            println!("    - {f}");
        }
    }

    println!("\n===== 自检结束 =====");
    fails.len()
}

fn deploytest() -> usize {
    use std::fs;

    println!("===== 部署 / 备份 / 还原 端到端自测 =====");
    // 用系统临时目录，不写死盘符：CI（GitHub runner）上没有 D: 盘。
    let root = std::env::temp_dir().join("fgm-deploytest");
    let _ = fs::remove_dir_all(&root);
    let src = root.join("src");
    let target = root.join("target");
    if fs::create_dir_all(&src).is_err() || fs::create_dir_all(&target).is_err() {
        println!("[FAIL] 无法创建测试目录");
        return 1;
    }

    let dll_src = src.join("version.dll");
    let ini_src = src.join("dlssg_sm86.ini");
    fs::write(&dll_src, b"FAKE_DLL_PAYLOAD_V1").unwrap();
    fs::write(&ini_src, b"FAKE_INI_PAYLOAD_V1").unwrap();

    // 目标目录里预先放一个用户自己的 INI，它必须被备份并在还原时恢复
    let orig_ini: &[u8] = b"ORIGINAL_INI_FROM_USER";
    fs::write(target.join("dlssg_sm86.ini"), orig_ini).unwrap();

    let mut fails = 0usize;
    macro_rules! check {
        ($cond:expr, $label:expr) => {
            if $cond {
                println!("  [PASS] {}", $label);
            } else {
                println!("  [FAIL] {}", $label);
                fails += 1;
            }
        };
    }

    // ---- 旧代理入口的处置规则（纯逻辑，不需要真 DLL）----
    println!("
-- 旧代理入口怎么处置 --");
    check!(
        framegen_core::deploy::plan_orphan(true, false, true, true, false) == framegen_core::deploy::OrphanAction::Remove,
        "原本空着、我们放进去的旧入口 -> 删掉"
    );
    check!(
        framegen_core::deploy::plan_orphan(true, false, true, true, true)
            == framegen_core::deploy::OrphanAction::RemoveButKeepRecord,
        "原本就有文件的旧入口 -> 删掉但保留备份记录（这次修的就是这条）"
    );
    check!(
        framegen_core::deploy::plan_orphan(true, true, true, true, true) == framegen_core::deploy::OrphanAction::Keep,
        "这次还要用的入口 -> 不碰"
    );
    check!(
        framegen_core::deploy::plan_orphan(true, false, true, false, false) == framegen_core::deploy::OrphanAction::Keep,
        "内容已被用户换过 -> 不碰"
    );
    check!(
        framegen_core::deploy::plan_orphan(false, false, true, true, false) == framegen_core::deploy::OrphanAction::Keep,
        "不是代理入口 -> 不碰"
    );
    check!(
        framegen_core::deploy::plan_orphan(true, false, false, false, true) == framegen_core::deploy::OrphanAction::Keep,
        "文件已经不在了 -> 不碰"
    );

    check!(
        framegen_core::deploy::state_of(&target) == framegen_core::deploy::DeployState::NotDeployed,
        "初始状态 = 未部署"
    );

    let dep_files = [
        framegen_core::deploy::DeployFile::new("version.dll", &dll_src),
        framegen_core::deploy::DeployFile::new(framegen_core::deploy::INI_NAME, &ini_src),
    ];
    match framegen_core::deploy::deploy(&target, "version.dll", &dep_files, &[]) {
        Ok(_) => {
            check!(
                fs::read(target.join("version.dll")).ok().as_deref() == Some(&b"FAKE_DLL_PAYLOAD_V1"[..]),
                "部署后代理 DLL 内容正确"
            );
            check!(
                fs::read(target.join("dlssg_sm86.ini")).ok().as_deref() == Some(&b"FAKE_INI_PAYLOAD_V1"[..]),
                "部署后 INI 内容正确"
            );
            check!(
                matches!(framegen_core::deploy::state_of(&target), framegen_core::deploy::DeployState::Deployed { .. }),
                "部署后状态 = 已部署"
            );
            check!(
                !target.join(".version.dll.tmp").exists(),
                "没有残留临时文件"
            );

            match framegen_core::deploy::restore(&target) {
                Ok(_) => {
                    check!(!target.join("version.dll").exists(), "还原后代理 DLL 已移除");
                    check!(
                        fs::read(target.join("dlssg_sm86.ini")).ok().as_deref() == Some(orig_ini),
                        "还原后 INI 恢复为原内容"
                    );
                    check!(
                        framegen_core::deploy::state_of(&target) == framegen_core::deploy::DeployState::NotDeployed,
                        "还原后状态 = 未部署"
                    );
                }
                Err(e) => {
                    println!("  [FAIL] 还原报错: {e}");
                    fails += 1;
                }
            }
        }
        Err(e) => {
            println!("  [FAIL] 部署报错: {e}");
            fails += 1;
        }
    }

    // 重部署：上游更新之后用户会再点一次「部署」。
    // 关键：这时不能把我们上次部署进去的文件当成「游戏原文件」重新备份，
    // 否则真正的原件会被覆盖掉，「还原」就还原不回游戏原样了。
    println!("
-- 重部署（模拟上游更新后再部署一次）--");
    // 这里只部署 INI、不带 DLL：deploy 会检查目标里同名文件的签名，测试用的假字节
    // 没有本项目签名，第二次部署会被正当地拒掉。真实的已签名 DLL（15 MB）没法在自测里
    // 造出来，所以这一段专门验证「原始备份不被自己的旧版本覆盖」。
    let t4 = root.join("redeploy");
    let _ = fs::create_dir_all(&t4);
    fs::write(t4.join(framegen_core::deploy::INI_NAME), orig_ini).unwrap();
    let ini_only = [framegen_core::deploy::DeployFile::new(framegen_core::deploy::INI_NAME, &ini_src)];
    fs::write(&ini_src, b"FAKE_INI_PAYLOAD_V1").unwrap();
    match framegen_core::deploy::deploy(&t4, "version.dll", &ini_only, &[]) {
        Ok(_) => {
            // 上游更新了：源文件换成 V2，注意中间**没有**先还原
            fs::write(&ini_src, b"FAKE_INI_PAYLOAD_V2").unwrap();
            match framegen_core::deploy::deploy(&t4, "version.dll", &ini_only, &[]) {
                Ok(_) => {
                    check!(
                        fs::read(t4.join(framegen_core::deploy::INI_NAME)).ok().as_deref()
                            == Some(&b"FAKE_INI_PAYLOAD_V2"[..]),
                        "重部署后 INI 已是新内容"
                    );
                    let orig_kept = framegen_core::deploy::load_manifest(&t4)
                        .and_then(|m| {
                            m.files.into_iter().find(|e| e.rel_path == framegen_core::deploy::INI_NAME)
                        })
                        .and_then(|e| e.original_sha256)
                        .map(|h| h == framegen_core::util::sha256_hex(orig_ini))
                        .unwrap_or(false);
                    check!(
                        orig_kept,
                        "重部署后备份里仍是最初的用户原始 INI（没被自己的旧版本覆盖）"
                    );
                    match framegen_core::deploy::restore(&t4) {
                        Ok(_) => check!(
                            fs::read(t4.join(framegen_core::deploy::INI_NAME)).ok().as_deref() == Some(orig_ini),
                            "重部署后仍能还原出最初的用户原始 INI"
                        ),
                        Err(e) => {
                            println!("  [FAIL] 重部署后还原报错: {e}");
                            fails += 1;
                        }
                    }
                }
                Err(e) => {
                    println!("  [FAIL] 重部署报错: {e}");
                    fails += 1;
                }
            }
        }
        Err(e) => {
            println!("  [FAIL] 首次部署报错: {e}");
            fails += 1;
        }
    }

    // 换代理入口：旧入口不能留在目录里变成孤儿
    println!("
-- 换代理入口 --");
    let t5 = root.join("switch");
    let _ = fs::create_dir_all(&t5);
    let winmm_src = src.join("winmm.dll");
    fs::write(&winmm_src, b"FAKE_WINMM_V1").unwrap();
    let only_version = [framegen_core::deploy::DeployFile::new("version.dll", &dll_src)];
    let only_winmm = [framegen_core::deploy::DeployFile::new("winmm.dll", &winmm_src)];
    match framegen_core::deploy::deploy(&t5, "version.dll", &only_version, &[]) {
        Ok(_) => {
            check!(t5.join("version.dll").is_file(), "先用 version.dll 部署成功");
            match framegen_core::deploy::deploy(&t5, "winmm.dll", &only_winmm, &[]) {
                Ok(_) => {
                    check!(t5.join("winmm.dll").is_file(), "换入口后 winmm.dll 已部署");
                    check!(
                        !t5.join("version.dll").exists(),
                        "换入口后旧的 version.dll 已被清掉（不再有两个代理并存）"
                    );
                    match framegen_core::deploy::restore(&t5) {
                        Ok(_) => check!(
                            !t5.join("winmm.dll").exists(),
                            "换入口后仍能正常还原"
                        ),
                        Err(e) => {
                            println!("  [FAIL] 换入口后还原报错: {e}");
                            fails += 1;
                        }
                    }
                }
                Err(e) => {
                    println!("  [FAIL] 换入口部署报错: {e}");
                    fails += 1;
                }
            }
        }
        Err(e) => {
            println!("  [FAIL] 首次部署报错: {e}");
            fails += 1;
        }
    }

    // 冲突：目录里已有第三方的 version.dll
    println!("
-- 入口冲突 --");
    let t2 = root.join("conflict");
    let _ = fs::create_dir_all(&t2);
    fs::write(t2.join("version.dll"), b"SOME_OTHER_MOD").unwrap();
    check!(
        framegen_core::deploy::deploy(&t2, "version.dll", &dep_files, &[]).is_err(),
        "已存在第三方 version.dll 时拒绝部署"
    );
    check!(
        fs::read(t2.join("version.dll")).ok().as_deref() == Some(&b"SOME_OTHER_MOD"[..]),
        "被拒绝后第三方文件未被改动"
    );
    let dep_files_alt = [
        framegen_core::deploy::DeployFile::new("winmm.dll", &dll_src),
        framegen_core::deploy::DeployFile::new(framegen_core::deploy::INI_NAME, &ini_src),
    ];
    check!(
        framegen_core::deploy::deploy(&t2, "winmm.dll", &dep_files_alt, &[]).is_ok(),
        "改用 winmm.dll 替代入口可以部署"
    );
    check!(t2.join("winmm.dll").is_file(), "替代入口文件已写入");
    let _ = framegen_core::deploy::restore(&t2);
    check!(!t2.join("winmm.dll").exists(), "替代入口已还原");

    // 反作弊闸门
    println!("
-- 反作弊闸门 --");
    let t3 = root.join("ac");
    let _ = fs::create_dir_all(t3.join("EasyAntiCheat"));
    // 光有一个同名目录不算内核级：那可能只是卸载残留或启动器。
    // （以前这里按内核级算，于是只剩一个用户态组件的游戏也会弹「封号风险」，
    //   和同一份文件里服务表的判定自相矛盾。）
    check!(
        !framegen_core::anticheat::scan_deep(&t3).is_blocked(),
        "只有 EasyAntiCheat 目录、没有驱动文件时算用户态，不阻止"
    );
    fs::write(t3.join("EasyAntiCheat").join("EasyAntiCheat.sys"), b"x").unwrap();
    check!(
        framegen_core::anticheat::scan_deep(&t3).is_blocked(),
        "目录里有 EasyAntiCheat.sys 才判内核级并阻止"
    );
    // scan_game_dir() 会顺带检查最多 4 级**祖先**目录（EAC / BattlEye 通常装在游戏
    // 根目录，而部署目标可能是 ...\Binaries\Win64）。所以这个「干净目录」必须放得足够深，
    // 让那 4 级祖先全都落在本次测试自己的目录里 —— 否则会一路扫到系统临时目录的祖先，
    // 那不受测试控制，换台机器结论就可能反过来。
    let t4 = root.join("clean").join("a").join("b").join("c").join("d");
    let _ = fs::create_dir_all(&t4);
    check!(
        !framegen_core::anticheat::scan_deep(&t4).is_blocked(),
        "普通空目录不阻止部署"
    );

    // 反向也要成立：标记在**祖先**目录里时同样要挡住（这正是 scan_game_dir 扫祖先的理由）
    let t5 = root.join("anc").join("Binaries").join("Win64");
    let _ = fs::create_dir_all(&t5);
    let _ = fs::create_dir_all(root.join("anc").join("EasyAntiCheat"));
    // 用 EAC 的真实布局：驱动文件就在**游戏根**目录里（部署目标在 Binaries\Win64 下面），
    // 祖先扫描必须能靠它判出内核级。
    fs::write(root.join("anc").join("EasyAntiCheat.sys"), b"x").unwrap();
    check!(
        framegen_core::anticheat::scan_deep(&t5).is_blocked(),
        "祖先目录里有驱动文件时同样阻止部署"
    );

    // ---- 已装过本项目：允许覆盖（这是「判断用户是否手动装过」的核心行为）----
    println!("
-- 游戏目录已有本项目文件 --");
    let ours_src: Option<PathBuf> = [
        r"D:\Epic Game\HogwartsLegacy\Phoenix\Binaries\Win64\version.dll",
        r"E:\SteamLibrary\steamapps\common\PUBG\TslGame\Binaries\Win64\version.dll",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file());

    match ours_src {
        None => {
            println!("  （本机没找到已安装的本项目文件，跳过这段）");
        }
        Some(real) => {
            check!(
                framegen_core::scan::identify_dll(&real) == framegen_core::scan::FileIdentity::ThisProject,
                "识别出真实的本项目文件（DLSSG Native Project 签名）"
            );
            let t5 = root.join("ours");
            let _ = fs::create_dir_all(&t5);
            fs::copy(&real, t5.join("version.dll")).unwrap();
            check!(
                framegen_core::scan::identify_dll(&t5.join("version.dll")).is_ours(),
                "复制过去后仍判定为本项目文件"
            );
            // 先确认它是「可覆盖」的，再实际部署一次
            let advice = framegen_core::scan::advise_proxy(&t5);
            check!(
                advice.occupied.is_empty() && !advice.own_existing.is_empty(),
                "已有本项目文件时不算被占用，而是归入 own_existing"
            );
            let r = framegen_core::deploy::deploy(&t5, "version.dll", &dep_files, &[]);
            check!(r.is_ok(), "目标已有本项目文件时允许覆盖（不再误拒）");
            let recorded_as_existing = framegen_core::deploy::load_manifest(&t5)
                .map(|m| {
                    m.files
                        .iter()
                        .any(|e| e.rel_path == "version.dll" && e.existed_before)
                })
                .unwrap_or(false);
            check!(recorded_as_existing, "这个位置被记成「原本就有文件」");

            // 换成别的入口再部署：不处理这一分支的话，两个代理会并存，
            // 而且原件再也还原不回来。
            let winmm_switch = src.join("winmm_switch.dll");
            fs::write(&winmm_switch, b"FAKE_WINMM_SWITCH").unwrap();
            let switch = [framegen_core::deploy::DeployFile::new("winmm.dll", &winmm_switch)];
            match framegen_core::deploy::deploy(&t5, "winmm.dll", &switch, &[]) {
                Ok(_) => {
                    check!(
                        !t5.join("version.dll").exists(),
                        "换入口后旧的 version.dll 已从游戏目录挪走（不再两个代理并存）"
                    );
                    check!(t5.join("winmm.dll").is_file(), "新入口 winmm.dll 已部署");
                    let backup_kept = framegen_core::deploy::load_manifest(&t5)
                        .map(|m| {
                            m.files.iter().any(|e| {
                                e.rel_path == "version.dll"
                                    && e.existed_before
                                    && e.backup_name.is_some()
                            })
                        })
                        .unwrap_or(false);
                    check!(backup_kept, "旧入口的备份记录保留下来了（还原还管得着）");
                    match framegen_core::deploy::restore(&t5) {
                        Ok(_) => {
                            check!(
                                framegen_core::scan::identify_dll(&t5.join("version.dll")).is_ours(),
                                "还原后原本那份本项目文件原样回来了"
                            );
                            check!(
                                !t5.join("winmm.dll").exists(),
                                "还原后本次部署的 winmm.dll 也移除了"
                            );
                        }
                        Err(e) => {
                            println!("  [FAIL] 换入口后还原报错: {e}");
                            fails += 1;
                        }
                    }
                }
                Err(e) => {
                    println!("  [FAIL] 换入口部署报错: {e}");
                    fails += 1;
                }
            }
        }
    }

    // ---- 目录里有「本项目的另一个代理入口」，但不是本工具装的 ----
    println!("
-- 目录里有另一个本项目代理（手动装的）--");
    let real_ours: Option<PathBuf> = [
        r"D:\Epic Game\HogwartsLegacy\Phoenix\Binaries\Win64\version.dll",
        r"E:\SteamLibrary\steamapps\common\PUBG\TslGame\Binaries\Win64\version.dll",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file());
    match real_ours {
        None => println!("  （本机没有真实的本项目 DLL，跳过这段）"),
        Some(real) => {
            let t6 = root.join("extra-proxy");
            let _ = fs::create_dir_all(&t6);
            // 模拟「用户手动装了 dinput8.dll」，而待部署的是 version.dll
            fs::copy(&real, t6.join("dinput8.dll")).unwrap();
            check!(
                framegen_core::deploy::find_extra_own_proxies(&t6, "version.dll") == vec!["dinput8.dll".to_owned()],
                "认出了目录里另一个本项目代理（该弹窗问用户）"
            );
            check!(
                framegen_core::deploy::find_extra_own_proxies(&t6, "dinput8.dll").is_empty(),
                "和本次要用的入口同名时不算多余"
            );

            let ver2 = src.join("ver_extra.dll");
            let ini2 = src.join("ini_extra.ini");
            fs::write(&ver2, b"FAKE_VER_EXTRA").unwrap();
            fs::write(&ini2, b"FAKE_INI_EXTRA").unwrap();
            let payload2 = [
                framegen_core::deploy::DeployFile::new("version.dll", &ver2),
                framegen_core::deploy::DeployFile::new(framegen_core::deploy::INI_NAME, &ini2),
            ];
            let r = framegen_core::deploy::deploy(&t6, "version.dll", &payload2, &["dinput8.dll".to_owned()]);
            check!(r.is_ok(), "用户选「移除并继续」后部署成功");
            check!(
                !t6.join("dinput8.dll").exists(),
                "多余的那个本项目代理已移出游戏目录（只剩一个代理）"
            );
            let recorded = framegen_core::deploy::load_manifest(&t6)
                .map(|m| {
                    m.files.iter().any(|e| {
                        e.rel_path == "dinput8.dll" && e.existed_before && e.backup_name.is_some()
                    })
                })
                .unwrap_or(false);
            check!(recorded, "移除的那份原件备份记录留下来了");

            // 再部署一次（用户往往还会再点一次），记录不能被丢掉
            let ini_only2 = [framegen_core::deploy::DeployFile::new(framegen_core::deploy::INI_NAME, &ini2)];
            let r2 = framegen_core::deploy::deploy(&t6, "version.dll", &ini_only2, &[]);
            check!(r2.is_ok(), "再部署一次也成功");
            let recorded2 = framegen_core::deploy::load_manifest(&t6)
                .map(|m| m.files.iter().any(|e| e.rel_path == "dinput8.dll"))
                .unwrap_or(false);
            check!(recorded2, "再部署之后那条记录还在（还原仍管得着）");

            match framegen_core::deploy::restore(&t6) {
                Ok(_) => {
                    check!(
                        framegen_core::scan::identify_dll(&t6.join("dinput8.dll")).is_ours(),
                        "还原后手动装的那份 dinput8.dll 回来了"
                    );
                    check!(
                        !t6.join("version.dll").exists(),
                        "还原后本次部署的 version.dll 已移除"
                    );
                }
                Err(e) => {
                    println!("  [FAIL] 还原报错: {e}");
                    fails += 1;
                }
            }
        }
    }

    let _ = fs::remove_dir_all(&root);
    println!("
===== 结果: {fails} 项失败 =====");
    fails
}

// ------------------------------------------------------------------ UI

// ---------------------------------------------------------------- cargo test 入口
// 用系统临时目录当数据根：自测会写备份/日志，绝不能落到用户真实的数据目录里。
fn hermetic_data_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("fgm-selftest-data").join(name);
    let _ = std::fs::remove_dir_all(&d);
    let _ = std::fs::create_dir_all(&d);
    d
}

#[test]
fn sourcetest_passes() {
    std::env::set_var("FGM_DATA_DIR", hermetic_data_dir("source"));
    let n = sourcetest();
    assert_eq!(n, 0, "sourcetest 有 {n} 项失败（明细见上面的 [FAIL]）");
}

#[test]
fn selftest_passes() {
    std::env::set_var("FGM_DATA_DIR", hermetic_data_dir("self"));
    let n = selftest();
    assert_eq!(n, 0, "selftest 有 {n} 项失败（明细见上面的 [FAIL]）");
}

#[test]
fn deploytest_passes() {
    std::env::set_var("FGM_DATA_DIR", hermetic_data_dir("deploy"));
    let n = deploytest();
    assert_eq!(n, 0, "deploytest 有 {n} 项失败");
}
