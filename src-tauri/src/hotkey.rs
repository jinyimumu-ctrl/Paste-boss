//! 全局快捷键 + 窗口「贴边弹出」+ 点卡片自动粘贴。
//!
//! 全部直接用 Win32 API 实现，不额外引入插件依赖：
//! - 快捷键：`RegisterHotKey(hwnd = NULL)` 会把 `WM_HOTKEY` 投递到**调用它的那个线程**的消息队列，
//!   所以这里开一条专用线程，阻塞在 `GetMessageW` 上等按键（不占 CPU），
//!   收到按键后回调 Tauri 的窗口 API（这些 API 本身是线程安全的）。
//! - 自动粘贴：唤出面板时记下当时的前台窗口，点卡片后 `SetForegroundWindow` 切回去，
//!   再用 `SendInput` 补一个 Ctrl+V，用户就不用自己按了。

use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager, PhysicalPosition};

/// 主窗口的 label，与 tauri.conf.json 中的默认值一致
const MAIN_WINDOW: &str = "main";
/// 热键编号，取消注册时要用同一个值
const HOTKEY_ID: i32 = 1;
/// 与屏幕边缘留的空隙（物理像素）
const SCREEN_MARGIN: i32 = 16;
/// 等目标窗口拿到焦点：5ms × 40 次 = 最多 200ms，通常第一轮就到位
const FOCUS_WAIT_STEP: Duration = Duration::from_millis(5);
const FOCUS_WAIT_STEPS: usize = 40;
/// 切到目标窗口后再等一会儿才发按键，避免目标窗口刚激活就把按键吞掉
const INPUT_SETTLE: Duration = Duration::from_millis(15);

/// 唤出面板之前的前台窗口句柄（HWND 原始值，0 表示没记录到）。
/// 点卡片时把内容粘贴回这个窗口，就是「从哪儿来回哪儿去」。
static PREVIOUS_FOREGROUND: AtomicIsize = AtomicIsize::new(0);

/// 粘贴之后面板要不要「失焦也保持打开」。
///
/// 粘贴必须把焦点交给目标窗口（否则 Ctrl+V 会打到面板自己身上），
/// 但用户希望面板留在屏幕上继续看/继续贴，什么时候收起来由用户自己决定。
static KEEP_OPEN_AFTER_PASTE: AtomicBool = AtomicBool::new(false);

/// 刚粘贴完、面板应当继续保持打开（`lib.rs` 的失焦事件里会查询它）
pub fn keep_open_on_blur() -> bool {
    KEEP_OPEN_AFTER_PASTE.load(Ordering::Relaxed)
}

/// 面板重新弹出或重新收起后，恢复默认的「失焦自动收起」
fn reset_keep_open() {
    KEEP_OPEN_AFTER_PASTE.store(false, Ordering::Relaxed);
}

/// 用户配置的「常驻」开关：开启后窗口失去焦点也不收起（`lib.rs` 的失焦事件里会查询它）。
///
/// 和 `KEEP_OPEN_AFTER_PASTE` 的区别：那个是「刚粘贴完」的临时豁免，唤出/收起一次就失效；
/// 这里是用户明确选择的长期行为，存进 `config.json`，重启后依然有效。
static PINNED: AtomicBool = AtomicBool::new(false);

/// 设置常驻开关（启动时按配置装好，设置面板里随时可改）
pub fn set_pinned(value: bool) {
    PINNED.store(value, Ordering::Relaxed);
}

/// 当前是否常驻显示
pub fn pinned() -> bool {
    PINNED.load(Ordering::Relaxed)
}

/// 刚呼出面板之后的保护期终点（进程启动以来的毫秒数，0 表示没有保护期）。
///
/// 为什么要它：`show_window` 里 `show()` 之后紧跟 `set_focus()`，但 Windows 的前台锁
/// （foreground lock）有时会把这次激活挡回去 —— 窗口刚显示出来就收到一次 `Focused(false)`。
/// 若按「失焦即收」处理，用户看到的就是「按 Alt+V 面板闪一下就没了」。
/// 保护期只持续几百毫秒，到点自动失效，不影响之后「真的切到别的程序 → 收起」。
static OPEN_GUARD_UNTIL: AtomicU64 = AtomicU64::new(0);

/// 保护期长度：够覆盖「显示 + 抢焦点」被挡回去的那一下，又不至于让用户觉得收起变迟钝
const OPEN_GUARD: Duration = Duration::from_millis(600);

/// 进程启动以来的毫秒数（单调时钟，不受系统时间调整影响）
fn process_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 面板是否还在「刚呼出」的保护期内
pub fn open_guard_active() -> bool {
    let until = OPEN_GUARD_UNTIL.load(Ordering::Relaxed);
    until != 0 && process_ms() < until
}

/// 呼出面板时开始一段保护期
fn arm_open_guard() {
    let until = process_ms().saturating_add(OPEN_GUARD.as_millis() as u64);
    OPEN_GUARD_UNTIL.store(until, Ordering::Relaxed);
}

/// 用户主动收起面板时清掉保护期，下一次失焦该收就收
fn clear_open_guard() {
    OPEN_GUARD_UNTIL.store(0, Ordering::Relaxed);
}

/// 一次失焦之后能查到的全部现场状态。
///
/// 用结构体而不是一串 `bool`：这几个条件彼此很像（都是「这种情况下别收」），
/// 位置参数一多就极容易传错顺序，写出来也看不懂谁是谁。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlurSnapshot {
    /// 窗口现在是否还显示着（自己 hide() 也会触发失焦）
    pub visible: bool,
    /// 刚粘贴完，面板本来就该继续留着
    pub keep_open: bool,
    /// 用户开了「常驻显示」
    pub pinned: bool,
    /// 还在「刚呼出」的保护期内
    pub open_guard: bool,
    /// 窗口自己仍是前台窗口（点标题栏、拖边框时就是这种状态）
    pub foreground_self: bool,
    /// 鼠标左键还按着（正在拖动或缩放）
    pub button_down: bool,
}

