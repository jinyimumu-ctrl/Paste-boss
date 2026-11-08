//! 设置面板的后端能力：版本号 + 开机自启。
//!
//! 开机自启不往「启动」文件夹里塞快捷方式，而是写注册表的「登录时运行」键
//! （`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`）：
//! - 只动当前用户，不需要管理员权限；
//! - 关掉开关就是把值删掉，注册表里不留垃圾；
//! - 程序本来就是隐藏窗口启动的，所以开机后只是安静待在托盘里，按 Alt+V 才弹出来。
//!
//! 和 `hotkey.rs` 一样直接声明 Win32 API（advapi32），不额外引入插件依赖。

use std::path::{Path, PathBuf};

use serde::Serialize;

/// 在 Run 键下用的值名：一个用户只可能有一份自启登记
const AUTOSTART_VALUE: &str = "paste-boss";

/// 注册表 Run 键的路径（当前用户）
#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 设置面板要显示的程序状态
#[derive(Serialize)]
pub struct AppStatus {
    /// 程序版本号，来自 Cargo.toml（安装包与程序本体同源；界面不再显示）
    pub version: String,
    /// 当前是否已经登记了开机自启
    pub autostart: bool,
    /// 当前平台是否支持开机自启（只有 Windows 支持）
    pub autostart_supported: bool,
    /// 自启时会被拉起的程序路径（方便用户确认自启的是哪一份 exe）
    pub exe_path: String,
}

/// 读取当前状态（版本号 + 自启开关）
pub fn app_status() -> AppStatus {
    AppStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        autostart: win::query().is_some(),
        autostart_supported: cfg!(target_os = "windows"),
        exe_path: current_exe_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    }
}

/// 打开 / 关闭开机自启，返回操作后的最新状态
pub fn set_autostart(enabled: bool) -> Result<AppStatus, String> {
    if enabled {
        win::enable()?;
    } else {
        win::disable()?;
    }
    Ok(app_status())
}

/// 启动时自检：用户之前打开过自启、但程序被挪到了别的目录时，
/// 把注册表里的路径更新成现在这个 exe，免得下次开机去拉起一个已经不存在的旧路径。
pub fn heal_autostart() {
    let Some(registered) = win::query() else {
        return;
    };
    let Ok(exe) = current_exe_path() else {
        return;
    };
    let expected = autostart_command(&exe);
    if registered == expected {
        eprintln!("[settings] 开机自启已登记：{registered}");
        return;
    }
    match win::enable() {
        Ok(()) => eprintln!("[settings] 程序路径变了，已把开机自启更新为 {expected}"),
        Err(err) => eprintln!("[settings] 更新开机自启失败: {err}"),
    }
}

/// 自启时写进注册表的命令行。
/// 一定要带引号：程序放在 `Program Files` 这类有空格的路径里也能正常启动。
fn autostart_command(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// 当前 exe 的完整路径
fn current_exe_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|err| format!("取程序路径失败: {err}"))
}

