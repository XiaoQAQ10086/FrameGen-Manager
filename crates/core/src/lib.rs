//! FrameGen Manager 的业务核心（无 UI 依赖）。
//!
//! 迁移进度见 `dev-only/MIGRATION-TAURI.md`：B1.2 已完成 util 的搬入。

pub mod util;
pub mod log;
pub mod anticheat;
pub mod gpu;
pub mod verify;
pub mod update;
pub mod scan;
pub mod deploy;
pub mod dlss5;
pub mod importer;
pub mod icon;