/// 失焦之后到底要不要把面板收起来。
///
/// 不能「一失焦就 hide」：Windows 在**点标题栏、拖窗口、拖边框缩放、点最小化/最大化**
/// 这些非客户区操作时会先给窗口发一次失焦（系统进了自己的模态循环，焦点暂时离开客户区），
/// 但窗口其实还在前台、鼠标左键还按着。按「立刻收起」处理，用户看到的就是
/// 「一碰标题栏整个面板就没了，窗口根本拖不动、也缩放不了」。
///
/// 所以真正的判断推迟一小会儿（`lib.rs` 的 `BLUR_GRACE`），再用现场状态复核：
/// - 窗口已经不在屏幕上了 → 什么都不用做；
/// - 刚粘贴完 或 用户开了常驻 或 刚呼出的保护期内 → 本来就不该收；
/// - 窗口仍在前台 → 那是标题栏 / 边框的模态循环，不是真的失焦；
/// - 左键还按着 → 正在拖动或缩放；
/// - 全都不是 → 真的被别的窗口顶到后面了，收起。
///
/// 参数全部由调用方现场查询，纯函数，方便单测各种组合。
pub fn should_hide_after_blur(state: &BlurSnapshot) -> bool {
    if !state.visible {
        // 自己 hide() 也会触发一次失焦，这里必须早退，否则会一直重复收
        return false;
    }
    if state.keep_open || state.pinned || state.open_guard {
        return false;
    }
    if state.foreground_self {
        return false;
    }
    if state.button_down {
        return false;
    }
    true
}

/// 主窗口现在是不是前台窗口。
///
/// 拖动标题栏、拖边框缩放时它**仍然是**前台窗口，所以能用来区分
/// 「用户在自己窗户上按住鼠标」和「用户真的切到别的程序去了」。
#[cfg(target_os = "windows")]
pub fn window_is_foreground(app_handle: &AppHandle) -> bool {
    let own = app_handle
        .get_webview_window(MAIN_WINDOW)
        .and_then(|window| window.hwnd().ok())
        .map(|hwnd| hwnd.0 as isize)
        .unwrap_or(0);
    if own == 0 {
        // 查不到自己的句柄：宁可认为「不在前台」，后面还有鼠标键那道判断兜着
        return false;
    }
    // SAFETY: GetForegroundWindow 只读查询，无副作用
    unsafe { GetForegroundWindow() as isize == own }
}

/// 非 Windows：没有前台窗口的概念
#[cfg(not(target_os = "windows"))]
pub fn window_is_foreground(_app_handle: &AppHandle) -> bool {
    false
}

/// 鼠标左键是否还按着（拖动 / 缩放窗口时一直按住不放）
#[cfg(target_os = "windows")]
pub fn mouse_button_down() -> bool {
    /// 左键的虚拟键码
    const VK_LBUTTON: i32 = 0x01;
    /// 返回值最高位为 1 表示此刻正按着
    const KEY_PRESSED: u16 = 0x8000;
    // SAFETY: GetAsyncKeyState 只读查询，不阻塞、不改系统状态
    let state = unsafe { GetAsyncKeyState(VK_LBUTTON) } as u16;
    state & KEY_PRESSED != 0
}

/// 非 Windows：不做鼠标状态判断
#[cfg(not(target_os = "windows"))]
pub fn mouse_button_down() -> bool {
    false
}

// ---------------------------------------------------------------------------
// 快捷键描述串：「Alt+V」这样的写法和 Win32 参数之间的互转
// ---------------------------------------------------------------------------

/// 一条全局快捷键：修饰键位 + 虚拟键码
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HotkeySpec {
    /// 修饰键位（`MOD_ALT` / `MOD_CTRL` / `MOD_SHIFT` / `MOD_WIN` 的按位或）
    pub modifiers: u32,
    /// 虚拟键码（`RegisterHotKey` 的 vk 参数）
    pub vk: u32,
}

impl HotkeySpec {
    /// 出厂默认值：Alt+V
    pub const DEFAULT: HotkeySpec = HotkeySpec {
        modifiers: MOD_ALT,
        vk: VK_V,
    };

    /// 给用户看的写法，例如 `Ctrl+Alt+V`
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.modifiers & MOD_CTRL != 0 {
            parts.push("Ctrl");
        }
        if self.modifiers & MOD_ALT != 0 {
            parts.push("Alt");
        }
        if self.modifiers & MOD_SHIFT != 0 {
            parts.push("Shift");
        }
        if self.modifiers & MOD_WIN != 0 {
            parts.push("Win");
        }
        let mut text = parts.join("+");
        if !text.is_empty() {
            text.push('+');
        }
        text.push_str(&key_name(self.vk));
        text
    }
}

/// 虚拟键码 → 名字。
/// 名字同时被 `display` 和前端录制功能使用，改动时要两边一起看。
fn key_name(vk: u32) -> String {
    match vk {
        // 0-9 / A-Z 的虚拟键码恰好等于它们的 ASCII 码
        0x30..=0x39 | 0x41..=0x5A => char::from(vk as u8).to_string(),
        // F1 = 0x70，依次递增到 F24 = 0x87
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        _ => match vk {
            0x20 => "Space".to_string(),
            0x09 => "Tab".to_string(),
            0x0D => "Enter".to_string(),
            0x1B => "Esc".to_string(),
            0x08 => "Backspace".to_string(),
            0x2D => "Insert".to_string(),
            0x2E => "Delete".to_string(),
            0x24 => "Home".to_string(),
            0x23 => "End".to_string(),
            0x21 => "PageUp".to_string(),
            0x22 => "PageDown".to_string(),
            0x26 => "Up".to_string(),
            0x28 => "Down".to_string(),
            0x25 => "Left".to_string(),
            0x27 => "Right".to_string(),
            // 兜底：万一以后加了别的键，至少还能看出来是哪个
            _ => format!("VK{vk:02X}"),
        },
    }
}