// ---------------------------------------------------------------------------
// Windows：读写注册表 Run 键
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};

    use super::{AUTOSTART_VALUE, RUN_KEY, autostart_command, current_exe_path};

    /// 预定义句柄 HKEY_CURRENT_USER。
    /// C 里是 `0x80000001` 的有符号扩展，所以 64 位下要写成 `0xFFFFFFFF80000001`。
    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    /// 打开子键时只要读值的权限
    const KEY_QUERY_VALUE: u32 = 0x0001;
    /// 打开 / 创建子键时要写值的权限
    const KEY_SET_VALUE: u32 = 0x0002;
    /// 值类型：以 NUL 结尾的宽字符串
    const REG_SZ: u32 = 1;
    /// 成功
    const ERROR_SUCCESS: i32 = 0;
    /// 要删的值本来就不存在
    const ERROR_FILE_NOT_FOUND: i32 = 2;
    /// 读值时缓冲区不够大
    const ERROR_MORE_DATA: i32 = 234;
    /// 一次读取的缓冲区大小（宽字符个数）：自启命令只是「引号 + 路径」，远远够用
    const QUERY_BUFFER_UNITS: usize = 512;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegCreateKeyExW(
            key: *mut c_void,
            sub_key: *const u16,
            reserved: u32,
            class: *const u16,
            options: u32,
            sam: u32,
            security: *mut c_void,
            result: *mut *mut c_void,
            disposition: *mut u32,
        ) -> i32;
        fn RegOpenKeyExW(
            key: *mut c_void,
            sub_key: *const u16,
            options: u32,
            sam: u32,
            result: *mut *mut c_void,
        ) -> i32;
        fn RegSetValueExW(
            key: *mut c_void,
            value_name: *const u16,
            reserved: u32,
            kind: u32,
            data: *const u8,
            bytes: u32,
        ) -> i32;
        fn RegDeleteValueW(key: *mut c_void, value_name: *const u16) -> i32;
        fn RegQueryValueExW(
            key: *mut c_void,
            value_name: *const u16,
            reserved: *mut u32,
            kind: *mut u32,
            data: *mut u8,
            bytes: *mut u32,
        ) -> i32;
        fn RegCloseKey(key: *mut c_void) -> i32;
    }

    /// Rust 字符串 → 以 NUL 结尾的宽字符串（Win32 的 LPCWSTR）
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 打开（必要时创建）自启所在的 Run 键。返回的句柄用完必须 `RegCloseKey`。
    fn open_run_key() -> Result<*mut c_void, String> {
        let sub_key = wide(RUN_KEY);
        let mut key: *mut c_void = null_mut();
        let mut disposition = 0u32;

        // SAFETY: sub_key 以 NUL 结尾；key / disposition 都是本函数的栈变量，由系统填写
        let code = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER as *mut c_void,
                sub_key.as_ptr(),
                0,
                null(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                null_mut(),
                &mut key,
                &mut disposition,
            )
        };

        if code != ERROR_SUCCESS {
            return Err(format!("打开注册表 Run 键失败（错误码 {code}）"));
        }
        Ok(key)
    }

    /// 打开已存在的 Run 键（只读，不会顺带创建）。返回的句柄用完必须 `RegCloseKey`。
    fn open_run_key_readonly() -> Result<*mut c_void, String> {
        let sub_key = wide(RUN_KEY);
        let mut key: *mut c_void = null_mut();

        // SAFETY: sub_key 以 NUL 结尾；key 是本函数的栈变量，由系统填写
        let code = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER as *mut c_void,
                sub_key.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };

        if code != ERROR_SUCCESS {
            return Err(format!("打开注册表 Run 键失败（错误码 {code}）"));
        }
        Ok(key)
    }

    /// 登记开机自启（已登记则覆盖成当前 exe 路径）
    pub fn enable() -> Result<(), String> {
        let exe = current_exe_path()?;
        let command = autostart_command(&exe);
        let command_wide = wide(&command);
        let value_name = wide(AUTOSTART_VALUE);

        let key = open_run_key()?;
        // SAFETY: 句柄来自 RegCreateKeyExW；字符串都以 NUL 结尾，
        // 字节数 = UTF-16 单元数 × 2，正好包含结尾的 NUL
        let code = unsafe {
            RegSetValueExW(
                key,
                value_name.as_ptr(),
                0,
                REG_SZ,
                command_wide.as_ptr() as *const u8,
                (command_wide.len() * 2) as u32,
            )
        };
        // SAFETY: 句柄用完必须关闭
        unsafe {
            RegCloseKey(key);
        }

        if code != ERROR_SUCCESS {
            return Err(format!("写入开机自启失败（错误码 {code}）"));
        }
        eprintln!("[settings] 已开启开机自启：{command}");
        Ok(())
    }

    /// 取消开机自启（本来就没开也算成功）
    pub fn disable() -> Result<(), String> {
        let value_name = wide(AUTOSTART_VALUE);
        let key = open_run_key()?;
        // SAFETY: 句柄来自 RegCreateKeyExW；value_name 以 NUL 结尾
        let code = unsafe { RegDeleteValueW(key, value_name.as_ptr()) };
        // SAFETY: 句柄用完必须关闭
        unsafe {
            RegCloseKey(key);
        }

        if code != ERROR_SUCCESS && code != ERROR_FILE_NOT_FOUND {
            return Err(format!("关闭开机自启失败（错误码 {code}）"));
        }
        eprintln!("[settings] 已关闭开机自启");
        Ok(())
    }

    /// 读取当前登记的自启命令；没有登记（或读不到）时返回 None
    pub fn query() -> Option<String> {
        let value_name = wide(AUTOSTART_VALUE);
        let key = open_run_key_readonly().ok()?;

        let mut buffer = vec![0u16; QUERY_BUFFER_UNITS];
        let mut bytes = (buffer.len() * 2) as u32;
        let mut kind = 0u32;
        // SAFETY: 缓冲区长度与传入的字节数一致，RegQueryValueExW 最多写这么多
        let code = unsafe {
            RegQueryValueExW(
                key,
                value_name.as_ptr(),
                null_mut(),
                &mut kind,
                buffer.as_mut_ptr() as *mut u8,
                &mut bytes,
            )
        };
        // SAFETY: 句柄用完必须关闭
        unsafe {
            RegCloseKey(key);
        }

        if code != ERROR_SUCCESS {
            // 缓冲区不足时拿不到完整命令，按「没登记」处理（正常路径远远用不到 1KB）
            if code == ERROR_MORE_DATA {
                eprintln!("[settings] 自启命令过长，无法读取");
            }
            return None;
        }
        if kind != REG_SZ {
            return None;
        }

        let units = (bytes as usize / 2).min(buffer.len());
        let text = String::from_utf16_lossy(&buffer[..units]);
        let text = text.trim_end_matches('\0').trim().to_string();
        if text.is_empty() { None } else { Some(text) }
    }
}

