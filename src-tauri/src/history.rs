//! 历史落盘：把内存里的剪贴板历史写到磁盘，下次启动再读回来。
//!
//! 磁盘布局（`%APPDATA%\com.tauri-app.paste-boss\`）：
//! - `history.json`：元数据（文字全文、图片尺寸与缩略图），最新的排在最前面；
//! - `blobs\blob-<id>.png`：图片原图，重启后粘贴时按需读取，不占内存。
//!
//! 写盘全部在独立线程里进行，并且只写「最新的一份快照」：
//! 连续复制多份内容时中间那些快照会被直接丢弃，不会反复写盘、也不会拖慢监听线程。

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use super::{ClipboardItemInfo, ImageSource, ImageStore, StoredItem, StoredPayload, text_preview};

/// 历史文件格式版本。以后格式不兼容时递增，读到更高版本就整体忽略（退化成空历史）。
const HISTORY_VERSION: u32 = 1;
/// 历史文件名
const HISTORY_FILE: &str = "history.json";
/// 存放图片原图的子目录
const BLOB_DIR: &str = "blobs";

/// 磁盘上的一条记录
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PersistedEntry {
    Text {
        id: u64,
        /// 完整文字（不是预览），重启后粘贴用的就是它
        text: String,
    },
    Image {
        id: u64,
        width: usize,
        height: usize,
        /// 缩略图 data URL：存下来就不用启动时重新解码原图
        thumbnail: String,
        /// 原图文件名（`blob-<id>.png`），相对 `blobs\` 目录
        blob: String,
    },
}

impl PersistedEntry {
    /// 记录编号，图片和文字共用同一套自增编号
    fn id(&self) -> u64 {
        match self {
            PersistedEntry::Text { id, .. } | PersistedEntry::Image { id, .. } => *id,
        }
    }
}

/// `history.json` 的整体结构
#[derive(Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    items: Vec<PersistedEntry>,
}

/// 历史目录（`%APPDATA%\com.tauri-app.paste-boss`）。
/// 拿不到系统目录时返回 None，历史就只留在内存里（不影响本次运行）。
pub fn data_dir(app: &AppHandle) -> Option<PathBuf> {
    match app.path().app_data_dir() {
        Ok(dir) => Some(dir),
        Err(err) => {
            eprintln!("[history] 取应用数据目录失败，历史不会保存到磁盘: {err}");
            None
        }
    }
}

/// 某条图片记录的原图文件路径
pub fn blob_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(BLOB_DIR).join(blob_name(id))
}

fn blob_name(id: u64) -> String {
    format!("blob-{id}.png")
}

/// 从 `blob-12.png` 还原出编号
fn blob_id(name: &str) -> Option<u64> {
    name.strip_prefix("blob-")?.strip_suffix(".png")?.parse().ok()
}

/// 把内存里的历史拍成一份可以落盘的快照（顺序保持一致：最新在前）
pub fn snapshot(items: &VecDeque<StoredItem>) -> Vec<PersistedEntry> {
    items.iter().filter_map(entry_of).collect()
}

fn entry_of(item: &StoredItem) -> Option<PersistedEntry> {
    match (&item.info, &item.payload) {
        (
            ClipboardItemInfo::Image {
                id,
                width,
                height,
                thumbnail,
            },
            StoredPayload::Image(_),
        ) => Some(PersistedEntry::Image {
            id: *id,
            width: *width,
            height: *height,
            thumbnail: thumbnail.clone(),
            blob: blob_name(*id),
        }),
        (ClipboardItemInfo::Text { id, .. }, StoredPayload::Text(text)) => Some(PersistedEntry::Text {
            id: *id,
            text: text.clone(),
        }),
        // 信息与数据类型对不上（正常流程不会发生），直接跳过
        _ => None,
    }
}

/// 启动写盘线程，返回投递快照的通道。
///
/// 线程只保留队列里最新的那份快照，所以短时间内连续复制多份内容也只写一次盘。
pub fn spawn_writer(dir: PathBuf) -> Sender<Vec<PersistedEntry>> {
    let (sender, receiver) = mpsc::channel::<Vec<PersistedEntry>>();

    let spawn_result = thread::Builder::new()
        .name("history-writer".into())
        .spawn(move || {
            while let Ok(mut entries) = receiver.recv() {
                // 把排队中的旧快照全部丢掉，只写最新的那一份
                while let Ok(newer) = receiver.try_recv() {
                    entries = newer;
                }
                if let Err(err) = save(&dir, &entries) {
                    eprintln!("[history] 历史写入磁盘失败: {err}");
                }
            }
        });

    if let Err(err) = spawn_result {
        eprintln!("[history] 创建写盘线程失败，历史不会保存到磁盘: {err}");
    }

    sender
}

