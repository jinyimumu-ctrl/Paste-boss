//! 系统托盘：面板收起（或窗口被关掉）之后，从这里把程序找回来。
//!
//! 左键单击 = 显示/收起面板（和全局快捷键同一个动作），
//! 右键 = 菜单（显示面板 / 暂停记录 / 打开数据目录 / 清空历史 / 设置 / 退出）。
//! 启动时窗口是隐藏的，托盘图标就是用户唯一能「看见」这个程序的入口。

use std::sync::OnceLock;

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter};

/// 托盘图标 id
const TRAY_ID: &str = "paste-boss-tray";
/// 菜单项 id
const MENU_SHOW: &str = "show-panel";
const MENU_PAUSE: &str = "pause-recording";
const MENU_OPEN_DIR: &str = "open-data-dir";
const MENU_CLEAR: &str = "clear-history";
const MENU_SETTINGS: &str = "open-settings";
const MENU_QUIT: &str = "quit";

/// 托盘里「暂停记录」这一项的句柄。
///
/// `TrayIcon` 只能设置菜单、不能取回菜单，所以这里自己留一份，
/// 设置面板切换暂停状态时才能把勾选状态同步过来。
static PAUSE_ITEM: OnceLock<CheckMenuItem<tauri::Wry>> = OnceLock::new();

/// 「显示面板 (Alt+V)」这一项的句柄：快捷键改了之后文字也要跟着改，
/// 否则菜单上写的还是旧的组合键。
static SHOW_ITEM: OnceLock<MenuItem<tauri::Wry>> = OnceLock::new();

/// 托盘图标：优先用 exe 里已经嵌好的应用图标（和资源管理器里看到的图标一致），
/// 拿不到时退回打包进二进制的 32×32 PNG，
/// 这样发布包只有 exe 一个文件也能显示出真正的程序图标。
fn tray_icon(app_handle: &AppHandle) -> Option<Image<'static>> {
    app_handle
        .default_window_icon()
        .map(|icon| icon.clone().to_owned())
        .or_else(|| {
            let png = include_bytes!("../icons/32x32.png");
            let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
                .ok()?
                .to_rgba8();
            let (width, height) = decoded.dimensions();
            Some(Image::new_owned(decoded.into_raw(), width, height))
        })
}