// ---------------------------------------------------------------------------
// 其它平台：没有「登录时运行」这种机制，开关直接报不支持
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
mod win {
    pub fn enable() -> Result<(), String> {
        Err("当前平台暂不支持开机自启（仅 Windows）".to_string())
    }

    pub fn disable() -> Result<(), String> {
        Err("当前平台暂不支持开机自启（仅 Windows）".to_string())
    }

    pub fn query() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_command_quotes_path() {
        // 路径里有空格时必须加引号，否则 Windows 会把 "C:\Program" 当成程序名
        let command = autostart_command(Path::new(r"C:\Program Files\paste-boss\paste-boss.exe"));
        assert_eq!(command, "\"C:\\Program Files\\paste-boss\\paste-boss.exe\"");
    }

    #[test]
    fn version_matches_cargo_manifest() {
        // 界面显示的版本号必须就是 Cargo.toml 里的版本，不能另写一份
        let status = app_status();
        assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
        assert!(!status.version.is_empty());
    }

    /// 手动验证（默认不跑）：真正写一次注册表再删掉。
    /// 运行：`cargo test --lib autostart_registry_roundtrip -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn autostart_registry_roundtrip() {
        // 先确保是关闭状态
        win::disable().expect("关闭自启");
        assert!(win::query().is_none(), "关闭后不应再读到自启命令");

        win::enable().expect("开启自启");
        let registered = win::query();
        // 先清理再断言：断言失败也不会把自启项留在注册表里
        win::disable().expect("再次关闭自启");

        let expected = autostart_command(&current_exe_path().expect("取当前 exe"));
        assert_eq!(registered.as_deref(), Some(expected.as_str()));
        assert!(win::query().is_none(), "关闭后不应再读到自启命令");
    }
}
