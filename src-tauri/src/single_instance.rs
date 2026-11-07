//! 单实例：第二次双击 exe 时不再开第二个进程，而是把已经在跑的那个面板呼出来。
//!
//! 为什么必须做：
//! - Alt+V 是进程级的全局快捷键，多开一份就会互抢，用户看到的现象就是「按了没反应」；
//! - 托盘里会出现两个一模一样的图标；
//! - 两份程序各自记录剪贴板，历史会打架，磁盘上还会出现两条写同一份 history.json 的线程。
//!
//! 实现只用 kernel32 的两个命名对象（和 `settings.rs`、`hotkey.rs` 一样不引插件）：
//! - 命名互斥体 `Local\Paste-boss.SingleInstance`：第二份创建时拿到 `ERROR_ALREADY_EXISTS`；
//! - 命名事件 `Local\Paste-boss.Activate`：第二份 `SetEvent`，第一份有线程在等它，
//!   收到就把面板显示出来，然后第二份自己安静退出。

/// 单实例互斥体名。`Local\` 前缀表示只在当前登录会话内有效，多用户同时登录互不干扰
const MUTEX_NAME: &str = r"Local\Paste-boss.SingleInstance";
/// 唤起已有实例的命名事件
const EVENT_NAME: &str = r"Local\Paste-boss.Activate";

#[cfg(target_os = "windows")]
pub use platform::{acquire, activate_existing, watch};

/// 非 Windows：不做单实例限制
#[cfg(not(target_os = "windows"))]
pub fn acquire() -> bool {
    true
}

#[cfg(not(target_os = "windows"))]
pub fn activate_existing() {}

#[cfg(not(target_os = "windows"))]
pub fn watch(_app_handle: tauri::AppHandle) {}

#[cfg(target_os = "windows")]
mod platform {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use std::thread;
    use std::time::Duration;

    use tauri::AppHandle;

    use super::{EVENT_NAME, MUTEX_NAME};

    /// `CreateMutexW` 发现同名对象已存在
    const ERROR_ALREADY_EXISTS: u32 = 183;
    /// 打开事件时需要的权限（只用来 SetEvent）
    const EVENT_MODIFY_STATE: u32 = 0x0002;
    /// `WaitForSingleObject` 等到对象的返回值
    const WAIT_OBJECT_0: u32 = 0;
    const INFINITE: u32 = 0xFFFF_FFFF;
    /// 第二份实例启动时，第一份可能还没建好事件：重试次数 × 间隔 = 最多等 1 秒
    const OPEN_RETRY: usize = 20;
    const OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(50);

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateMutexW(attrs: *mut c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
        fn CreateEventW(
            attrs: *mut c_void,
            manual_reset: i32,
            initial_state: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> *mut c_void;
        fn SetEvent(handle: *mut c_void) -> i32;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }

    /// 互斥体句柄。进程退出前不能关：关掉就等于放弃「我还在跑」的声明
    static MUTEX: OnceLock<isize> = OnceLock::new();

    /// 宽字符串（以 NUL 结尾）
    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 创建（或打开）单实例互斥体。
    ///
    /// 返回 true 表示本进程是唯一实例，可以继续启动；
    /// 返回 false 表示已经有一份在跑，调用方应当唤起它然后退出。
    pub fn acquire() -> bool {
        let name = wide(MUTEX_NAME);
        // SAFETY: 名字是合法的宽字符串；句柄刻意不关闭，由系统在进程退出时回收
        let handle = unsafe { CreateMutexW(null_mut(), 0, name.as_ptr()) };
        // 必须紧接着读错误码，中间不能再调用其它 Win32 函数
        let last_error = unsafe { GetLastError() };

        if handle.is_null() {
            eprintln!("[instance] 创建单实例互斥体失败（错误码 {last_error}），按首次启动处理");
            return true;
        }
        // 句柄只是「占位」，后面用不到，但要一直留着
        let _ = MUTEX.set(handle as isize);
        last_error != ERROR_ALREADY_EXISTS
    }

    /// 通知已经在跑的那一份「有人又启动了，把面板弹出来」。
    /// 找不到事件（对方还在启动中）就重试几次，实在找不到就安静退出。
    pub fn activate_existing() {
        let name = wide(EVENT_NAME);
        for attempt in 0..OPEN_RETRY {
            // SAFETY: 名字是合法的宽字符串
            let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
            if !handle.is_null() {
                // SAFETY: handle 是刚打开的事件句柄，用完立刻关闭
                unsafe {
                    SetEvent(handle);
                    CloseHandle(handle);
                }
                eprintln!("[instance] 已经有实例在运行，已通知它显示面板（第 {} 次尝试）", attempt + 1);
                return;
            }
            thread::sleep(OPEN_RETRY_INTERVAL);
        }
        let code = unsafe { GetLastError() };
        eprintln!("[instance] 已经有实例在运行，但找不到它的激活事件（错误码 {code}）");
    }

    /// 起一个线程等「第二份实例」的通知，收到就把面板显示出来。
    pub fn watch(app_handle: AppHandle) {
        let name = wide(EVENT_NAME);
        // 自动复位事件：每收到一次 SetEvent 就唤醒一次，不需要手动 Reset
        // SAFETY: 名字是合法的宽字符串；句柄交给监听线程持有，进程退出时回收
        let handle = unsafe { CreateEventW(null_mut(), 0, 0, name.as_ptr()) };
        if handle.is_null() {
            let code = unsafe { GetLastError() };
            eprintln!("[instance] 创建激活事件失败（错误码 {code}），第二份启动时不会唤起面板");
            return;
        }
        // 裸句柄不能跨线程传递，转成整数带进线程再转回来
        let handle = handle as isize;

        let spawn_result = thread::Builder::new()
            .name("single-instance".into())
            .spawn(move || loop {
                // SAFETY: handle 在本线程生命周期内一直有效（进程不退出就不关闭）
                let result = unsafe { WaitForSingleObject(handle as *mut c_void, INFINITE) };
                if result != WAIT_OBJECT_0 {
                    eprintln!("[instance] 等待激活事件失败（返回值 {result}），单实例唤起功能停止");
                    break;
                }
                eprintln!("[instance] 收到第二份实例的唤起请求，显示面板");
                crate::hotkey::show_window(&app_handle);
            });

        if let Err(err) = spawn_result {
            eprintln!("[instance] 创建激活监听线程失败: {err}");
        }
    }
}