/// 名字 → 虚拟键码（大小写不敏感）。
/// 只接受「能安全当快捷键」的键：字母、数字、F1-F24 和一批功能键，
/// 标点符号各键盘布局不一致，干脆不支持。
fn vk_from_name(name: &str) -> Option<u32> {
    let upper = name.trim().to_ascii_uppercase();

    // 单个字母 / 数字
    if upper.len() == 1 {
        let byte = upper.as_bytes()[0];
        if byte.is_ascii_uppercase() || byte.is_ascii_digit() {
            return Some(u32::from(byte));
        }
    }

    // F1 - F24
    if let Some(digits) = upper.strip_prefix('F') {
        if let Ok(number) = digits.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Some(0x6F + number);
            }
        }
    }

    Some(match upper.as_str() {
        "SPACE" => 0x20,
        "TAB" => 0x09,
        "ENTER" | "RETURN" => 0x0D,
        "ESC" | "ESCAPE" => 0x1B,
        "BACKSPACE" => 0x08,
        "INSERT" | "INS" => 0x2D,
        "DELETE" | "DEL" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        "UP" | "ARROWUP" => 0x26,
        "DOWN" | "ARROWDOWN" => 0x28,
        "LEFT" | "ARROWLEFT" => 0x25,
        "RIGHT" | "ARROWRIGHT" => 0x27,
        _ => return None,
    })
}

/// 解析 `Ctrl+Alt+V` 这样的写法。
///
/// 规则和系统保持一致：必须**至少有一个** Ctrl / Alt / Win，且只能有一个主按键。
/// 只按 Shift+字母、或者干脆不带修饰键的组合系统根本注册不了，这里提前挡掉并说明原因。
pub fn parse_spec(spec: &str) -> Result<HotkeySpec, String> {
    let mut modifiers = 0u32;
    let mut key: Option<u32> = None;

    for raw in spec.split('+') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_uppercase().as_str() {
            "CTRL" | "CONTROL" => modifiers |= MOD_CTRL,
            "ALT" => modifiers |= MOD_ALT,
            "SHIFT" => modifiers |= MOD_SHIFT,
            "WIN" | "META" | "SUPER" | "CMD" => modifiers |= MOD_WIN,
            _ => {
                let Some(vk) = vk_from_name(part) else {
                    return Err(format!("无法识别的按键「{part}」"));
                };
                if key.replace(vk).is_some() {
                    return Err("一次只能设置一个主按键".to_string());
                }
            }
        }
    }

    let Some(vk) = key else {
        return Err("缺少主按键（例如 Ctrl+Alt+V 里的 V）".to_string());
    };
    if modifiers & (MOD_CTRL | MOD_ALT | MOD_WIN) == 0 {
        return Err("快捷键至少要包含 Ctrl、Alt 或 Win 其中之一".to_string());
    }
    Ok(HotkeySpec { modifiers, vk })
}

// ---------------------------------------------------------------------------
// 运行状态：当前快捷键、监听线程 id、待生效的更换请求
// ---------------------------------------------------------------------------

/// 当前**已经注册成功**的快捷键（换快捷键失败时保持旧值）
static CURRENT: Mutex<HotkeySpec> = Mutex::new(HotkeySpec::DEFAULT);
/// 当前是否真的注册上了（启动时也可能一次都没成功，比如被别人占着）
static REGISTERED: AtomicBool = AtomicBool::new(false);
/// 监听线程 id（0 表示线程还没起来）
static LISTENER_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// 待生效的快捷键 + 把注册结果回传给 `apply` 的通道
static PENDING: Mutex<Option<(HotkeySpec, Sender<Result<(), String>>)>> = Mutex::new(None);

/// 加锁并按「中毒也照用」处理：换快捷键失败不该让整个程序不可用
fn lock_current() -> MutexGuard<'static, HotkeySpec> {
    CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 同上，用于待生效请求
fn lock_pending() -> MutexGuard<'static, Option<(HotkeySpec, Sender<Result<(), String>>)>> {
    PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 当前生效的快捷键（给设置面板显示用）
pub fn current() -> HotkeySpec {
    *lock_current()
}

/// 等快捷键线程起来并拿到它的 id（最多等 1 秒）。
///
/// 线程是 `start` 里异步创建的，正常使用时早就在跑了；
/// 但应用刚启动的那几百毫秒内，或者某次线程意外退出后重建时，
/// 这里直接报错会让用户以为「改快捷键不能用」，等一下更稳妥。
fn wait_for_listener() -> u32 {
    for _ in 0..50 {
        let thread_id = LISTENER_THREAD_ID.load(Ordering::Relaxed);
        if thread_id != 0 {
            return thread_id;
        }
        thread::sleep(Duration::from_millis(20));
    }
    0
}