/// 写入 history.json：先写临时文件再改名，避免崩在中间时留下半个 JSON
fn save(dir: &Path, entries: &[PersistedEntry]) -> io::Result<()> {
    fs::create_dir_all(dir.join(BLOB_DIR))?;

    // 原图还没落盘（后台编码中）的图片记录先不写进去，等下一条快照
    let entries: Vec<PersistedEntry> = entries
        .iter()
        .filter(|entry| match entry {
            PersistedEntry::Image { blob, .. } => dir.join(BLOB_DIR).join(blob).is_file(),
            PersistedEntry::Text { .. } => true,
        })
        .cloned()
        .collect();

    let file = HistoryFile {
        version: HISTORY_VERSION,
        items: entries,
    };
    let json = serde_json::to_vec(&file).map_err(io::Error::other)?;

    let temp = dir.join("history.json.tmp");
    fs::write(&temp, &json)?;
    fs::rename(&temp, dir.join(HISTORY_FILE))?;

    prune_blobs(dir, &file.items);
    Ok(())
}

/// 启动时把磁盘历史读回内存仓库，返回恢复的条数。
///
/// 任何一步失败都只记录日志并保留「空历史」，绝不因为历史文件损坏而让程序起不来。
pub fn load_into(store: &mut ImageStore, dir: &Path) -> usize {
    let raw = match fs::read(dir.join(HISTORY_FILE)) {
        Ok(raw) => raw,
        // 第一次运行时没有历史文件，属于正常情况
        Err(err) if err.kind() == io::ErrorKind::NotFound => return 0,
        Err(err) => {
            eprintln!("[history] 读取历史文件失败: {err}");
            return 0;
        }
    };

    let file: HistoryFile = match serde_json::from_slice(&raw) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("[history] 解析历史文件失败，本次从空历史开始: {err}");
            return 0;
        }
    };

    if file.version > HISTORY_VERSION {
        eprintln!(
            "[history] 历史文件版本 {} 高于当前支持的 {HISTORY_VERSION}，已忽略",
            file.version
        );
        return 0;
    }

    let mut restored = Vec::with_capacity(file.items.len());
    for entry in file.items {
        if let Some(item) = restore_entry(dir, entry) {
            restored.push(item);
        }
    }

    let count = restored.len();
    store.restore(restored);
    eprintln!("[history] 从磁盘恢复 {count} 条历史记录");
    count
}

/// 复原一条记录；数据不完整（例如原图文件丢了）时返回 None
fn restore_entry(dir: &Path, entry: PersistedEntry) -> Option<StoredItem> {
    match entry {
        PersistedEntry::Text { id, text } => Some(StoredItem {
            info: ClipboardItemInfo::Text {
                id,
                preview: text_preview(&text),
                length: text.chars().count(),
            },
            payload: StoredPayload::Text(text),
        }),
        PersistedEntry::Image {
            id,
            width,
            height,
            thumbnail,
            blob,
        } => {
            // 防御：只接受纯文件名，不允许用 ../ 跳出 blobs 目录
            if blob.contains('/') || blob.contains('\\') || blob.contains("..") {
                eprintln!("[history] 忽略可疑的原图文件名: {blob}");
                return None;
            }
            let path = dir.join(BLOB_DIR).join(&blob);
            if !path.is_file() {
                eprintln!("[history] 原图丢失，跳过一条图片记录: {blob}");
                return None;
            }
            Some(StoredItem {
                info: ClipboardItemInfo::Image {
                    id,
                    width,
                    height,
                    thumbnail,
                },
                // 原图留在磁盘上，粘贴时再读，避免启动就把几十 MB 图片全塞进内存
                payload: StoredPayload::Image(ImageSource::File(path)),
            })
        }
    }
}

/// 删掉已经被挤出历史（编号比现有最旧一条还小）的原图，避免磁盘越占越多。
///
/// 用「编号小于最小保留编号」判断：编号只会递增，被挤掉的必定是旧编号，
/// 因此绝不会误删正在写入、马上要入队的新原图。
fn prune_blobs(dir: &Path, entries: &[PersistedEntry]) {
    let Some(keep_from) = entries.iter().map(PersistedEntry::id).min() else {
        return;
    };
    let Ok(read_dir) = fs::read_dir(dir.join(BLOB_DIR)) else {
        return;
    };

    for entry in read_dir.flatten() {
        let name = entry.file_name();
        match blob_id(&name.to_string_lossy()) {
            Some(id) if id < keep_from => {
                if let Err(err) = fs::remove_file(entry.path()) {
                    eprintln!("[history] 清理旧原图失败: {err}");
                }
            }
            _ => {}
        }
    }
}

/// 删掉某条图片记录的原图（用户在界面上删掉一张图片时调用）
pub fn delete_blob(dir: &Path, id: u64) {
    let path = blob_path(dir, id);
    match fs::remove_file(&path) {
        Ok(()) => {}
        // 本来就没有（例如这张图还没归档完就被删了）属于正常情况
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("[history] 删除原图失败（{}）: {err}", path.display()),
    }
}