/// 创建托盘图标并挂上菜单。
///
/// 托盘图标不依赖任何窗口，窗口隐藏时它依然在通知区里。
pub fn setup(app_handle: &AppHandle) -> tauri::Result<()> {
    // 菜单文字里的组合键跟着真实设置走（用户可能已经在设置里改过）
    let show_item = MenuItem::with_id(
        app_handle,
        MENU_SHOW,
        format!("显示面板 ({})", crate::hotkey::current().display()),
        true,
        None::<&str>,
    )?;
    // 勾选状态就是「是否暂停记录」：托盘和设置面板共用同一个状态，两边永远一致
    let pause_item = CheckMenuItem::with_id(
        app_handle,
        MENU_PAUSE,
        "暂停记录",
        true,
        crate::privacy::paused(),
        None::<&str>,
    )?;
    let open_dir_item =
        MenuItem::with_id(app_handle, MENU_OPEN_DIR, "打开数据目录", true, None::<&str>)?;
    let clear_item = MenuItem::with_id(app_handle, MENU_CLEAR, "清空历史", true, None::<&str>)?;
    let settings_item = MenuItem::with_id(app_handle, MENU_SETTINGS, "设置…", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app_handle, MENU_QUIT, "退出", true, None::<&str>)?;

    // 两条分隔线：把「常用操作」和「管理 / 退出」分开，误点「退出」的概率会低很多
    let separator_top = PredefinedMenuItem::separator(app_handle)?;
    let separator_bottom = PredefinedMenuItem::separator(app_handle)?;

    let menu = Menu::with_items(
        app_handle,
        &[
            &show_item,
            &pause_item,
            &separator_top,
            &open_dir_item,
            &clear_item,
            &separator_bottom,
            &settings_item,
            &quit_item,
        ],
    )?;
    // 留一份句柄：设置面板切暂停、改快捷键时用它同步
    let _ = PAUSE_ITEM.set(pause_item.clone());
    let _ = SHOW_ITEM.set(show_item.clone());

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip("Paste-boss · 全局快捷键呼出面板")
        .menu(&menu)
        // 左键留给「显示/收起面板」，菜单只在右键弹出来
        .show_menu_on_left_click(false)
        .on_menu_event(|app_handle, event| match event.id.as_ref() {
            MENU_SHOW => crate::hotkey::toggle_window(app_handle),
            MENU_PAUSE => {
                let paused = crate::privacy::toggle_paused();
                // 系统的勾选状态可能已经自己翻转了，这里以真实状态为准再刷一遍
                sync_paused();
                if let Err(err) = app_handle.emit("pause-changed", paused) {
                    eprintln!("[tray] 通知前端暂停状态失败: {err}");
                }
                eprintln!("[tray] 记录{}", if paused { "已暂停" } else { "已恢复" });
            }
            MENU_OPEN_DIR => {
                if let Err(err) = crate::reveal_data_dir(app_handle) {
                    eprintln!("[tray] 打开数据目录失败: {err}");
                }
            }
            MENU_CLEAR => {
                // 托盘里点「清空历史」没有二次确认的机会，删了就是删了 ——
                // 菜单项本身写着「清空历史」，点它就是想清空，这里不再弹窗
                crate::clear_all_history(app_handle);
            }
            MENU_SETTINGS => {
                // 设置面板在窗口里：先把窗口叫出来，再通知前端打开面板
                crate::hotkey::show_window(app_handle);
                if let Err(err) = app_handle.emit(crate::OPEN_SETTINGS_EVENT, ()) {
                    eprintln!("[tray] 通知前端打开设置失败: {err}");
                }
            }
            MENU_QUIT => quit(app_handle),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 只认「左键松开」这一下。双击在 Windows 上还会额外发一个 DoubleClick 事件，
            // 这里刻意不处理，免得一次双击把面板开关两次。
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::hotkey::toggle_window(tray.app_handle());
            }
        });

    if let Some(icon) = tray_icon(app_handle) {
        builder = builder.icon(icon);
    }

    builder.build(app_handle)?;
    Ok(())
}

/// 让托盘菜单里「暂停记录」的勾选状态和真实状态保持一致。
///
/// 设置面板也能切换暂停，两个入口必须同步 —— 否则会出现
/// 「设置里写着已暂停，托盘菜单却没打勾」这种自相矛盾的状态。
pub fn sync_paused() {
    let Some(item) = PAUSE_ITEM.get() else {
        return;
    };
    if let Err(err) = item.set_checked(crate::privacy::paused()) {
        eprintln!("[tray] 更新「暂停记录」勾选状态失败: {err}");
    }
}

/// 快捷键改了之后，把菜单里的「显示面板 (Alt+V)」同步成新的组合键。
///
/// 不刷新的话，用户改完快捷键回头一看托盘菜单还写着 Alt+V，
/// 会以为设置没生效。
pub fn sync_shortcut(display: &str) {
    let Some(item) = SHOW_ITEM.get() else {
        return;
    };
    if let Err(err) = item.set_text(format!("显示面板 ({display})")) {
        eprintln!("[tray] 更新「显示面板」文字失败: {err}");
    }
}

/// 托盘菜单里的「退出」：真正结束进程。
///
/// 窗口右上角的关闭按钮只是把窗口收进托盘（见 `lib.rs` 的 CloseRequested 处理），
/// 所以这里才是唯一的退出入口，免得用户以为程序停了、其实后台还在记录剪贴板。
fn quit(app_handle: &AppHandle) {
    app_handle.exit(0);
}