/// 换一个全局快捷键：交给监听线程试注册，注册成功才真正生效。
///
/// `RegisterHotKey(hwnd = NULL)` 只对**调用它的线程**生效，所以这里不能自己注册，
/// 只能把新快捷键放进 `PENDING`、唤醒监听线程，再等它把结果送回来（最多 2 秒）。
/// 这样「新快捷键被别的程序占用」的情况能被发现，而不是静默变成「按了没反应」。
pub fn apply(spec: &str) -> Result<HotkeySpec, String> {
    let next = parse_spec(spec)?;
    // 和当前一样、并且确实注册着，就当没改过
    if next == current() && REGISTERED.load(Ordering::Relaxed) {
        return Ok(next);
    }

    let thread_id = wait_for_listener();
    if thread_id == 0 {
        return Err("快捷键线程还没准备好，请稍后再试".to_string());
    }

    let (sender, receiver) = mpsc::channel();
    *lock_pending() = Some((next, sender));

    // 唤醒监听线程去处理 PENDING。消息循环是阻塞在 GetMessageW 上的，
    // 只有往它的队列里投消息才能让它动起来。
    #[cfg(target_os = "windows")]
    let posted = unsafe { PostThreadMessageW(thread_id, WM_RELOAD_HOTKEY, 0, 0) };
    #[cfg(not(target_os = "windows"))]
    let posted = 0;

    if posted == 0 {
        *lock_pending() = None;
        return Err("无法通知快捷键线程".to_string());
    }

    match receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(next),
        Ok(Err(message)) => Err(message),
        Err(_) => {
            // 超时：把请求撤回来，免得「界面说失败了、过一会儿又悄悄生效」
            *lock_pending() = None;
            Err("切换快捷键超时，请重试".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Win32 定义（只在 Windows 上需要；非 Windows 走 show_window 的降级分支）
// ---------------------------------------------------------------------------

#[cfg(target_os = "windows")]
const WM_HOTKEY: u32 = 0x0312;
// 下面几个位标志同时用于「解析用户设置的快捷键」，所以不再限制在 Windows 上定义；
// 数值和 Win32 `RegisterHotKey` 的 fuModifiers 完全一致。
/// 修饰键：Alt
pub const MOD_ALT: u32 = 0x0001;
/// 修饰键：Ctrl
pub const MOD_CTRL: u32 = 0x0002;
/// 修饰键：Shift
pub const MOD_SHIFT: u32 = 0x0004;
/// 修饰键：Win
pub const MOD_WIN: u32 = 0x0008;
/// 按住不放时不要重复触发（Win7+）
const MOD_NOREPEAT: u32 = 0x4000;
/// 虚拟键码：V（默认快捷键的主按键，也是粘贴时要按的键）
const VK_V: u32 = 0x56;
/// 虚拟键码：Ctrl
#[cfg(target_os = "windows")]
const VK_CONTROL: u16 = 0x11;
/// SendInput 的事件类型：键盘
#[cfg(target_os = "windows")]
const INPUT_KEYBOARD: u32 = 1;
/// 按键抬起标志
#[cfg(target_os = "windows")]
const KEYEVENTF_KEYUP: u32 = 0x0002;

/// Win32 的 POINT
#[cfg(target_os = "windows")]
#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

/// Win32 的 MSG（GetMessageW 会往这块内存里写数据，字段顺序不能改）
#[cfg(target_os = "windows")]
#[repr(C)]
struct Msg {
    hwnd: *mut c_void,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    pt: Point,
    l_private: u32,
}

/// Win32 的 KEYBDINPUT
#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy)]
struct KeybdInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

/// Win32 的 MOUSEINPUT。我们只用键盘事件，但 INPUT 里的联合体大小由最大的成员决定，
/// 所以必须把它写全，否则 `SendInput` 会把每个元素按错误的步长解析。
#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy)]
union InputUnion {
    ki: KeybdInput,
    mi: MouseInput,
}

/// Win32 的 INPUT
#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    kind: u32,
    data: InputUnion,
}

#[cfg(target_os = "windows")]
#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterHotKey(hwnd: *mut c_void, id: i32, modifiers: u32, vk: u32) -> i32;
    fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: *mut c_void, filter_min: u32, filter_max: u32) -> i32;
    fn GetForegroundWindow() -> *mut c_void;
    fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
    fn IsWindow(hwnd: *mut c_void) -> i32;
    fn IsIconic(hwnd: *mut c_void) -> i32;
    /// 查询按键/鼠标键此刻的状态（最高位为 1 表示正按着）
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn GetClassNameW(hwnd: *mut c_void, buffer: *mut u16, max_count: i32) -> i32;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    /// 往指定线程的消息队列里投一条消息（应用没有窗口，靠它唤醒消息循环）
    fn PostThreadMessageW(thread_id: u32, message: u32, w_param: usize, l_param: isize) -> i32;
    fn PeekMessageW(
        msg: *mut Msg,
        hwnd: *mut c_void,
        filter_min: u32,
        filter_max: u32,
        remove: u32,
    ) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
    /// 当前线程 id：`RegisterHotKey` 是按线程生效的，换快捷键必须回到同一条线程
    fn GetCurrentThreadId() -> u32;
}

/// 「这个组合键已经被别的程序占用」的错误码（`RegisterHotKey` 失败时返回）
#[cfg(target_os = "windows")]
const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;
/// 让监听线程重新注册快捷键的消息（`WM_APP + 1`，应用自定义消息的常规起点）
#[cfg(target_os = "windows")]
const WM_RELOAD_HOTKEY: u32 = 0x8000 + 1;
/// `PeekMessageW` 的「只看不取」标志
#[cfg(target_os = "windows")]
const PM_NOREMOVE: u32 = 0x0000;

// ---------------------------------------------------------------------------
// 对外入口
// ---------------------------------------------------------------------------

/// 注册全局快捷键（默认 Alt+V，可在设置里改），之后按它就能显示/隐藏窗口。
///
/// 注册失败（比如被别的程序占用）时会打印日志并直接把窗口显示出来，
/// 避免用户完全找不到这个程序。
pub fn start(app_handle: AppHandle, spec: &str) {
    // 配置里的值被手改坏了也不能让程序起不来，退回默认的 Alt+V
    let initial = parse_spec(spec).unwrap_or_else(|err| {
        eprintln!(
            "[hotkey] 配置里的快捷键「{spec}」不可用（{err}），改用默认的 {}",
            HotkeySpec::DEFAULT.display()
        );
        HotkeySpec::DEFAULT
    });
    *lock_current() = initial;

    let spawn_result = thread::Builder::new()
        .name("hotkey-listener".into())
        .spawn(move || run_message_loop(app_handle, initial));

    if let Err(err) = spawn_result {
        eprintln!("[hotkey] 创建快捷键线程失败，全局快捷键不可用: {err}");
    }
}

// ---------------------------------------------------------------------------
// 内部实现
// ---------------------------------------------------------------------------

