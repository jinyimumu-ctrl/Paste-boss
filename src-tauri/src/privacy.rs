//! 什么时候「不该记录」剪贴板内容。
//!
//! 两种情况：
//! 1. 用户手动「暂停记录」——临时不想被记录时的开关。状态只放在内存里：
//!    重启后自动恢复记录，免得用户暂停后忘了打开、反过来以为程序坏了。
//! 2. 剪贴板内容自带「不要记录」标记 —— 密码管理器（1Password / KeePass / Bitwarden）
//!    和部分银行的安全控件在复制密码时会挂上这些格式，任何剪贴板历史工具
//!    看到它们都应当作没看见，否则等于把用户的密码存进了明文 JSON。

use std::sync::atomic::{AtomicBool, Ordering};

/// 是否暂停记录（默认记录）
static PAUSED: AtomicBool = AtomicBool::new(false);

/// 当前是否处于「暂停记录」状态
pub fn paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

/// 设置暂停状态
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
}

/// 切换暂停状态，返回切换后的值
pub fn toggle_paused() -> bool {
    let next = !paused();
    set_paused(next);
    next
}

/// 剪贴板里这份内容是否带「别记录」标记。
///
/// 只查格式是否存在（`IsClipboardFormatAvailable` 不会打开剪贴板，
/// 也就不会和监听线程抢锁），有标记就整份跳过。
#[cfg(target_os = "windows")]
pub fn clipboard_excluded() -> bool {
    use std::sync::OnceLock;

    use clipboard_win::raw::{is_format_avail, register_format};

    /// 业界约定的「不要记录这份内容」标记
    const MARKERS: [&str; 2] = [
        "Clipboard Viewer Ignore",
        "ExcludeClipboardContentFromMonitorProcessing",
    ];

    /// 格式编号只需要注册一次（注册名是全局的，重复注册会拿到同一个编号）
    static IDS: [OnceLock<Option<u32>>; MARKERS.len()] = [const { OnceLock::new() }; MARKERS.len()];

    MARKERS.iter().enumerate().any(|(index, name)| {
        match IDS[index].get_or_init(|| register_format(name).map(|id| id.get())) {
            Some(format) => is_format_avail(*format),
            None => false,
        }
    })
}

/// 非 Windows：没有这套标记格式
#[cfg(not(target_os = "windows"))]
pub fn clipboard_excluded() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_toggle_flips_state() {
        set_paused(false);
        assert!(!paused());
        assert!(toggle_paused());
        assert!(paused());
        assert!(!toggle_paused());
        assert!(!paused());
    }
}
