//! 用户配置（历史条数上限、呼出快捷键、是否常驻），存在 `%APPDATA%\<identifier>\config.json`。
//!
//! 配置和历史文件分开存放：历史是每隔几秒就可能被改写的「数据」，
//! 配置是用户手动做出的「选择」，混在一起容易被历史快照顺手覆盖掉。
//! 读不到或读不懂配置文件时一律退化成默认值 —— 配置坏了也不能让程序起不来。

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 默认保留多少条历史
pub const DEFAULT_HISTORY_LIMIT: usize = 20;
/// 允许设置的最小值：再小就不像「剪贴板历史」了
pub const MIN_HISTORY_LIMIT: usize = 5;
/// 允许设置的最大值：再多卡片列表就翻不动了，内存/磁盘也会明显变大
pub const MAX_HISTORY_LIMIT: usize = 200;
/// 默认的呼出快捷键（和托盘菜单、使用说明里写的保持一致）
pub const DEFAULT_HOTKEY: &str = "Alt+V";

/// 配置文件名
const CONFIG_FILE: &str = "config.json";

/// 用户配置
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// 最多保留多少条历史记录
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
    /// 呼出面板的全局快捷键，例如 `Alt+V`。
    /// 这里只存字符串，合法性由 `hotkey::parse_spec` 判断；
    /// 万一用户手改坏了，启动时会退回默认值，不影响程序起来。
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    /// 面板是否常驻：开启后点别的窗口（包括按 Win 键打开开始菜单）也不会自动收起，
    /// 只有再按一次快捷键、按 Esc 或点关闭才收起来。
    /// 默认关闭，保持「用完就走」的手感；老版本配置里没有这个字段，读出来就是 false。
    #[serde(default)]
    pub pinned: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            history_limit: DEFAULT_HISTORY_LIMIT,
            hotkey: DEFAULT_HOTKEY.to_string(),
            pinned: false,
        }
    }
}

fn default_history_limit() -> usize {
    DEFAULT_HISTORY_LIMIT
}

fn default_hotkey() -> String {
    DEFAULT_HOTKEY.to_string()
}

/// 把任意输入收进允许范围（前端传 0 或 10000 都不会出事）
pub fn clamp_limit(limit: usize) -> usize {
    limit.clamp(MIN_HISTORY_LIMIT, MAX_HISTORY_LIMIT)
}

/// 读取配置；文件不存在或内容损坏时返回默认配置
pub fn load(dir: &Path) -> Config {
    let raw = match fs::read(dir.join(CONFIG_FILE)) {
        Ok(raw) => raw,
        // 第一次运行没有配置文件，属于正常情况
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Config::default(),
        Err(err) => {
            eprintln!("[config] 读取配置文件失败，使用默认配置: {err}");
            return Config::default();
        }
    };

    match serde_json::from_slice::<Config>(&raw) {
        Ok(mut config) => {
            config.history_limit = clamp_limit(config.history_limit);
            config
        }
        Err(err) => {
            eprintln!("[config] 解析配置文件失败，使用默认配置: {err}");
            Config::default()
        }
    }
}

/// 写入配置：先写临时文件再改名，避免写到一半断电留下半个 JSON
pub fn save(dir: &Path, config: &Config) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|err| format!("创建配置目录失败: {err}"))?;
    let json = serde_json::to_vec_pretty(config).map_err(|err| format!("序列化配置失败: {err}"))?;
    let temp = dir.join("config.json.tmp");
    fs::write(&temp, json).map_err(|err| format!("写配置文件失败: {err}"))?;
    fs::rename(&temp, dir.join(CONFIG_FILE)).map_err(|err| format!("保存配置文件失败: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每个测试用独立的临时目录，避免并行执行时互相干扰
    fn temp_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "paste-boss-config-{}-{name}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn clamp_keeps_limit_in_range() {
        assert_eq!(clamp_limit(0), MIN_HISTORY_LIMIT);
        assert_eq!(clamp_limit(1), MIN_HISTORY_LIMIT);
        assert_eq!(clamp_limit(DEFAULT_HISTORY_LIMIT), DEFAULT_HISTORY_LIMIT);
        assert_eq!(clamp_limit(MAX_HISTORY_LIMIT + 1), MAX_HISTORY_LIMIT);
        assert_eq!(clamp_limit(usize::MAX), MAX_HISTORY_LIMIT);
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let dir = temp_dir("missing");
        assert_eq!(load(&dir).history_limit, DEFAULT_HISTORY_LIMIT);
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let config = Config {
            history_limit: 60,
            hotkey: "Ctrl+Alt+Space".to_string(),
            pinned: true,
        };
        save(&dir, &config).expect("保存配置");
        let loaded = load(&dir);
        assert_eq!(loaded.history_limit, 60);
        assert_eq!(loaded.hotkey, "Ctrl+Alt+Space");
        assert!(loaded.pinned);
    }

    #[test]
    fn missing_pinned_falls_back_to_auto_hide() {
        // 老版本配置里没有 pinned 字段：必须默认「不常驻」，保持失焦自动收起
        let dir = temp_dir("no-pinned");
        fs::create_dir_all(&dir).expect("创建临时目录");
        fs::write(dir.join(CONFIG_FILE), br#"{ "history_limit": 30, "hotkey": "Ctrl+Alt+V" }"#)
            .expect("写入配置");
        assert!(!load(&dir).pinned);
    }

    #[test]
    fn missing_hotkey_falls_back_to_default() {
        // 老版本写的 config.json 里没有 hotkey 字段，读出来必须是默认值而不是空串
        let dir = temp_dir("no-hotkey");
        fs::create_dir_all(&dir).expect("创建临时目录");
        fs::write(dir.join(CONFIG_FILE), br#"{ "history_limit": 30 }"#).expect("写入配置");
        assert_eq!(load(&dir).hotkey, DEFAULT_HOTKEY);
    }

    #[test]
    fn broken_file_falls_back_to_defaults() {
        let dir = temp_dir("broken");
        fs::create_dir_all(&dir).expect("创建临时目录");
        fs::write(dir.join(CONFIG_FILE), b"{ not json").expect("写入坏配置");
        assert_eq!(load(&dir).history_limit, DEFAULT_HISTORY_LIMIT);
    }

    #[test]
    fn out_of_range_value_is_clamped_on_load() {
        let dir = temp_dir("clamped");
        fs::create_dir_all(&dir).expect("创建临时目录");
        fs::write(dir.join(CONFIG_FILE), br#"{ "history_limit": 100000 }"#).expect("写入配置");
        assert_eq!(load(&dir).history_limit, MAX_HISTORY_LIMIT);
    }
}