/// 注册热键并阻塞等待按键 / 更换请求；正常情况下这条线程永远不退出。
#[cfg(target_os = "windows")]
fn run_message_loop(app_handle: AppHandle, initial: HotkeySpec) {
    // GetMessageW 取第一条消息时会顺手创建线程消息队列，但 PostThreadMessageW 要求队列已存在。
    // 先 Peek 一下（PM_NOREMOVE，不取走消息），保证线程 id 对外可见之前队列就已经建好，
    // 否则第一次换快捷键会报「无法通知快捷键线程」。
    let mut peek = MaybeUninit::<Msg>::zeroed();
    // SAFETY: peek 指向一块已清零的 Msg，PM_NOREMOVE 表示只看不取
    unsafe { PeekMessageW(peek.as_mut_ptr(), null_mut(), 0, 0, PM_NOREMOVE) };

    // SAFETY: 无参数、无副作用，只返回当前线程 id
    LISTENER_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);

    let mut active = match register(initial) {
        Ok(()) => Some(initial),
        Err(_) => {
            // 快捷键不可用时至少把窗口显示出来，别让程序变成「隐形」的
            show_window(&app_handle);
            None
        }
    };

    loop {
        // 每次循环都重新初始化，避免残留上一轮的数据
        let mut msg = MaybeUninit::<Msg>::zeroed();
        // SAFETY: 指针指向一块足够大的、已清零的 Msg；hwnd 为 NULL 表示取本线程所有消息
        let result = unsafe { GetMessageW(msg.as_mut_ptr(), null_mut(), 0, 0) };

        // 0 = WM_QUIT，-1 = 出错，两种情况都结束监听
        if result <= 0 {
            eprintln!("[hotkey] 消息循环结束（返回值 {result}），全局快捷键不再可用");
            break;
        }

        // SAFETY: 返回值为正表示 msg 已被 GetMessageW 填充
        let msg = unsafe { msg.assume_init() };
        if msg.message == WM_HOTKEY {
            // 这条线程只注册了一个热键，但保险起见还是核对一下编号
            if msg.w_param as i32 == HOTKEY_ID {
                toggle_window(&app_handle);
            }
        } else if msg.message == WM_RELOAD_HOTKEY {
            active = reload();
        }
    }

    if active.is_some() {
        // SAFETY: 与上面的注册成对出现
        unsafe {
            UnregisterHotKey(null_mut(), HOTKEY_ID);
        }
        REGISTERED.store(false, Ordering::Relaxed);
    }
    // 线程退出后 id 失效，置 0 让 `apply` 直接报「还没准备好」而不是等 2 秒超时
    LISTENER_THREAD_ID.store(0, Ordering::Relaxed);
}

/// 试着注册一个快捷键。
///
/// 成功返回 `Ok`，失败返回 Win32 错误码（1409 表示这个组合已经被别的程序占用）。
/// 只允许在快捷键线程里调用 —— `RegisterHotKey(hwnd = NULL)` 注册的是**调用线程**。
#[cfg(target_os = "windows")]
fn register(spec: HotkeySpec) -> Result<(), u32> {
    // SAFETY: hwnd 为 NULL，把 WM_HOTKEY 投递到本线程的消息队列
    let registered = unsafe {
        RegisterHotKey(
            null_mut(),
            HOTKEY_ID,
            spec.modifiers | MOD_NOREPEAT,
            spec.vk,
        )
    };

    if registered == 0 {
        // SAFETY: RegisterHotKey 刚失败，错误码就是本次调用的
        let code = unsafe { GetLastError() };
        eprintln!(
            "[hotkey] 注册 {} 失败（错误码 {code}）{}",
            spec.display(),
            if code == ERROR_HOTKEY_ALREADY_REGISTERED {
                "：这个组合键已经被其它程序占用"
            } else {
                ""
            }
        );
        REGISTERED.store(false, Ordering::Relaxed);
        return Err(code);
    }

    REGISTERED.store(true, Ordering::Relaxed);
    eprintln!(
        "[hotkey] 已注册全局快捷键 {}（显示/隐藏窗口）",
        spec.display()
    );
    Ok(())
}

/// 处理「换快捷键」请求：注销旧的 → 注册新的 → 把结果回传给 `apply`。
///
/// 新组合用不了（被占用）时会把旧组合装回去，避免两个都用不了、
/// 用户还以为快捷键彻底坏了。
#[cfg(target_os = "windows")]
fn reload() -> Option<HotkeySpec> {
    let previous = current();
    let Some((next, reply)) = lock_pending().take() else {
        // 没有待处理请求：保持现状
        return REGISTERED.load(Ordering::Relaxed).then_some(previous);
    };

    // 先腾出旧的，才能判断新的是不是真的可用
    if REGISTERED.load(Ordering::Relaxed) {
        // SAFETY: 与注册成对出现
        unsafe {
            UnregisterHotKey(null_mut(), HOTKEY_ID);
        }
        REGISTERED.store(false, Ordering::Relaxed);
    }

    match register(next) {
        Ok(()) => {
            *lock_current() = next;
            let _ = reply.send(Ok(()));
            Some(next)
        }
        Err(code) => {
            let restored = register(previous).is_ok();
            let reason = if code == ERROR_HOTKEY_ALREADY_REGISTERED {
                format!("{} 已被其它程序占用，换一个试试", next.display())
            } else {
                format!("注册 {} 失败（错误码 {code}）", next.display())
            };
            let _ = reply.send(Err(reason));
            if restored {
                Some(previous)
            } else {
                None
            }
        }
    }
}

/// 非 Windows：没有 RegisterHotKey，只打印一次提示，不影响其它功能。
#[cfg(not(target_os = "windows"))]
fn run_message_loop(app_handle: AppHandle, _initial: HotkeySpec) {
    eprintln!("[hotkey] 当前平台暂不支持全局快捷键，请手动显示窗口");
    show_window(&app_handle);
}

/// 可见就隐藏，隐藏就（贴边）弹出来。
/// 全局快捷键和「左键单击托盘图标」走的都是这里，保证两边手感一致。
pub fn toggle_window(app_handle: &AppHandle) {
    let Some(window) = app_handle.get_webview_window(MAIN_WINDOW) else {
        eprintln!("[hotkey] 找不到主窗口 {MAIN_WINDOW}");
        return;
    };

    match window.is_visible() {
        Ok(true) => {
            // 收起之后恢复「失焦自动隐藏」，下次唤出还是熟悉的手感
            reset_keep_open();
            if let Err(err) = window.hide() {
                eprintln!("[hotkey] 隐藏窗口失败: {err}");
            }
        }
        Ok(false) => show_window(app_handle),
        Err(err) => {
            // 查不到状态时按「没显示」处理，保证按键总能唤出窗口
            eprintln!("[hotkey] 查询窗口可见性失败: {err}");
            show_window(app_handle);
        }
    }
}

