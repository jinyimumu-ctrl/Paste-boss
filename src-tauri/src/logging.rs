//! 日志落盘：把程序运行时的 `eprintln!` 全部写进文件，出问题时才有线索。
//!
//! 发布版是 GUI 子系统（`main.rs` 里的 `windows_subsystem = "windows"`），没有控制台，
//! `eprintln!` 写向一个不存在的 stderr —— 用户报「闪退 / 按 Alt+V 没反应」时，
//! 我们手上一个字的现场都没有。
//!
//! 这里的做法不侵入业务代码：启动时把进程的 stdout / stderr 句柄换成日志文件，
//! 于是现有代码里所有 `eprintln!` 原样落盘，一行调用都不用改。
//! 开发模式下终端本来就能看到这些输出，就不抢句柄了。
//!
//! 日志位置：`%APPDATA%\<identifier>\logs\paste-boss.log`（超过 1MB 滚动成 `.log.1`）。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

/// 单个日志文件的大小上限，超过就滚动
const MAX_LOG_BYTES: u64 = 1024 * 1024;
/// 日志子目录名
const LOG_DIR: &str = "logs";
/// 当前日志文件名
const LOG_FILE: &str = "paste-boss.log";
/// 上一份日志文件名（只保留一份旧的，避免无限增长）
const PREVIOUS_LOG_FILE: &str = "paste-boss.log.1";

/// 日志目录（`%APPDATA%\<identifier>\logs`），拿不到应用数据目录时返回 None
pub fn log_dir(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok().map(|dir| dir.join(LOG_DIR))
}

/// 当前日志文件路径（`...\logs\paste-boss.log`）。
/// 设置面板的「日志」按钮用它定位文件，所以不把文件名常量暴露出去。
pub fn log_file(app: &AppHandle) -> Option<PathBuf> {
    log_dir(app).map(|dir| dir.join(LOG_FILE))
}

/// 初始化日志：滚动旧文件、写下启动横幅、接管 stderr / stdout、装 panic 钩子。
///
/// 任何一步失败都只影响「有没有日志」，绝不影响程序本身能不能跑。
pub fn init(app: &AppHandle) {
    let Some(dir) = log_dir(app) else {
        eprintln!("[logging] 取应用数据目录失败，本次没有日志文件");
        return;
    };
    if let Err(err) = fs::create_dir_all(&dir) {
        eprintln!("[logging] 创建日志目录失败（{}）: {err}", dir.display());
        return;
    }

    let path = dir.join(LOG_FILE);
    // 先滚动上一份（改名为 .log.1）再打开当前文件，避免单文件无限增长
    rotate(&path);

    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("[logging] 打开日志文件失败（{}）: {err}", path.display());
            return;
        }
    };

    let mode = if cfg!(debug_assertions) {
        "开发模式，日志同时输出到终端"
    } else {
        "发布模式，日志只写入本文件"
    };
    // 每行日志都没有时间戳（eprintln 的原文就是这样），
    // 所以每份日志开头记一条时间，配合「文件修改时间」推算问题发生的时段。
    let _ = writeln!(
        file,
        "===== {} Paste-boss v{} 启动（{mode}）=====",
        timestamp(),
        env!("CARGO_PKG_VERSION")
    );

    install_panic_hook();

    // 发布版没有控制台：把 stdout / stderr 都接到日志文件上，之后所有 eprintln! 自动落盘
    #[cfg(not(debug_assertions))]
    redirect_stdio(file);
}

/// 日志文件超过上限就滚动：当前文件改名为 `.log.1`，旧的 `.log.1` 直接丢弃
fn rotate(path: &Path) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() < MAX_LOG_BYTES {
        return;
    }
    let previous = path.with_file_name(PREVIOUS_LOG_FILE);
    let _ = fs::remove_file(&previous);
    if let Err(err) = fs::rename(path, &previous) {
        eprintln!("[logging] 滚动日志失败: {err}");
    }
}

/// panic 钩子：把崩溃现场写进日志。
///
/// 默认的 panic 输出只会在终端打一段，发布版没有终端 —— 不装这个钩子的话，
/// 用户那边看到的就只是「程序突然没了」。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("[panic] {info}");
        // 回溯需要调试符号，发布版做了 strip，只能在开发模式下参考
        if cfg!(debug_assertions) {
            eprintln!("[panic] 回溯:\n{}", std::backtrace::Backtrace::force_capture());
        }
    }));
}

/// 把进程的 stdout / stderr 换成日志文件句柄
///
/// 只在发布版编译：开发模式下终端里本来就能看到 `eprintln!` 的输出，
/// 抢走句柄反而会让调试时看不到日志。
#[cfg(all(target_os = "windows", not(debug_assertions)))]
fn redirect_stdio(file: std::fs::File) {
    use std::ffi::c_void;
    use std::os::windows::io::IntoRawHandle;

    /// `GetStdHandle` / `SetStdHandle` 用的句柄类型
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetStdHandle(kind: u32, handle: *mut c_void) -> i32;
    }

    // 句柄交给系统之后不能再关闭，否则后续日志会写到已失效的句柄上；
    // 进程退出时由系统回收，这里的「泄漏」是刻意的。
    let handle = file.into_raw_handle() as *mut c_void;

    // SAFETY: handle 来自一个仍然打开的文件句柄，且进程生命周期内不会关闭
    unsafe {
        if SetStdHandle(STD_ERROR_HANDLE, handle) == 0 {
            // 连 stderr 都接不上，后面的日志只能放弃
            return;
        }
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
    }
}

/// 本地时间戳，例如 `2026-11-07 19:42:03`
#[cfg(target_os = "windows")]
fn timestamp() -> String {
    /// Win32 的 SYSTEMTIME（字段顺序与 C 结构体一致）
    #[repr(C)]
    #[derive(Default)]
    struct LocalSystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLocalTime(time: *mut LocalSystemTime);
    }

    let mut now = LocalSystemTime::default();
    // SAFETY: GetLocalTime 只会写这一块栈上的结构体
    unsafe { GetLocalTime(&mut now) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        now.year, now.month, now.day, now.hour, now.minute, now.second
    )
}

/// 非 Windows：没有 GetLocalTime，用 UTC 时间凑合（只用于日志开头那一行）
#[cfg(not(target_os = "windows"))]
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();

    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
        seconds % 60
    )
}

/// 把「1970-01-01 起的天数」换算成公历年月日
/// （Howard Hinnant 的 `civil_from_days` 算法，与时区无关）
#[cfg(not(target_os = "windows"))]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era as i64 + era * 400;
    (if month <= 2 { year + 1 } else { year }, month as u32, day as u32)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_readable() {
        let stamp = timestamp();
        // 形如 `2026-11-07 19:42:03`：长度固定、中间是空格和冒号
        assert!(stamp.len() >= 19, "时间戳长度不对: {stamp}");
        assert_eq!(&stamp[4..5], "-");
        assert_eq!(&stamp[7..8], "-");
        assert_eq!(&stamp[10..11], " ");
        assert_eq!(&stamp[13..14], ":");
    }
}