/// 清空全部原图（用户点「清空历史」时调用）
pub fn clear_blobs(dir: &Path) {
    let read_dir = match fs::read_dir(dir.join(BLOB_DIR)) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return,
        Err(err) => {
            eprintln!("[history] 读取原图目录失败: {err}");
            return;
        }
    };

    for entry in read_dir.flatten() {
        if let Err(err) = fs::remove_file(entry.path()) {
            eprintln!("[history] 清空原图失败（{}）: {err}", entry.path().display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每个测试用独立的临时目录，避免并行执行时互相干扰
    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "paste-boss-history-{}-{name}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn memory_image() -> ImageSource {
        ImageSource::Memory(Arc::new(OnceLock::new()))
    }

    #[test]
    fn history_round_trips_through_disk() {
        let dir = temp_dir("roundtrip");

        let mut store = ImageStore::default();
        store.push_text("第一段".to_string());
        store.push_text("第二段".to_string());
        save(&dir, &snapshot(&store.items)).expect("写盘成功");

        let mut restored = ImageStore::default();
        assert_eq!(load_into(&mut restored, &dir), 2);

        // 最新的依然排在最前面，全文也被完整带回来了
        assert_eq!(restored.items.front().map(|item| item.info.id()), Some(2));
        assert_eq!(restored.next_id, 2);
        match &restored.items.front().expect("有恢复的记录").payload {
            StoredPayload::Text(text) => assert_eq!(text, "第二段"),
            _ => panic!("应该是文字记录"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_records_need_their_blob_on_disk() {
        let dir = temp_dir("blob");

        let mut store = ImageStore::default();
        store.push_image(
            4,
            4,
            "data:image/png;base64,AA==".to_string(),
            memory_image(),
        );

        // 原图还没落盘：这条记录不应写进 history.json
        save(&dir, &snapshot(&store.items)).expect("写盘成功");
        let mut restored = ImageStore::default();
        assert_eq!(load_into(&mut restored, &dir), 0);

        // 原图落盘后再写一次，就能恢复出来，并且改成「从磁盘读原图」
        fs::create_dir_all(dir.join(BLOB_DIR)).expect("创建 blobs 目录");
        fs::write(blob_path(&dir, 1), b"fake png").expect("写入假原图");
        save(&dir, &snapshot(&store.items)).expect("写盘成功");

        let mut restored = ImageStore::default();
        assert_eq!(load_into(&mut restored, &dir), 1);
        match &restored.items.front().expect("有恢复的记录").payload {
            StoredPayload::Image(ImageSource::File(path)) => {
                assert_eq!(path, &blob_path(&dir, 1));
            }
            _ => panic!("应该是从磁盘读取原图的图片记录"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn new_items_keep_counting_after_restore() {
        let dir = temp_dir("next-id");

        let mut store = ImageStore::default();
        store.push_text("旧内容".to_string());
        save(&dir, &snapshot(&store.items)).expect("写盘成功");

        let mut restored = ImageStore::default();
        load_into(&mut restored, &dir);
        // 重启后编号接着往下数，不会和磁盘上已有的原图重名
        assert_eq!(restored.push_text("新内容".to_string()).id(), 2);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blobs_of_evicted_items_are_pruned() {
        let dir = temp_dir("prune");
        fs::create_dir_all(dir.join(BLOB_DIR)).expect("创建 blobs 目录");
        fs::write(blob_path(&dir, 1), b"old").expect("写入旧原图");
        fs::write(blob_path(&dir, 9), b"keep").expect("写入新原图");

        let entries = vec![PersistedEntry::Text {
            id: 9,
            text: "最新的文字".to_string(),
        }];
        save(&dir, &entries).expect("写盘成功");

        assert!(
            !blob_path(&dir, 1).exists(),
            "编号 1 已被挤出历史，原图应删除"
        );
        assert!(blob_path(&dir, 9).exists(), "编号 9 仍在历史里，原图必须保留");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_history_file_is_ignored() {
        let dir = temp_dir("broken");
        fs::write(dir.join(HISTORY_FILE), b"{ not json").expect("写入坏文件");

        let mut store = ImageStore::default();
        assert_eq!(load_into(&mut store, &dir), 0);
        assert!(store.items.is_empty());

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blob_name_cannot_escape_blob_dir() {
        let dir = temp_dir("escape");
        let file = HistoryFile {
            version: HISTORY_VERSION,
            items: vec![PersistedEntry::Image {
                id: 1,
                width: 1,
                height: 1,
                thumbnail: String::new(),
                blob: "../evil.png".to_string(),
            }],
        };
        fs::write(
            dir.join(HISTORY_FILE),
            serde_json::to_vec(&file).expect("序列化"),
        )
        .expect("写入历史文件");

        let mut store = ImageStore::default();
        assert_eq!(load_into(&mut store, &dir), 0);

        fs::remove_dir_all(&dir).ok();
    }
}