/// 显示窗口并抢焦点。
///
/// 位置跟随鼠标所在的显示器，贴在**工作区右边缘**弹出；
/// 工作区放不下（比如窗口比屏幕还宽）时，自动往左挪到能放下的位置。
pub fn show_window(app_handle: &AppHandle) {
    let Some(window) = app_handle.get_webview_window(MAIN_WINDOW) else {
        eprintln!("[hotkey] 找不到主窗口 {MAIN_WINDOW}");
        return;
    };

    // 重新弹出 = 新一轮使用，先恢复「失焦自动隐藏」
    reset_keep_open();
    // 刚显示出来的这一小会儿别被「抢焦点失败」触发的失焦立刻收回去（见 OPEN_GUARD）
    arm_open_guard();

    // 只在「从隐藏状态唤起」时记录目标窗口：这一刻抢占焦点之前的那个窗口，
    // 才是用户真正在用的窗口。面板本来就开着的话，记下来的是我们自己，没有意义。
    if !window.is_visible().unwrap_or(false) {
        capture_previous_foreground(&window);
    }

    // 多屏时跟着鼠标走，比固定弹在主屏更符合直觉
    let monitor = app_handle
        .cursor_position()
        .ok()
        .and_then(|cursor| app_handle.monitor_from_point(cursor.x, cursor.y).ok().flatten());

    if let Some(monitor) = monitor {
        let area = monitor.work_area();
        if let Ok(size) = window.outer_size() {
            let right_edge = area.position.x + area.size.width as i32;
            let bottom_edge = area.position.y + area.size.height as i32;

            // 贴右边缘，但保证不超出工作区左边界
            let x = (right_edge - size.width as i32 - SCREEN_MARGIN)
                .max(area.position.x + SCREEN_MARGIN);
            // 顶部对齐，窗口太高时往下夹
            let y = (area.position.y + SCREEN_MARGIN)
                .min(bottom_edge - size.height as i32 - SCREEN_MARGIN)
                .max(area.position.y);

            if let Err(err) = window.set_position(PhysicalPosition::new(x, y)) {
                eprintln!("[hotkey] 设置窗口位置失败: {err}");
            }
        }
    }

    if let Err(err) = window.show() {
        eprintln!("[hotkey] 显示窗口失败: {err}");
    }
    // Windows 上 show() 不一定抢到焦点，显式再要一次，否则用户还得点一下才能操作
    if let Err(err) = window.set_focus() {
        eprintln!("[hotkey] 设置焦点失败: {err}");
    }
    eprintln!("[hotkey] 已呼出面板（{}ms 内不响应失焦收起）", OPEN_GUARD.as_millis());
}

/// 收起面板（前端按 Esc 时调用，和 Alt+V 的隐藏走同一套逻辑）
pub fn hide_window(app_handle: &AppHandle) {
    let Some(window) = app_handle.get_webview_window(MAIN_WINDOW) else {
        eprintln!("[hotkey] 找不到主窗口 {MAIN_WINDOW}");
        return;
    };
    // 用户主动收起，恢复正常「失焦自动隐藏」
    reset_keep_open();
    clear_open_guard();
    if let Err(err) = window.hide() {
        eprintln!("[hotkey] 隐藏窗口失败: {err}");
    }
}

/// 记下唤出面板前的前台窗口，点卡片时就粘贴回它。
#[cfg(target_os = "windows")]
fn capture_previous_foreground(window: &tauri::WebviewWindow) {
    // 面板自己不能算目标，否则内容会「粘贴」回面板里
    let own = window.hwnd().map(|hwnd| hwnd.0 as isize).unwrap_or(0);
    // SAFETY: GetForegroundWindow 只是查询，没有参数
    let foreground = unsafe { GetForegroundWindow() } as isize;

    let target = if foreground == 0 || foreground == own {
        0
    } else {
        foreground
    };
    PREVIOUS_FOREGROUND.store(target, Ordering::Relaxed);
}

#[cfg(not(target_os = "windows"))]
fn capture_previous_foreground(_window: &tauri::WebviewWindow) {
    // 其它平台暂不做自动粘贴，保持 0 表示「没有目标窗口」
}

/// 桌面、任务栏这些外壳窗口的类名：它们收不到有意义的粘贴
#[cfg(target_os = "windows")]
const SHELL_WINDOW_CLASSES: [&str; 3] = ["Progman", "WorkerW", "Shell_TrayWnd"];

/// 判断句柄是不是桌面/任务栏之类的外壳窗口
#[cfg(target_os = "windows")]
fn is_shell_window(hwnd: *mut c_void) -> bool {
    let mut buffer = [0u16; 64];
    // SAFETY: 缓冲区容量与传入的长度一致，GetClassNameW 最多写这么多字节
    let len = unsafe { GetClassNameW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if len <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buffer[..len as usize]);
    SHELL_WINDOW_CLASSES.contains(&class.as_str())
}

/// 点卡片后：切回唤出面板前的那个窗口，并模拟一次 Ctrl+V。
///
/// 返回是否真的把按键送进了目标窗口。失败时内容已经写进剪贴板了，
/// 面板保持打开，用户自己按 Ctrl+V 就行。
///
/// 注意：这里**不会**收起面板。焦点交给目标窗口之后面板留在屏幕上，
/// 什么时候关闭由用户自己按 Alt+V / Esc 决定。
#[cfg(target_os = "windows")]
pub fn paste_into_previous_window(_app_handle: &AppHandle) -> bool {
    simulate_control_v()
}

/// 真正做「切前台 + 补按 Ctrl+V」的部分，不碰窗口，方便单独测试。
#[cfg(target_os = "windows")]
fn simulate_control_v() -> bool {
    let target = PREVIOUS_FOREGROUND.load(Ordering::Relaxed);
    if target == 0 {
        eprintln!("[paste] 没有记录到目标窗口，内容只写进了剪贴板，请手动 Ctrl+V");
        return false;
    }
    let target_hwnd = target as *mut c_void;

    // SAFETY: 都是只读查询；句柄失效时 IsWindow 返回 0
    if unsafe { IsWindow(target_hwnd) } == 0 {
        eprintln!("[paste] 目标窗口已经关掉了，内容只写进了剪贴板");
        // 句柄作废，别再拿它去切前台
        PREVIOUS_FOREGROUND.store(0, Ordering::Relaxed);
        return false;
    }
    // 最小化的窗口拿不到输入焦点，此时发按键会打到别的窗口上
    if unsafe { IsIconic(target_hwnd) } != 0 {
        eprintln!("[paste] 目标窗口处于最小化状态，内容只写进了剪贴板，请手动 Ctrl+V");
        return false;
    }
    // 桌面/任务栏不是能接收粘贴的容器，粘过去等于什么都没发生
    if is_shell_window(target_hwnd) {
        eprintln!("[paste] 目标窗口是桌面或任务栏，内容只写进了剪贴板");
        return false;
    }

    // SAFETY: 句柄已通过 IsWindow 校验
    //
    // 先把「保持打开」标记立起来：SetForegroundWindow 会让面板失焦，
    // 失焦事件里据此跳过自动隐藏，面板就留在屏幕上（关闭时机交给用户）。
    KEEP_OPEN_AFTER_PASTE.store(true, Ordering::Relaxed);
    unsafe { SetForegroundWindow(target_hwnd) };

    // 切前台是异步的：必须等焦点真的过去再发按键，
    // 否则 Ctrl+V 会打到还处于激活状态的面板上（等于自己粘自己）
    let mut focused = false;
    for _ in 0..FOCUS_WAIT_STEPS {
        // SAFETY: 只读查询
        if unsafe { GetForegroundWindow() } == target_hwnd {
            focused = true;
            break;
        }
        thread::sleep(FOCUS_WAIT_STEP);
    }
    if !focused {
        // 焦点没切走：面板本来就还在前台，撤销上面的标记恢复正常手感
        reset_keep_open();
        eprintln!("[paste] 没能把焦点切到目标窗口，内容只写进了剪贴板，请手动 Ctrl+V");
        return false;
    }

    // 再给目标窗口一点点时间处理 WM_ACTIVATE，避免它刚激活就把按键吞掉
    thread::sleep(INPUT_SETTLE);

    let inputs = [
        key_event(VK_CONTROL, 0),                // Ctrl 按下
        key_event(VK_V as u16, 0),               // V 按下
        key_event(VK_V as u16, KEYEVENTF_KEYUP), // V 抬起
        key_event(VK_CONTROL, KEYEVENTF_KEYUP),  // Ctrl 抬起
    ];

    // SAFETY: 指针指向 4 个按 Win32 布局排好的 INPUT，长度和步长都传对了
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<Input>() as i32,
        )
    };

    if sent != inputs.len() as u32 {
        // 常见原因：目标窗口以管理员权限运行，而本程序不是（UIPI 会拦掉模拟输入）
        eprintln!(
            "[paste] 模拟 Ctrl+V 被系统拦截（{sent}/{}），目标窗口可能有更高权限",
            inputs.len()
        );
        return false;
    }
    true
}

#[cfg(not(target_os = "windows"))]
pub fn paste_into_previous_window(_app_handle: &AppHandle) -> bool {
    false
}

/// 构造一个键盘事件：`flags` 为 0 表示按下，`KEYEVENTF_KEYUP` 表示抬起
#[cfg(target_os = "windows")]
fn key_event(vk: u16, flags: u32) -> Input {
    Input {
        kind: INPUT_KEYBOARD,
        data: InputUnion {
            ki: KeybdInput {
                vk,
                scan: 0,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

#[cfg(test)]
mod spec_tests {
    use super::{MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN, HotkeySpec, parse_spec};

    #[test]
    fn default_is_alt_v() {
        assert_eq!(HotkeySpec::DEFAULT.display(), "Alt+V");
        assert_eq!(parse_spec("Alt+V").expect("解析默认快捷键"), HotkeySpec::DEFAULT);
    }

    #[test]
    fn parse_ignores_case_and_spaces() {
        let spec = parse_spec(" ctrl + ALT + k ").expect("解析混合大小写");
        assert_eq!(spec.modifiers, MOD_CTRL | MOD_ALT);
        assert_eq!(spec.vk, u32::from(b'K'));
        // 显示顺序固定为 Ctrl → Alt → Shift → Win，和用户按键习惯一致
        assert_eq!(spec.display(), "Ctrl+Alt+K");
    }

    #[test]
    fn parse_supports_named_and_function_keys() {
        let f12 = parse_spec("Win+F12").expect("解析 F12");
        assert_eq!(f12.modifiers, MOD_WIN);
        assert_eq!(f12.vk, 0x7B);
        assert_eq!(f12.display(), "Win+F12");

        let page_down = parse_spec("Ctrl+PageDown").expect("解析 PageDown");
        assert_eq!(page_down.vk, 0x22);
        assert_eq!(page_down.display(), "Ctrl+PageDown");

        let space = parse_spec("Meta+Space").expect("解析 Meta+Space");
        assert_eq!(space.modifiers, MOD_WIN);
        assert_eq!(space.vk, 0x20);
        assert_eq!(space.display(), "Win+Space");
    }

    #[test]
    fn parse_requires_a_real_modifier() {
        // 没有修饰键、只有主按键
        assert!(parse_spec("V").is_err());
        // 只有 Shift 也不行：系统本来就注册不上
        assert!(parse_spec("Shift+K").is_err());
        assert_eq!(parse_spec("K").is_err(), true);
    }

    #[test]
    fn parse_rejects_malformed_input() {
        // 缺主按键
        assert!(parse_spec("Ctrl+").is_err());
        assert!(parse_spec("Ctrl+Alt").is_err());
        // 两个主按键
        assert!(parse_spec("Ctrl+A+B").is_err());
        // 不认识的键名
        assert!(parse_spec("Ctrl+Hyper").is_err());
        // 空串
        assert!(parse_spec("").is_err());
    }

    #[test]
    fn shift_is_kept_when_combined() {
        let spec = parse_spec("Ctrl+Shift+Esc").expect("解析 Ctrl+Shift+Esc");
        assert_eq!(spec.modifiers, MOD_CTRL | MOD_SHIFT);
        assert_eq!(spec.vk, 0x1B);
        assert_eq!(spec.display(), "Ctrl+Shift+Esc");
    }
}

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::{Input, KeybdInput, MouseInput, Msg, Point};
    use std::sync::atomic::Ordering;

    #[test]
    fn msg_layout_matches_win32() {
        // FFI 结构体布局一旦错位，GetMessageW 会写坏栈内存，所以这里钉死大小：
        // x64 上 Win32 的 MSG 是 48 字节（hwnd 8 + message 4 + 4 填充 + wParam 8
        // + lParam 8 + time 4 + pt 8 + lPrivate 4 + 4 尾部填充）。
        assert_eq!(std::mem::size_of::<Point>(), 8);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(std::mem::size_of::<Msg>(), 48);
        #[cfg(target_pointer_width = "32")]
        assert_eq!(std::mem::size_of::<Msg>(), 28);
    }

    #[test]
    fn input_layout_matches_win32() {
        // SendInput 按 cbSize 步长遍历数组，INPUT 大小错了按键就会乱码/丢失。
        // x64：INPUT = 40（type 4 + 4 填充 + 32），KEYBDINPUT = 24，MOUSEINPUT = 32。
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(std::mem::size_of::<KeybdInput>(), 24);
            assert_eq!(std::mem::size_of::<MouseInput>(), 32);
            assert_eq!(std::mem::size_of::<Input>(), 40);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(std::mem::size_of::<KeybdInput>(), 16);
            assert_eq!(std::mem::size_of::<MouseInput>(), 24);
            assert_eq!(std::mem::size_of::<Input>(), 28);
        }
    }

    #[test]
    fn paste_is_skipped_when_no_target() {
        use super::{PREVIOUS_FOREGROUND, simulate_control_v};
        // 没记录到目标窗口时绝不能发按键，否则 Ctrl+V 会粘到当时的任意窗口里
        PREVIOUS_FOREGROUND.store(0, Ordering::Relaxed);
        assert!(!simulate_control_v());
    }

    #[test]
    fn paste_clears_dead_window_handle() {
        use super::{PREVIOUS_FOREGROUND, simulate_control_v};
        // 无效句柄：IsWindow 返回 0，应当直接放弃并清空记录，免得下次还拿它去切前台
        PREVIOUS_FOREGROUND.store(0x1234_5678, Ordering::Relaxed);
        assert!(!simulate_control_v());
        assert_eq!(PREVIOUS_FOREGROUND.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn shell_window_detection_handles_bad_handle() {
        use super::is_shell_window;
        // 空句柄读不到类名，应当安全地返回 false 而不是崩掉
        assert!(!is_shell_window(std::ptr::null_mut()));
    }
}

/// 失焦是否收起的判定表。
///
/// 这组用例对应真实报障：「点一下标题栏（红框处）面板就没了」「Win 键一按就没了」。
/// 判定本身是纯函数，所以能把这些组合全部钉死，不用真的去点窗口。
#[cfg(test)]
mod blur_tests {
    use super::{BlurSnapshot, should_hide_after_blur};

    /// 只写关心的字段，其余按「什么都没发生」处理
    fn snapshot() -> BlurSnapshot {
        BlurSnapshot {
            visible: true,
            ..BlurSnapshot::default()
        }
    }

    #[test]
    fn hidden_window_stays_hidden() {
        // 已经收起来了：自己 hide() 触发的那次失焦不能再去 hide 一次
        let state = BlurSnapshot::default();
        assert!(!should_hide_after_blur(&state));
    }

    #[test]
    fn real_blur_hides_panel() {
        // 窗口还开着、没常驻、焦点真去了别的程序、鼠标也没按着 → 这才是该收的时机
        assert!(should_hide_after_blur(&snapshot()));
    }

    #[test]
    fn titlebar_click_does_not_hide() {
        // 点标题栏 / 拖边框：窗口仍然在前台，只是系统进了模态循环
        let state = BlurSnapshot {
            foreground_self: true,
            ..snapshot()
        };
        assert!(!should_hide_after_blur(&state));
    }

    #[test]
    fn dragging_window_does_not_hide() {
        // 左键按着（拖动窗口、拖边框缩放）一律不收
        let state = BlurSnapshot {
            button_down: true,
            ..snapshot()
        };
        assert!(!should_hide_after_blur(&state));

        let both = BlurSnapshot {
            foreground_self: true,
            button_down: true,
            ..snapshot()
        };
        assert!(!should_hide_after_blur(&both));
    }

    #[test]
    fn pinned_or_just_pasted_never_hides() {
        // 常驻：按 Win 键、切到别的程序都不收
        let pinned = BlurSnapshot {
            pinned: true,
            ..snapshot()
        };
        assert!(!should_hide_after_blur(&pinned));

        // 刚粘贴完：焦点是被程序交给目标窗口的，面板要留着
        let just_pasted = BlurSnapshot {
            keep_open: true,
            ..snapshot()
        };
        assert!(!should_hide_after_blur(&just_pasted));
    }

    #[test]
    fn fresh_summon_does_not_hide_even_without_focus() {
        // 刚呼出的那一刻若没抢到前台（Windows 前台锁），会立刻收到一次失焦：
        // 保护期内必须不收，否则用户看到的就是「Alt+V 面板闪一下就没了」
        let state = BlurSnapshot {
            open_guard: true,
            foreground_self: false,
            ..snapshot()
        };
        assert!(!should_hide_after_blur(&state));
    }
}
