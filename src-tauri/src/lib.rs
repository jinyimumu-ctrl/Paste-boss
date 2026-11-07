use arboard::{Clipboard, ImageData};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

// 用户配置（历史条数上限），存 config.json
mod config;
// 历史落盘（重启后还能看到之前的记录）
mod history;
// 全局快捷键 + 窗口贴边弹出
mod hotkey;
// 日志落盘 + panic 钩子
mod logging;
// 隐私保护：暂停记录 + 剪贴板的「别记录」标记
mod privacy;
// 设置面板：开机自启（写注册表 Run 键）
mod settings;
// 单实例：避免多开之后互抢 Alt+V
mod single_instance;
// 系统托盘（右下角通知区）
mod tray;

/// 托盘菜单「设置…」发给前端的事件名：让面板把设置弹出来
pub(crate) const OPEN_SETTINGS_EVENT: &str = "open-settings";
/// 托盘菜单「清空历史」发给前端的事件名：让面板把本地列表也清掉
pub(crate) const HISTORY_CLEARED_EVENT: &str = "history-cleared";

/// 剪贴板轮询间隔。现在每轮只查一个序列号（几乎零开销），
/// 所以可以压到 40ms，让复制后几乎立刻出现卡片。
const POLL_INTERVAL: Duration = Duration::from_millis(40);
/// 拿不到剪贴板序列号时（非 Windows）的退化轮询间隔，避免每次都完整读取剪贴板
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// 缩略图最长边（像素）。比卡片显示尺寸大一倍，保证高分屏清晰
const THUMBNAIL_MAX_EDGE: u32 = 320;
/// 文字预览最多显示多少个字符
const TEXT_PREVIEW_CHARS: usize = 80;
/// 点击卡片时等待原图归档的最长时长（20ms × 100 次 = 2 秒）
const ARCHIVE_WAIT_STEP: Duration = Duration::from_millis(20);
const ARCHIVE_WAIT_STEPS: usize = 100;
/// 多选粘贴时文字之间的分隔符。
/// 用 Windows 剪贴板的标准换行，粘到聊天框里就是换行而不是一段连着的话。
const MULTI_PASTE_SEPARATOR: &str = "\r\n";
/// 文字 + 图片混选时分两次粘贴，两次 Ctrl+V 之间留的间隔：
/// 给目标窗口足够时间把第一次的文字插进输入框，再接收第二波图片
const PASTE_STEP_DELAY: Duration = Duration::from_millis(200);
/// 失焦后等多久再复核「是不是真的该收起」。
///
/// Windows 在点标题栏、拖窗口、拖边框缩放、点最小化/最大化时会先发一次失焦
/// （系统进入非客户区的模态循环），但窗口其实还在前台、鼠标还按着。
/// 留这么一小段缓冲，配合 `hotkey::should_hide_after_blur` 就不会误收起。
const BLUR_GRACE: Duration = Duration::from_millis(200);

/// 发给前端的一条剪贴板记录。
/// 图片只带缩略图、文字只带预览，避免事件体积过大。
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ClipboardItemInfo {
    /// 图片
    Image {
        id: u64,
        width: usize,
        height: usize,
        /// `data:image/png;base64,...` 形式的缩略图
        thumbnail: String,
    },
    /// 文字
    Text {
        id: u64,
        /// 压平空白后的开头若干字符
        preview: String,
        /// 原始字符总数
        length: usize,
    },
}

/// 一条图片记录的原图归档：后台线程编码完成后填入 PNG 字节。
/// 用 OnceLock 的好处是读取端无需加锁，并且天然只允许写入一次。
type ImageArchive = Arc<OnceLock<Vec<u8>>>;

/// 图片原图的来源
#[derive(Clone)]
enum ImageSource {
    /// 本次运行中截到的：原图由后台线程编码后放进内存
    Memory(ImageArchive),
    /// 上次运行留下的：原图在磁盘上，用到时再读（不占内存）
    File(PathBuf),
}

impl ImageSource {
    /// 取出原图 PNG 字节
    fn png(&self) -> Result<Vec<u8>, String> {
        match self {
            // 原图由后台线程编码，正常点开时早已就绪
            ImageSource::Memory(archive) => wait_for_archive(archive).cloned(),
            ImageSource::File(path) => std::fs::read(path)
                .map_err(|err| format!("读取原图文件失败（{}）: {err}", path.display())),
        }
    }
}

/// 原始数据，`paste_item` 写回剪贴板时使用
#[derive(Clone)]
enum StoredPayload {
    Image(ImageSource),
    Text(String),
}

/// 内存中的一条历史记录
struct StoredItem {
    info: ClipboardItemInfo,
    payload: StoredPayload,
}

impl ClipboardItemInfo {
    /// 记录编号，图片和文字共用同一套自增编号
    fn id(&self) -> u64 {
        match self {
            ClipboardItemInfo::Image { id, .. } | ClipboardItemInfo::Text { id, .. } => *id,
        }
    }
}

/// 剪贴板历史：id 越大越新，`items` 中最新的排在最前面
struct ImageStore {
    next_id: u64,
    items: VecDeque<StoredItem>,
    /// 最近一次已处理（或由 `paste_item` 写回）的内容指纹，用于去重
    last_signature: Option<u64>,
    /// 把历史写进磁盘的通道；None 表示不做持久化（拿不到用户数据目录时）
    persister: Option<Sender<Vec<history::PersistedEntry>>>,
    /// 最多保留多少条（设置里可改，默认 `config::DEFAULT_HISTORY_LIMIT`）
    limit: usize,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self {
            next_id: 0,
            items: VecDeque::new(),
            last_signature: None,
            persister: None,
            limit: config::DEFAULT_HISTORY_LIMIT,
        }
    }
}

impl ImageStore {
    /// 用磁盘上的历史替换当前内容（启动时调用一次）。
    ///
    /// 编号从最大 id 之后继续，避免新记录和磁盘上已有的原图重名。
    fn restore(&mut self, items: Vec<StoredItem>) {
        self.next_id = items.iter().map(|item| item.info.id()).max().unwrap_or(0);
        self.items = items.into_iter().collect();
    }

    /// 交给仓库一个写盘通道
    fn set_persister(&mut self, sender: Sender<Vec<history::PersistedEntry>>) {
        self.persister = Some(sender);
    }

    /// 把当前历史拍个快照丢给写盘线程。
    /// 只是往通道里塞一份数据，立刻返回，不会阻塞监听线程。
    fn request_save(&self) {
        let Some(sender) = &self.persister else {
            return;
        };
        if sender.send(history::snapshot(&self.items)).is_err() {
            eprintln!("[history] 写盘线程已退出，历史不会再更新到磁盘");
        }
    }

    /// 存入一张图片并返回它的信息。
    /// `archive` 是原图 PNG 的存放位置，由后台线程稍后填入。
    fn push_image(
        &mut self,
        width: usize,
        height: usize,
        thumbnail: String,
        archive: ImageSource,
    ) -> ClipboardItemInfo {
        self.next_id += 1;
        let info = ClipboardItemInfo::Image {
            id: self.next_id,
            width,
            height,
            thumbnail,
        };
        self.push(StoredItem {
            info: info.clone(),
            payload: StoredPayload::Image(archive),
        });
        info
    }

    /// 存入一段文字并返回它的信息
    fn push_text(&mut self, text: String) -> ClipboardItemInfo {
        self.next_id += 1;
        let info = ClipboardItemInfo::Text {
            id: self.next_id,
            preview: text_preview(&text),
            length: text.chars().count(),
        };
        self.push(StoredItem {
            info: info.clone(),
            payload: StoredPayload::Text(text),
        });
        info
    }

    /// 插入到队首，并保证总条数不超过当前上限
    fn push(&mut self, item: StoredItem) {
        self.items.push_front(item);
        self.trim();
    }

    /// 丢弃超出上限的旧记录（改小上限、或从磁盘恢复历史之后都会用到）
    fn trim(&mut self) {
        while self.items.len() > self.limit {
            self.items.pop_back();
        }
    }

    /// 设置新的条数上限，并立刻按上限裁剪
    fn set_limit(&mut self, limit: usize) {
        self.limit = config::clamp_limit(limit);
        self.trim();
    }
}

/// 监听线程与命令处理函数共享的历史仓库
type SharedStore = Arc<Mutex<ImageStore>>;

/// 加锁并在中毒时恢复内部数据，避免某个线程 panic 后整个应用不可用
fn lock_store(store: &SharedStore) -> MutexGuard<'_, ImageStore> {
    store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 读取「最近一次已处理的内容指纹」
fn stored_signature(store: &SharedStore) -> Option<u64> {
    lock_store(store).last_signature
}

/// 记录「最近一次已处理的内容指纹」。
/// `paste_item` 写回剪贴板时也调用它，监听线程就不会把刚写回的内容再存一条。
fn remember_signature(store: &SharedStore, signature: Option<u64>) {
    lock_store(store).last_signature = signature;
}

/// 历史有变化时调用：加锁拍快照并交给写盘线程。
/// 注意不要在持有仓库锁的时候调用，否则会死锁。
fn save_history(store: &SharedStore) {
    lock_store(store).request_save();
}

/// 按关键字搜索历史：大小写不敏感，匹配**完整文字**（不只是卡片上的预览）。
/// 关键字为空时返回全部记录；图片没有可搜索的文字，所以搜索时不会命中。
#[tauri::command]
fn search_items(query: String, store: State<'_, SharedStore>) -> Vec<ClipboardItemInfo> {
    search_store(store.inner(), &query)
}

/// 搜索实现（拆出来便于单元测试）
fn search_store(store: &SharedStore, query: &str) -> Vec<ClipboardItemInfo> {
    let store = lock_store(store);
    let needle = query.trim().to_lowercase();

    store
        .items
        .iter()
        .filter(|item| needle.is_empty() || matches_query(item, &needle))
        .map(|item| item.info.clone())
        .collect()
}

/// 一条记录是否命中关键字（`needle` 已转成小写）
fn matches_query(item: &StoredItem, needle: &str) -> bool {
    match &item.payload {
        StoredPayload::Text(text) => text.to_lowercase().contains(needle),
        // 图片没有可搜索的文字
        StoredPayload::Image(_) => false,
    }
}

/// 前端启动时拉取已有的历史记录（最新的在最前面）
#[tauri::command]
fn get_items(store: State<'_, SharedStore>) -> Vec<ClipboardItemInfo> {
    let store = lock_store(store.inner());
    store.items.iter().map(|item| item.info.clone()).collect()
}

/// 删除一条记录（卡片右上角的 ✕）。图片的原图文件一并删掉，不在磁盘上留垃圾。
#[tauri::command]
fn delete_item(id: u64, app_handle: AppHandle, store: State<'_, SharedStore>) -> Result<(), String> {
    let removed = {
        let mut store = lock_store(store.inner());
        match store.items.iter().position(|item| item.info.id() == id) {
            Some(index) => store.items.remove(index),
            None => None,
        }
    };

    let Some(removed) = removed else {
        return Err(format!("找不到编号为 {id} 的记录"));
    };

    // 图片的原图在 blobs 目录里，跟着记录一起删，否则会一直占着磁盘
    if matches!(removed.payload, StoredPayload::Image(_)) {
        if let Some(dir) = history::data_dir(&app_handle) {
            history::delete_blob(&dir, id);
        }
    }
    save_history(store.inner());
    Ok(())
}

/// 清空全部历史（连同磁盘上的原图）。
///
/// 拆成「实现 + 两个入口」是因为前端命令和托盘菜单都要用：
/// 命令版自己会把列表清掉，托盘版还得额外通知前端。
fn clear_history_inner(app_handle: &AppHandle, store: &SharedStore) {
    {
        let mut store = lock_store(store);
        store.items.clear();
        store.last_signature = None;
    }
    if let Some(dir) = history::data_dir(app_handle) {
        history::clear_blobs(&dir);
    }
    // 写盘线程会把「空历史」写进 history.json
    save_history(store);
}

/// 清空全部历史（面板里点「清空历史」）
#[tauri::command]
fn clear_history(app_handle: AppHandle, store: State<'_, SharedStore>) {
    clear_history_inner(&app_handle, store.inner());
}

/// 清空全部历史（托盘菜单版）。面板可能正开着，所以还要通知它把卡片也清掉。
pub(crate) fn clear_all_history(app_handle: &AppHandle) {
    let store = app_handle.state::<SharedStore>();
    clear_history_inner(app_handle, store.inner());
    if let Err(err) = app_handle.emit(HISTORY_CLEARED_EVENT, ()) {
        eprintln!("[history] 通知前端「历史已清空」失败: {err}");
    }
}

/// 设置历史条数上限（超出的旧记录立刻丢弃），返回实际生效的值
#[tauri::command]
fn set_history_limit(
    limit: usize,
    app_handle: AppHandle,
    store: State<'_, SharedStore>,
) -> Result<usize, String> {
    let limit = config::clamp_limit(limit);
    lock_store(store.inner()).set_limit(limit);

    // 存进配置文件，下次启动还是这个值
    if let Some(dir) = history::data_dir(&app_handle) {
        let mut user_config = config::load(&dir);
        user_config.history_limit = limit;
        config::save(&dir, &user_config)?;
    }
    save_history(store.inner());
    Ok(limit)
}

/// 暂停 / 恢复记录。托盘菜单和设置面板都走这里，两个入口的状态永远一致。
#[tauri::command]
fn set_paused(paused: bool, app_handle: AppHandle) -> bool {
    privacy::set_paused(paused);
    // 托盘菜单里的勾选状态跟着一起变
    tray::sync_paused();
    // 面板如果正开着，状态提示也要跟着变
    if let Err(err) = app_handle.emit("pause-changed", paused) {
        eprintln!("[privacy] 通知前端暂停状态失败: {err}");
    }
    paused
}

/// 把指定记录写回剪贴板，并直接粘贴进「唤出面板前的那个窗口」（前端点击卡片时调用）。
/// 大图解码、切前台、模拟按键都耗时，所以整体丢到阻塞线程池执行，不卡界面。
/// 返回值表示是否真的把内容「送出去」了；false 表示只写进了剪贴板（需要手动 Ctrl+V）。
#[tauri::command]
async fn paste_item(
    id: u64,
    app_handle: AppHandle,
    store: State<'_, SharedStore>,
) -> Result<bool, String> {
    let store = store.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        write_item_to_clipboard(&store, id)?;
        // 写入剪贴板之后再模拟 Ctrl+V，中间不留空窗，用户也就不会粘到旧内容
        Ok(hotkey::paste_into_previous_window(&app_handle))
    })
    .await
    .map_err(|err| format!("粘贴任务异常退出: {err}"))?
}

/// 真正写剪贴板的实现：同步阻塞，只应在后台线程里调用
fn write_item_to_clipboard(store: &SharedStore, id: u64) -> Result<(), String> {
    // 先取出原始数据并释放锁，剪贴板读写不占用历史仓库
    let payload = {
        let store = lock_store(store);
        store
            .items
            .iter()
            .find(|item| item.info.id() == id)
            .map(|item| item.payload.clone())
            .ok_or_else(|| format!("找不到编号为 {id} 的记录"))?
    };

    let mut clipboard = Clipboard::new().map_err(|err| format!("打开剪贴板失败: {err}"))?;

    match payload {
        StoredPayload::Image(source) => {
            // 内存里的原图（本次运行）或磁盘上的原图（上次运行）
            let png = source.png()?;
            // 存的是 PNG，arboard 需要的是 RGBA 原始像素
            let image = image::load_from_memory_with_format(png.as_slice(), image::ImageFormat::Png)
                .map_err(|err| format!("解析原图失败: {err}"))?
                .to_rgba8();
            let (width, height) = image.dimensions();
            let raw = image.into_raw();

            // 先登记指纹再写剪贴板，避免监听线程在这两步之间读到内容而重复记录
            remember_signature(
                store,
                Some(image_signature(
                    width as usize,
                    height as usize,
                    raw.as_slice(),
                )),
            );

            if let Err(err) = clipboard.set_image(ImageData {
                width: width as usize,
                height: height as usize,
                bytes: Cow::Owned(raw),
            }) {
                // 写入失败就清掉指纹，别让监听线程误以为这份内容已处理过
                remember_signature(store, None);
                return Err(format!("写入剪贴板图片失败: {err}"));
            }
        }
        StoredPayload::Text(text) => {
            remember_signature(store, Some(text_signature(&text)));
            if let Err(err) = clipboard.set_text(text) {
                remember_signature(store, None);
                return Err(format!("写入剪贴板文字失败: {err}"));
            }
        }
    }

    Ok(())
}

/// 多选粘贴：把选中的多条记录合成一份内容写回剪贴板，再粘贴进上一个窗口。
///
/// 只选中一条时和单击卡片完全一致（图片也能粘）；多条时文字按复制先后拼成一段，
/// 图片则以「文件列表」的形式写进剪贴板，聊天软件一次就能贴出多张图；
/// 文字和图片混着选时分「先文字、再图片」两步粘贴，两边都不缺。
#[tauri::command]
async fn paste_items(
    ids: Vec<u64>,
    app_handle: AppHandle,
    store: State<'_, SharedStore>,
) -> Result<bool, String> {
    let store = store.inner().clone();
    // 多图粘贴要把图片的真实文件路径交给剪贴板，这里先把历史目录拿好
    // （拿不到就退化成纯文字，不影响本次运行）
    let history_dir = history::data_dir(&app_handle);
    tauri::async_runtime::spawn_blocking(move || {
        let ids = normalize_ids(&ids);
        match ids.len() {
            0 => Err("没有选中任何记录".to_string()),
            // 只有一条：和单击卡片走同一条路径（单张图片会以像素形式写进剪贴板）
            1 => {
                write_item_to_clipboard(&store, ids[0])?;
                Ok(hotkey::paste_into_previous_window(&app_handle))
            }
            _ => {
                let plan = plan_multi_paste(&store, &ids, history_dir.as_deref())?;
                if plan.is_mixed() {
                    Ok(paste_mixed_in_two_steps(&store, &plan, &app_handle))
                } else {
                    write_multi_paste(&store, &plan)?;
                    // 写入剪贴板之后再模拟 Ctrl+V，中间不留空窗
                    Ok(hotkey::paste_into_previous_window(&app_handle))
                }
            }
        }
    })
    .await
    .map_err(|err| format!("粘贴任务异常退出: {err}"))?
}

/// 一次多选粘贴要送出去的内容
#[derive(Debug)]
struct MultiPastePlan {
    /// 选中的文字，按复制先后用换行拼成一段（没选文字时为 None）
    text: Option<String>,
    /// 选中的图片原图（磁盘上的 PNG 路径，按复制先后）
    files: Vec<PathBuf>,
}

impl MultiPastePlan {
    /// 文字和图片都有：同一次 Ctrl+V 目标程序只会挑一种格式，这种组合要分两步贴
    fn is_mixed(&self) -> bool {
        self.text.is_some() && !self.files.is_empty()
    }
}

/// 去重并按编号排序 = 按复制先后（先复制的排在前面）
fn normalize_ids(ids: &[u64]) -> Vec<u64> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// 把选中的记录整理成「要贴出去的内容」（`ids` 需已按 `normalize_ids` 排好序）。
///
/// 图片要先落成磁盘上的 PNG 路径：进剪贴板用的是一个「文件列表」（CF_HDROP），
/// 只认路径，不认内存里的像素。
fn plan_multi_paste(
    store: &SharedStore,
    ids: &[u64],
    history_dir: Option<&Path>,
) -> Result<MultiPastePlan, String> {
    let texts = combine_selected_text(store, ids);
    let images = collect_image_files(store, ids, history_dir);
    // 部分图片没准备好（例如原图正在归档）：其余内容照常贴出去，原因记到日志里
    for reason in &images.skipped {
        eprintln!("[paste] 多选粘贴跳过一张图片：{reason}");
    }

    if texts.is_empty() && images.files.is_empty() {
        return Err(if images.skipped.is_empty() {
            "选中的记录里没有可以粘贴的内容".to_string()
        } else {
            images.skipped.join("；")
        });
    }

    Ok(MultiPastePlan {
        text: if texts.is_empty() {
            None
        } else {
            Some(texts.join(MULTI_PASTE_SEPARATOR))
        },
        files: images.files,
    })
}

/// 把整份内容一次性写进剪贴板：文字和图片文件列表两种格式同时挂着。
fn write_multi_paste(store: &SharedStore, plan: &MultiPastePlan) -> Result<(), String> {
    // 先登记指纹再写剪贴板，避免监听线程把这份拼接结果又存成一条新记录
    remember_signature(store, plan.text.as_deref().map(text_signature));

    let result = match &plan.text {
        // 选中的都是文字：按复制先后拼成一段
        Some(text) if plan.files.is_empty() => write_text_only(text.clone()),
        // 文字 + 多张图片（或只有图片）：两种格式一起挂上去，由目标程序自己挑
        _ => set_text_and_files(plan.text.as_deref(), &plan.files),
    };

    if result.is_err() {
        // 写入失败就清掉指纹，别让监听线程误以为这份内容已处理过
        remember_signature(store, None);
    }
    result
}

/// 文字 + 图片混选：分两次 Ctrl+V。
///
/// Windows 剪贴板允许同时挂「文字」和「文件列表」，但**同一次粘贴目标程序只会挑一种**：
/// 聊天软件一定挑文件，文字就丢了。所以这里
/// 1. 先把剪贴板换成「只有文字」，贴一次 —— 文字进了聊天输入框 / 编辑器；
/// 2. 再把剪贴板换成「只有图片」，贴一次 —— 图片作为附件跟上。
///
/// 反过来在记事本这类只认文字的地方，第二次粘贴没有文字格式可用，什么都不会发生，
/// 所以也不会粘出重复内容。
fn paste_mixed_in_two_steps(
    store: &SharedStore,
    plan: &MultiPastePlan,
    app_handle: &AppHandle,
) -> bool {
    let Some(text) = plan.text.as_deref() else {
        return false;
    };

    // 第一步：只放文字
    remember_signature(store, Some(text_signature(text)));
    if let Err(err) = write_text_only(text.to_string()) {
        eprintln!("[paste] 混选粘贴：写入文字失败，改为一次写入全部内容: {err}");
        remember_signature(store, None);
        if let Err(err) = write_multi_paste(store, plan) {
            eprintln!("[paste] 混选粘贴：写入剪贴板失败: {err}");
        }
        return false;
    }

    if !hotkey::paste_into_previous_window(app_handle) {
        // 文字没送出去（多半是没记录到目标窗口）：恢复成「文字 + 图片」，
        // 用户手动 Ctrl+V 时还能拿到图片
        if let Err(err) = set_text_and_files(plan.text.as_deref(), &plan.files) {
            eprintln!("[paste] 混选粘贴：写入剪贴板失败: {err}");
            remember_signature(store, None);
        }
        return false;
    }

    // 给目标窗口一点时间把文字插进输入框，再换内容，避免两次粘贴挤在一起
    thread::sleep(PASTE_STEP_DELAY);

    // 第二步：剪贴板里只留图片，再贴一次
    if let Err(err) = set_text_and_files(None, &plan.files) {
        // 文字已经贴上了，这里失败只影响图片
        eprintln!("[paste] 混选粘贴：写入图片失败，本次只贴上了文字: {err}");
        return true;
    }
    remember_signature(store, None);
    hotkey::paste_into_previous_window(app_handle);
    true
}

/// 只写一段文字（没有图片时的多选粘贴）
fn write_text_only(text: String) -> Result<(), String> {
    let mut clipboard = Clipboard::new().map_err(|err| format!("打开剪贴板失败: {err}"))?;
    clipboard
        .set_text(text)
        .map_err(|err| format!("写入剪贴板文字失败: {err}"))
}

/// 取出这些编号里的文字（按传入顺序，图片被跳过）
fn combine_selected_text(store: &SharedStore, ids: &[u64]) -> Vec<String> {
    let store = lock_store(store);
    ids.iter()
        .filter_map(|id| store.items.iter().find(|item| item.info.id() == *id))
        .filter_map(|item| match &item.payload {
            StoredPayload::Text(text) => Some(text.clone()),
            StoredPayload::Image(_) => None,
        })
        .collect()
}

/// 多选里挑出来的图片
struct SelectedImages {
    /// 可以放进剪贴板文件列表的真实文件路径（按复制先后）
    files: Vec<PathBuf>,
    /// 没能准备好的图片说明；一张都没成功时直接把这些原因报给用户
    skipped: Vec<String>,
}

/// 把选中的图片收集成「可以直接放进剪贴板文件列表」的磁盘路径。
///
/// 上次运行留下的记录本来就在磁盘上，直接用；本次运行截到的先等后台编码完，
/// 再确保对应的 PNG 真的落在磁盘上（CF_HDROP 只认路径，不认内存里的像素）。
fn collect_image_files(
    store: &SharedStore,
    ids: &[u64],
    history_dir: Option<&Path>,
) -> SelectedImages {
    let mut images = SelectedImages {
        files: Vec::new(),
        skipped: Vec::new(),
    };

    for id in ids {
        let source = {
            let store = lock_store(store);
            match store
                .items
                .iter()
                .find(|item| item.info.id() == *id)
                .map(|item| &item.payload)
            {
                Some(StoredPayload::Image(source)) => source.clone(),
                // 文字记录已经由 combine_selected_text 处理过了
                _ => continue,
            }
        };

        match &source {
            // 上次运行留下的原图：文件本来就在磁盘上
            ImageSource::File(path) => {
                if path.is_file() {
                    images.files.push(path.clone());
                } else {
                    images
                        .skipped
                        .push(format!("第 {id} 条图片的原图文件已丢失"));
                }
            }
            // 本次运行截到的：等后台线程编码完，再确保磁盘上有一份同样的 PNG
            ImageSource::Memory(_) => {
                let png = match source.png() {
                    Ok(png) => png,
                    Err(err) => {
                        images.skipped.push(format!("第 {id} 条图片还没准备好：{err}"));
                        continue;
                    }
                };

                let path = match history_dir {
                    Some(dir) => history::blob_path(dir, *id),
                    // 拿不到应用数据目录时退到系统临时目录，照样能贴出去
                    None => std::env::temp_dir().join(format!("paste-boss-blob-{id}.png")),
                };
                // 后台归档线程编码完就会写这个文件，通常已经存在；没有就自己补一份
                if !path.is_file() {
                    write_blob(&path, &png);
                }

                if path.is_file() {
                    images.files.push(path);
                } else {
                    images
                        .skipped
                        .push(format!("第 {id} 条图片无法写入磁盘，已跳过"));
                }
            }
        }
    }

    images
}

/// 前端按 Esc 收起面板（走命令而不是窗口 API，省得再配一份窗口权限）
#[tauri::command]
fn hide_panel(app_handle: AppHandle) {
    hotkey::hide_window(&app_handle);
}

/// 设置面板要显示的程序状态
#[derive(Serialize)]
struct AppStatus {
    /// 程序版本号（来自 Cargo.toml，界面、安装包、程序本体完全一致）
    version: String,
    /// 当前是否已经登记了开机自启
    autostart: bool,
    /// 当前平台是否支持开机自启（只有 Windows 支持）
    autostart_supported: bool,
    /// 自启时会被拉起的程序路径（方便确认自启的是哪一份 exe）
    exe_path: String,
    /// 是否处于「暂停记录」状态
    paused: bool,
    /// 历史条数上限（设置里可改）
    history_limit: usize,
    /// 当前生效的全局快捷键（例如 `Alt+V`）
    hotkey: String,
    /// 历史 / 配置 / 日志所在的数据目录（设置面板里展示，并可一键打开）
    data_dir: String,
    /// 面板是否常驻显示（开启后切窗口 / 按 Win 键都不自动收起）
    pinned: bool,
}

/// 设置面板：读取版本号、开机自启、暂停状态、历史上限、当前快捷键、数据目录
#[tauri::command]
fn app_status(app_handle: AppHandle, store: State<'_, SharedStore>) -> AppStatus {
    let base = settings::app_status();
    AppStatus {
        version: base.version,
        autostart: base.autostart,
        autostart_supported: base.autostart_supported,
        exe_path: base.exe_path,
        paused: privacy::paused(),
        // 以内存里的值为准：设置刚改完、还没落盘时也能显示正确的数字
        history_limit: lock_store(store.inner()).limit,
        hotkey: hotkey::current().display(),
        // 拿不到目录就留空：界面显示「—」，点「打开」时会给出具体原因
        data_dir: history::data_dir(&app_handle)
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
        pinned: hotkey::pinned(),
    }
}

/// 设置面板：打开 / 关闭开机自启
#[tauri::command]
fn set_autostart(enabled: bool) -> Result<settings::AppStatus, String> {
    settings::set_autostart(enabled)
}

/// 用资源管理器打开一个目录；给了 `select` 就打开所在目录并选中这个文件。
///
/// 直接调 `explorer.exe` 而不是走 frontend 的 opener 插件：
/// 这样不用给前端开放「打开任意路径」的权限，能少一条被滥用的口子。
#[cfg(target_os = "windows")]
fn open_in_explorer(path: &Path, select: Option<&Path>) -> Result<(), String> {
    let mut command = std::process::Command::new("explorer.exe");
    match select {
        // /select,<路径> 是资源管理器的经典参数，逗号后面不能有空格
        Some(file) => {
            command.arg(format!("/select,{}", file.display()));
        }
        None => {
            command.arg(path);
        }
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("打开资源管理器失败: {err}"))
}

/// 非 Windows：没有资源管理器，直接告诉用户路径
#[cfg(not(target_os = "windows"))]
fn open_in_explorer(path: &Path, _select: Option<&Path>) -> Result<(), String> {
    Err(format!("当前平台请手动打开目录：{}", path.display()))
}

/// 在资源管理器里打开数据目录（历史、配置、日志都在这里）。
/// 设置面板的「打开」按钮和托盘菜单的「打开数据目录」共用。
pub(crate) fn reveal_data_dir(app_handle: &AppHandle) -> Result<(), String> {
    let dir = history::data_dir(app_handle).ok_or("拿不到应用数据目录")?;
    open_in_explorer(&dir, None)
}

/// 设置面板：「打开数据目录」
#[tauri::command]
fn open_data_dir(app_handle: AppHandle) -> Result<(), String> {
    reveal_data_dir(&app_handle)
}

/// 设置面板：「日志」—— 在资源管理器里选中日志文件，日志还没生成就打开日志目录
#[tauri::command]
fn open_log_file(app_handle: AppHandle) -> Result<(), String> {
    let Some(log) = logging::log_file(&app_handle) else {
        return Err("拿不到日志目录".to_string());
    };
    match log.parent() {
        Some(dir) if log.is_file() => open_in_explorer(dir, Some(&log)),
        Some(dir) => open_in_explorer(dir, None),
        None => Err(format!("日志路径不合法：{}", log.display())),
    }
}

/// 设置面板：改全局快捷键。
///
/// 注册失败（最常见的是组合键被别的程序占用）会把原因原样带回去，
/// 前端直接提示「已被占用，换一个」，而不是静默变成按了没反应。
#[tauri::command]
fn set_hotkey(spec: String, app_handle: AppHandle) -> Result<String, String> {
    let applied = hotkey::apply(&spec)?;
    let display = applied.display();

    // 托盘菜单上的「显示面板 (Alt+V)」跟着改
    tray::sync_shortcut(&display);

    // 存进配置文件，下次启动还是这个组合键
    if let Some(dir) = history::data_dir(&app_handle) {
        let mut user_config = config::load(&dir);
        user_config.hotkey = display.clone();
        if let Err(err) = config::save(&dir, &user_config) {
            // 注册已经成功，只是没落盘：本次运行有效，下次启动会回到旧值。
            // 这种情况必须说清楚，不能让用户以为设置被记住了。
            eprintln!("[hotkey] 保存快捷键到配置失败: {err}");
            return Err(format!(
                "快捷键已改为 {display}，但没能写入配置文件（{err}），重启后会恢复成原来的组合"
            ));
        }
    }
    Ok(display)
}

/// 设置面板 / 标题栏按钮：切换「常驻显示」。
///
/// 开启后窗口失焦也不收起（按 Win 键、点别的程序都一样），
/// 只有再按一次快捷键、按 Esc 或点关闭按钮才会收起来。
#[tauri::command]
fn set_pinned(pinned: bool, app_handle: AppHandle) -> Result<bool, String> {
    hotkey::set_pinned(pinned);

    // 存进配置文件，下次启动还是这个选择
    if let Some(dir) = history::data_dir(&app_handle) {
        let mut user_config = config::load(&dir);
        user_config.pinned = pinned;
        if let Err(err) = config::save(&dir, &user_config) {
            // 开关已经在内存里生效了，只是没能落盘。这种情况必须说清楚，
            // 不能让用户以为设置被记住了。
            eprintln!("[window] 保存常驻设置到配置失败: {err}");
            let action = if pinned { "开启" } else { "关闭" };
            return Err(format!(
                "已{action}常驻，但没能写入配置文件（{err}），重启后会恢复成原来的设置"
            ));
        }
    }
    Ok(pinned)
}

/// 把「一段文字 + 多张图片（文件列表）」一起写进剪贴板。
///
/// Windows 剪贴板允许同一时刻挂多种格式：聊天软件取文件列表（一次贴出多张图），
/// 文本编辑器取文字，各取所需。arboard 一次只能写一种格式，
/// 所以这里用更底层的 clipboard-win（本来就是 arboard 的依赖）。
#[cfg(target_os = "windows")]
fn set_text_and_files(text: Option<&str>, files: &[PathBuf]) -> Result<(), String> {
    // 整个写入过程要保持剪贴板打开：先写文字（这一步会清空旧内容），
    // 再追加文件列表（不覆盖文字）
    let _clipboard = clipboard_win::Clipboard::new_attempts(10)
        .map_err(|err| format!("打开剪贴板失败: {err}"))?;

    match text {
        Some(text) => clipboard_win::raw::set_string(text)
            .map_err(|err| format!("写入剪贴板文字失败: {err}"))?,
        // 只有图片时也要先清空，否则上一次留在剪贴板里的文字会跟着一起被粘出去
        None => clipboard_win::raw::empty().map_err(|err| format!("清空剪贴板失败: {err}"))?,
    }

    let paths: Vec<String> = files
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    clipboard_win::raw::set_file_list(&paths)
        .map_err(|err| format!("把图片放进剪贴板失败: {err}"))
}

/// 非 Windows：没有 CF_HDROP，多图粘贴暂不支持
#[cfg(not(target_os = "windows"))]
fn set_text_and_files(text: Option<&str>, files: &[PathBuf]) -> Result<(), String> {
    if !files.is_empty() {
        return Err("当前平台暂不支持一次粘贴多张图片".to_string());
    }
    match text {
        Some(text) => write_text_only(text.to_string()),
        None => Err("没有可以写入剪贴板的内容".to_string()),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 多开没有意义：第二个进程会和第一个抢全局快捷键 Alt+V，托盘里还会出现两个图标。
    // 已经有一份在跑时，通知它把面板显示出来，然后自己安静退出。
    if !single_instance::acquire() {
        single_instance::activate_existing();
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            get_items,
            search_items,
            delete_item,
            clear_history,
            set_history_limit,
            set_paused,
            paste_item,
            paste_items,
            hide_panel,
            app_status,
            set_autostart,
            set_hotkey,
            set_pinned,
            open_data_dir,
            open_log_file
        ])
        .setup(|app| {
            // 日志最先初始化：后面每一步出错都要留下记录
            logging::init(app.handle());

            // 历史仓库：监听线程负责写入，get_items / search_items 命令负责读出
            let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));

            // 上次运行留下的历史：启动时同步读回来。
            // 只解析 JSON（图片缩略图已存在里面），不解码原图，所以这一步很快。
            let history_dir = history::data_dir(app.handle());
            // 用户配置（历史条数上限 + 呼出快捷键）只读一次，下面两处共用
            let user_config = match &history_dir {
                Some(dir) => config::load(dir),
                None => config::Config::default(),
            };
            {
                let mut store_guard = lock_store(&store);
                if let Some(dir) = &history_dir {
                    // 恢复历史时直接按上限裁剪
                    store_guard.set_limit(user_config.history_limit);
                    history::load_into(&mut store_guard, dir);
                    store_guard.trim();
                    // 写盘线程拿到通道后，历史一有变化就把快照丢进去
                    store_guard.set_persister(history::spawn_writer(dir.clone()));
                }
            }
            app.manage(store.clone());

            // 拿到一个可以跨线程移动的句柄，交给后台线程使用
            let app_handle = app.handle().clone();
            start_clipboard_watcher(app_handle.clone(), store, history_dir);

            // 全局快捷键（默认 Alt+V，设置里可改）：显示/隐藏窗口（启动时窗口是隐藏的）
            hotkey::start(app_handle, &user_config.hotkey);

            // 「常驻」开关决定失焦要不要收起，启动时按配置装好
            hotkey::set_pinned(user_config.pinned);

            // 第二次启动本程序时，把已经在跑的这份面板呼出来
            single_instance::watch(app.handle().clone());

            // 开机自启自检：之前登记过就把注册表里的路径更新成当前 exe
            settings::heal_autostart();

            // 系统托盘：右下角通知区里的程序图标，
            // 左键单击唤出/收起面板，右键菜单里可以显示面板、暂停记录、退出
            tray::setup(app.handle())?;
            Ok(())
        })
        // 失焦自动隐藏：点其它窗口就收起来，像系统级剪贴板工具那样「用完就走」。
        // 例外：刚粘贴完（焦点是被程序交给目标窗口的）时不收起，
        // 面板留给用户继续看/继续贴，什么时候关由用户自己决定。
        .on_window_event(|window, event| match event {
            // 点右上角的关闭按钮：只收进托盘，程序继续在后台记录剪贴板。
            // 想彻底退出请用托盘菜单里的「退出」，避免用户以为程序关了、其实还在跑。
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                hotkey::hide_window(window.app_handle());
            }
            tauri::WindowEvent::Focused(false) => {
                // 不能在这里直接 hide()：点标题栏、拖窗口、拖边框缩放、点最小化/最大化时，
                // Windows 会先发一次失焦（系统进入非客户区的模态循环），
                // 但窗口还在前台、鼠标还按着 —— 立刻收起的结果就是「一碰标题栏面板就没了、
                // 窗口根本拖不动也缩放不了」。
                // 所以隔一小会儿再复核现场状态，只有真的被别的窗口顶到后面才收起。
                let win = window.clone();
                std::thread::spawn(move || {
                    thread::sleep(BLUR_GRACE);
                    let hide = hotkey::should_hide_after_blur(&hotkey::BlurSnapshot {
                        visible: win.is_visible().unwrap_or(false),
                        keep_open: hotkey::keep_open_on_blur(),
                        pinned: hotkey::pinned(),
                        open_guard: hotkey::open_guard_active(),
                        foreground_self: hotkey::window_is_foreground(&win.app_handle()),
                        button_down: hotkey::mouse_button_down(),
                    });
                    if !hide {
                        return;
                    }
                    eprintln!("[window] 真的失焦了，收起面板");
                    if let Err(err) = win.hide() {
                        eprintln!("[window] 失焦隐藏窗口失败: {err}");
                    }
                });
            }
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

// Win32：剪贴板内容的变更计数，内容一变就 +1。
// 直接声明 FFI，不引入额外的 windows crate 依赖。
#[cfg(target_os = "windows")]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetClipboardSequenceNumber() -> u32;
}

/// 读取当前剪贴板序列号。
///
/// 返回 `None` 表示拿不到（非 Windows，或系统拒绝返回），此时只能每次都完整读剪贴板。
/// Windows 上这个调用不打开剪贴板、不复制数据，开销约等于一次函数调用，
/// 因此可以用 40ms 的频率安全轮询，而不会像读图片那样每轮复制几 MB 像素。
fn clipboard_sequence() -> Option<u32> {
    #[cfg(target_os = "windows")]
    {
        // SAFETY: 该 Win32 函数无参数、无副作用，只返回一个计数
        let sequence = unsafe { GetClipboardSequenceNumber() };
        if sequence != 0 {
            return Some(sequence);
        }
    }
    None
}

/// 启动后台剪贴板监听线程。
///
/// 该线程每 40ms 检查一次剪贴板序列号，只有序列号变化时才真正读取内容：
/// - 有图片时生成缩略图和原图 PNG，存入历史并发送 `new-item` 事件；
/// - 没有图片时读取文字，存入历史并发送 `new-item` 事件（只带预览）。
/// 所有读取错误都只会跳过当前这一轮轮询，保证线程不会因为剪贴板被占用而崩溃。
fn start_clipboard_watcher(app_handle: AppHandle, store: SharedStore, history_dir: Option<PathBuf>) {
    let spawn_result = thread::Builder::new()
        .name("clipboard-watcher".into())
        .spawn(move || {
            // arboard 在 Windows 上只会在真正读写时短暂打开剪贴板，
            // 所以可以长期持有一个 Clipboard 实例；并且所有操作都只在这一个线程里进行。
            let mut clipboard = match Clipboard::new() {
                Ok(clipboard) => clipboard,
                Err(err) => {
                    eprintln!("[clipboard] 初始化剪贴板失败，监听线程退出: {err}");
                    return;
                }
            };

            // 内容指纹统一存放在共享仓库里（`paste_item` 写回剪贴板时也会登记），
            // 监听线程自己只维护「上一轮处理过的序列号」：
            // 序列号没变就完全不碰剪贴板，所以 40ms 轮询也不会持续复制整张图片、空耗 CPU。
            let mut last_sequence: Option<u32> = None;

            loop {
                // 用户点了「暂停记录」：完全不碰剪贴板，但序列号照样跟上，
                // 这样恢复记录之后不会把暂停期间复制的内容补记进来
                if privacy::paused() {
                    last_sequence = clipboard_sequence();
                    thread::sleep(POLL_INTERVAL);
                    continue;
                }

                let sequence = clipboard_sequence();

                // 序列号相同 = 剪贴板内容没变，直接跳过（这是延迟能压到几十毫秒的关键）
                if let (Some(current), Some(previous)) = (sequence, last_sequence) {
                    if current == previous {
                        thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                }

                // 只有这一轮真的处理完了才记住序列号；读取失败（例如剪贴板被占用）
                // 保留旧序列号，下一轮会自动重试同一份内容。
                let handled = 'poll: {
                    // 密码管理器 / 银行安全控件会给剪贴板挂「别记录」标记：
                    // 这类内容一律不入库，连缩略图都不生成
                    if privacy::clipboard_excluded() {
                        eprintln!("[clipboard] 内容带「不要记录」标记，已跳过");
                        remember_signature(&store, None);
                        break 'poll true;
                    }

                    // 先尝试读取图片。关键防线：绝不使用 unwrap()，
                    // 微信截图、其它剪贴板工具会短暂占用剪贴板，出错时直接跳过这一轮即可。
                    match clipboard.get_image() {
                        Ok(image) => {
                            let arboard::ImageData {
                                width,
                                height,
                                bytes,
                            } = image;
                            let signature = image_signature(width, height, bytes.as_ref());
                            // 同一张图仍然留在剪贴板里（或刚被 paste_item 写回），说明没有新内容
                            if stored_signature(&store) == Some(signature) {
                                break 'poll true;
                            }
                            remember_signature(&store, Some(signature));

                            // 卡片出现前唯一必须完成的工作：等比缩略图。
                            // 原图 PNG 编码要慢得多（大截图可到秒级），不放在这条关键路径上。
                            let started = std::time::Instant::now();
                            let thumbnail =
                                match make_thumbnail_data_url(width, height, bytes.as_ref()) {
                                    Ok(thumbnail) => thumbnail,
                                    Err(err) => {
                                        eprintln!("[clipboard] 生成缩略图失败，跳过这张图: {err}");
                                        // 也标记为已处理，否则会按轮询频率反复重试
                                        break 'poll true;
                                    }
                                };
                            let thumbnail_ms = started.elapsed().as_millis();

                            let archive: ImageArchive = Arc::new(OnceLock::new());
                            // 先入库再发事件：前端收到事件后立刻调用 get_items，也一定能查到这条记录
                            let info = lock_store(&store).push_image(
                                width,
                                height,
                                thumbnail,
                                ImageSource::Memory(archive.clone()),
                            );
                            // 原图要落到「和这条记录编号同名」的文件上，先把编号记下来
                            let blob_path = history_dir
                                .as_ref()
                                .map(|dir| history::blob_path(dir, info.id()));
                            emit_item(&app_handle, info);

                            // 归档（原图 PNG）交给后台线程：不阻塞监听线程，也不拖慢下一条内容
                            spawn_archive_encoder(
                                archive,
                                width,
                                height,
                                bytes.into_owned(),
                                blob_path,
                                store.clone(),
                            );
                            eprintln!(
                                "[clipboard] 缩略图耗时 {thumbnail_ms}ms，卡片已发送（{width}×{height}）"
                            );
                            break 'poll true;
                        }
                        // 剪贴板里没有图片，转去尝试文字
                        Err(arboard::Error::ContentNotAvailable) => {}
                        // 剪贴板正被其他程序占用，下一轮再试
                        Err(arboard::Error::ClipboardOccupied) => break 'poll false,
                        // 其它未知错误：记录日志后跳过，不影响后续轮询
                        Err(err) => {
                            eprintln!("[clipboard] 读取剪贴板图片失败，跳过本轮: {err}");
                            break 'poll false;
                        }
                    }

                    // 没有图片时再读取文字；图片和文字都没有，说明剪贴板是空的
                    match clipboard.get_text() {
                        Ok(text) => {
                            // 纯空白内容当作空剪贴板处理
                            if text.trim().is_empty() {
                                remember_signature(&store, None);
                                break 'poll true;
                            }

                            let signature = text_signature(&text);
                            if stored_signature(&store) == Some(signature) {
                                // 同一段文字仍然留在剪贴板里（或刚被 paste_item 写回）
                                break 'poll true;
                            }
                            remember_signature(&store, Some(signature));

                            let info = lock_store(&store).push_text(text);
                            emit_item(&app_handle, info);
                            // 新内容已经发出去了，落盘放到写盘线程里做，不影响感知速度
                            save_history(&store);
                            break 'poll true;
                        }
                        // 剪贴板为空：重置指纹，之后复制同样的内容依然能被识别
                        Err(arboard::Error::ContentNotAvailable) => {
                            remember_signature(&store, None);
                            break 'poll true;
                        }
                        // 剪贴板正被其他程序占用，下一轮再试
                        Err(arboard::Error::ClipboardOccupied) => break 'poll false,
                        // 其它未知错误：记录日志后跳过，不影响后续轮询
                        Err(err) => {
                            eprintln!("[clipboard] 读取剪贴板文字失败，跳过本轮: {err}");
                            break 'poll false;
                        }
                    }
                };

                if handled {
                    last_sequence = sequence;
                }

                // 拿不到序列号（非 Windows）时只能每轮完整读取，退化为较慢的轮询
                thread::sleep(match sequence {
                    Some(_) => POLL_INTERVAL,
                    None => FALLBACK_POLL_INTERVAL,
                });
            }
        });

    if let Err(err) = spawn_result {
        eprintln!("[clipboard] 创建监听线程失败: {err}");
    }
}

/// 计算图片指纹：宽 + 高 + 全部像素字节的哈希。
///
/// 尺寸相同但内容不同的图片会得到不同的指纹；内容完全相同的图片指纹一致，用于去重。
fn image_signature(width: usize, height: usize, bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    // 类型标记，避免图片和文字的指纹撞车
    0u8.hash(&mut hasher);
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// 计算文字指纹，用于「同一段文字留在剪贴板里」的去重
fn text_signature(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    1u8.hash(&mut hasher);
    text.hash(&mut hasher);
    hasher.finish()
}

/// 生成文字预览：折叠所有空白字符，只保留开头 TEXT_PREVIEW_CHARS 个字符。
///
/// 超长文本只处理后缀必要的部分，避免每次轮询都遍历整段文字。
fn text_preview(text: &str) -> String {
    let head: String = text.chars().take(TEXT_PREVIEW_CHARS * 2).collect();
    let collapsed = head.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview: String = collapsed.chars().take(TEXT_PREVIEW_CHARS).collect();
    if text.chars().count() > TEXT_PREVIEW_CHARS {
        preview.push('…');
    }
    preview
}

/// 向前端发送一条新记录；窗口已关闭等情况下只记录日志
fn emit_item(app_handle: &AppHandle, info: ClipboardItemInfo) {
    if let Err(err) = app_handle.emit("new-item", info) {
        eprintln!("[clipboard] 向前端发送 new-item 事件失败: {err}");
    }
}

/// 生成缩略图 data URL。
///
/// 卡片出现的速度只取决于这一步，所以这里只做「缩放 + 小图 PNG 编码」，
/// 原图归档交给 `spawn_archive_encoder`。
fn make_thumbnail_data_url(width: usize, height: usize, rgba: &[u8]) -> Result<String, String> {
    let image = rgba_image(width, height, rgba)?;
    // 缩略图：等比缩小到最长边不超过 THUMBNAIL_MAX_EDGE
    let thumbnail = make_thumbnail(&image);
    let mut thumbnail_png = Vec::new();
    PngEncoder::new(&mut thumbnail_png)
        .write_image(
            thumbnail.as_raw(),
            thumbnail.width(),
            thumbnail.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|err| format!("生成缩略图失败: {err}"))?;

    Ok(format!(
        "data:image/png;base64,{}",
        BASE64_STANDARD.encode(&thumbnail_png)
    ))
}

/// 把 RGBA 像素包成 RgbaImage；尺寸与字节数不匹配时返回错误
fn rgba_image(width: usize, height: usize, rgba: &[u8]) -> Result<image::RgbaImage, String> {
    let width = u32::try_from(width).map_err(|_| "图片宽度超出支持范围".to_string())?;
    let height = u32::try_from(height).map_err(|_| "图片高度超出支持范围".to_string())?;
    // from_raw 在字节数不等于 宽 * 高 * 4 时返回 None
    image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "剪贴板图片数据长度与尺寸不匹配".to_string())
}

/// 把原图编码成 PNG，供点击卡片时写回剪贴板。
///
/// 这是整个流程里最慢的一步（4K 截图可达秒级），因此只在后台线程里调用。
fn encode_original_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let image = rgba_image(width, height, rgba)?;
    let mut png = Vec::new();
    // Fast 压缩 + Up 过滤：体积略大于默认压缩，但编码速度快数倍
    PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Up,
    )
    .write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    )
    .map_err(|err| format!("生成原图 PNG 失败: {err}"))?;
    Ok(png)
}

/// 后台线程：把原图编成 PNG，填进这条记录的归档槽，并顺手写到磁盘。
fn spawn_archive_encoder(
    archive: ImageArchive,
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    blob_path: Option<PathBuf>,
    store: SharedStore,
) {
    let spawn_result = thread::Builder::new()
        .name("image-archiver".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            match encode_original_png(width, height, &rgba) {
                Ok(png) => {
                    let size = png.len();
                    // 先落盘再通知写盘线程：history.json 里引用的原图必须真实存在
                    if let Some(path) = &blob_path {
                        write_blob(path, &png);
                        save_history(&store);
                    }
                    // set 失败说明这条记录已被挤出历史，数据直接丢弃即可
                    let _ = archive.set(png);
                    eprintln!(
                        "[clipboard] 原图 {width}×{height} 归档完成 {size} 字节，耗时 {}ms",
                        started.elapsed().as_millis()
                    );
                }
                Err(err) => eprintln!("[clipboard] 原图归档失败: {err}"),
            }
        });

    if let Err(err) = spawn_result {
        eprintln!("[clipboard] 创建原图归档线程失败: {err}");
    }
}

/// 把原图 PNG 写到磁盘。
///
/// 失败只记日志：内存里那份仍然可用，本次运行不受影响，只是重启后这条图会丢。
fn write_blob(path: &Path, png: &[u8]) {
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            eprintln!("[clipboard] 创建原图目录失败: {err}");
            return;
        }
    }
    if let Err(err) = std::fs::write(path, png) {
        eprintln!("[clipboard] 原图写入磁盘失败（{}）: {err}", path.display());
    }
}

/// 等待后台线程写好的原图 PNG；正常情况下无需等待。
fn wait_for_archive(archive: &ImageArchive) -> Result<&Vec<u8>, String> {
    for _ in 0..ARCHIVE_WAIT_STEPS {
        if let Some(png) = archive.get() {
            return Ok(png);
        }
        thread::sleep(ARCHIVE_WAIT_STEP);
    }
    Err("原图仍在生成，请稍后再点一次".to_string())
}

/// 等比缩小缩略图，最长边不超过 THUMBNAIL_MAX_EDGE。
///
/// 注意不能用 `imageops::thumbnail(image, 320, 320)`：那个函数会把图片**强行拉伸**成
/// 320×320，超宽或超高的截图会因此变形；这里自己按最长边算目标尺寸。
/// 比上限还小的图片保持原尺寸，避免放大后变糊。
fn make_thumbnail(image: &image::RgbaImage) -> image::RgbaImage {
    let (width, height) = image.dimensions();
    if width <= THUMBNAIL_MAX_EDGE && height <= THUMBNAIL_MAX_EDGE {
        return image.clone();
    }

    let ratio = f64::from(THUMBNAIL_MAX_EDGE) / f64::from(width.max(height));
    let target_width = ((f64::from(width) * ratio).round() as u32).max(1);
    let target_height = ((f64::from(height) * ratio).round() as u32).max(1);

    // Lanczos3：缩小时细节保留最好，截图里的文字不会糊成一片
    image::imageops::resize(
        image,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_preview_collapses_whitespace() {
        // 换行、制表符、连续空格都应压成单个空格，首尾空白去掉
        assert_eq!(text_preview("  hello \n\n world\t!"), "hello world !");
    }

    #[test]
    fn text_preview_truncates_long_text() {
        // 按字符（而不是字节）截断，中文不会被切坏
        let text = "中".repeat(TEXT_PREVIEW_CHARS + 10);
        let preview = text_preview(&text);
        assert_eq!(preview.chars().count(), TEXT_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn text_preview_keeps_short_text_intact() {
        assert_eq!(text_preview("短文本"), "短文本");
    }

    #[test]
    fn thumbnail_keeps_aspect_ratio() {
        // 超宽、超高、接近方形的大截图都不能被压扁/拉伸
        for (width, height) in [(487u32, 172u32), (172, 487), (1290, 1113), (3840, 1080)] {
            let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
            let thumbnail = make_thumbnail(&image);

            assert!(thumbnail.width() <= THUMBNAIL_MAX_EDGE);
            assert!(thumbnail.height() <= THUMBNAIL_MAX_EDGE);
            // 最长边必须正好顶到上限，说明没有多余留白
            assert_eq!(
                thumbnail.width().max(thumbnail.height()),
                THUMBNAIL_MAX_EDGE
            );

            let source_ratio = f64::from(width) / f64::from(height);
            let thumbnail_ratio = f64::from(thumbnail.width()) / f64::from(thumbnail.height());
            assert!(
                (source_ratio - thumbnail_ratio).abs() < 0.05,
                "{width}x{height} 的缩略图宽高比变了: {}x{}",
                thumbnail.width(),
                thumbnail.height()
            );
        }
    }

    #[test]
    fn thumbnail_never_upscales_small_images() {
        let image = image::RgbaImage::from_pixel(120, 80, image::Rgba([10, 20, 30, 255]));
        let thumbnail = make_thumbnail(&image);
        assert_eq!(thumbnail.dimensions(), (120, 80));
    }

    /// 手动基准（默认不跑）：分别查看「缩略图」（决定卡片出现速度）和
    /// 「原图归档」（后台线程，不挡卡片）的耗时。
    /// 运行：`cargo test --lib bench_encode_large_image -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_encode_large_image() {
        for (width, height) in [(1920u32, 1080u32), (3840u32, 2160u32), (1290, 1113)] {
            // 伪随机像素：比纯色更接近真实截图，压缩耗时也更接近上限
            let mut seed = 12_345u32;
            let rgba: Vec<u8> = (0..width * height * 4)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                    (seed >> 16) as u8
                })
                .collect();

            let started = std::time::Instant::now();
            make_thumbnail_data_url(width as usize, height as usize, &rgba).expect("缩略图成功");
            println!(
                "{width}×{height} 缩略图（卡片关键路径）耗时 {}ms",
                started.elapsed().as_millis()
            );

            let started = std::time::Instant::now();
            encode_original_png(width as usize, height as usize, &rgba).expect("原图归档成功");
            println!(
                "{width}×{height} 原图归档（后台线程）耗时 {}ms",
                started.elapsed().as_millis()
            );
        }
    }

    #[test]
    fn item_id_works_for_both_kinds() {
        let mut store = ImageStore::default();
        let text = store.push_text("hello".to_string());
        let image = store.push_image(
            4,
            4,
            "data:".to_string(),
            ImageSource::Memory(Arc::new(OnceLock::new())),
        );
        assert_eq!(text.id(), 1);
        assert_eq!(image.id(), 2);
    }

    #[test]
    fn multi_paste_combines_text_in_copy_order() {
        let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));
        {
            let mut guard = lock_store(&store);
            guard.push_text("先复制的".to_string()); // 1
            guard.push_image(
                4,
                4,
                "data:".to_string(),
                ImageSource::Memory(Arc::new(OnceLock::new())),
            ); // 2
            guard.push_text("后复制的".to_string()); // 3
        }

        // 图片没有文字可拼，直接跳过；文字按编号（= 复制先后）排列
        let texts = combine_selected_text(&store, &[1, 2, 3]);
        assert_eq!(texts, vec!["先复制的".to_string(), "后复制的".to_string()]);
        assert_eq!(texts.join(MULTI_PASTE_SEPARATOR), "先复制的\r\n后复制的");
    }
    #[test]
    fn multi_paste_collects_image_files_in_copy_order() {
        // 磁盘上的原图可以直接复用；顺序必须和文字一样按「复制先后」
        let dir = std::env::temp_dir().join(format!("paste-boss-multi-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("blobs")).expect("创建 blobs 目录");
        let first = history::blob_path(&dir, 1);
        let third = history::blob_path(&dir, 3);
        std::fs::write(&first, b"png").expect("写入第一张原图");
        std::fs::write(&third, b"png").expect("写入第三张原图");

        let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));
        {
            let mut guard = lock_store(&store);
            guard.push_image(4, 4, "data:".to_string(), ImageSource::File(first.clone())); // 1
            guard.push_text("中间一段文字".to_string()); // 2
            guard.push_image(4, 4, "data:".to_string(), ImageSource::File(third.clone())); // 3
        }

        let images = collect_image_files(&store, &[1, 2, 3], Some(&dir));
        assert_eq!(images.files, vec![first.clone(), third.clone()]);
        assert!(images.skipped.is_empty(), "文件都在，不应该有跳过提示");

        // 原图被删掉的那条会被跳过，并说明原因（不能让用户以为粘贴成功了）
        std::fs::remove_file(&third).expect("删除第三张原图");
        let images = collect_image_files(&store, &[1, 2, 3], Some(&dir));
        assert_eq!(images.files, vec![first]);
        assert_eq!(images.skipped.len(), 1);
        assert!(images.skipped[0].contains("原图文件已丢失"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn multi_paste_without_selection_is_rejected() {
        let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));
        assert!(plan_multi_paste(&store, &[], None).is_err());
    }

    #[test]
    fn mixed_selection_needs_two_paste_steps() {
        // 文字 + 图片：必须走「先文字、再图片」两步，
        // 否则目标程序只会挑一种格式，文字就被丢掉了
        let dir = std::env::temp_dir().join(format!("paste-boss-mixed-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("blobs")).expect("创建 blobs 目录");
        let blob = history::blob_path(&dir, 1);
        std::fs::write(&blob, b"png").expect("写入原图");

        let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));
        {
            let mut guard = lock_store(&store);
            guard.push_image(4, 4, "data:".to_string(), ImageSource::File(blob)); // 1
            guard.push_text("混选的文字".to_string()); // 2
        }

        // 编号按复制先后排好并去重（前端传过来的顺序不可信）
        assert_eq!(normalize_ids(&[2, 1, 2]), vec![1, 2]);

        let plan = plan_multi_paste(&store, &[1, 2], Some(&dir)).expect("应该能整理出内容");
        assert_eq!(plan.text.as_deref(), Some("混选的文字"));
        assert_eq!(plan.files.len(), 1);
        assert!(plan.is_mixed());

        // 只有文字 / 只有图片时一次就贴完，不需要走两步
        let text_only = plan_multi_paste(&store, &[2], Some(&dir)).expect("只有文字");
        assert!(!text_only.is_mixed());
        let image_only = plan_multi_paste(&store, &[1], Some(&dir)).expect("只有图片");
        assert!(!image_only.is_mixed());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 手动验证（默认不跑）：文字 + 多张图片文件能否同时挂在剪贴板上。
    ///
    /// 会覆盖系统剪贴板，运行前确认没有正在复制的内容。
    /// 运行：`cargo test --lib clipboard_holds_text_and_files -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn clipboard_holds_text_and_files() {
        let dir = std::env::temp_dir().join(format!("paste-boss-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建目录");
        let first = dir.join("one.png");
        let second = dir.join("two.png");
        std::fs::write(&first, b"not a real png").expect("写入第一张");
        std::fs::write(&second, b"not a real png").expect("写入第二张");

        set_text_and_files(Some("paste-boss 多图测试"), &[first.clone(), second.clone()])
            .expect("写入剪贴板");

        let mut clipboard = Clipboard::new().expect("打开剪贴板");
        // 编辑器拿到文字……
        assert_eq!(
            clipboard.get_text().expect("读取剪贴板文字"),
            "paste-boss 多图测试"
        );
        // ……聊天软件拿到多张图片文件，两者互不覆盖
        let files = clipboard.get().file_list().expect("读取剪贴板文件列表");
        assert_eq!(files, vec![first, second]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 手动验证（默认不跑）：混选粘贴第二步只写图片时，剪贴板里不能再残留文字，
    /// 否则在记事本这类只认文字的地方会粘出两遍文字。会覆盖系统剪贴板。
    /// 运行：`cargo test --lib clipboard_files_only_drops_text -- --ignored --nocapture`
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn clipboard_files_only_drops_text() {
        let dir = std::env::temp_dir().join(format!("paste-boss-clip2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建目录");
        let file = dir.join("only.png");
        std::fs::write(&file, b"not a real png").expect("写入文件");

        write_text_only("第一步的文字".to_string()).expect("写入文字");
        set_text_and_files(None, &[file.clone()]).expect("只写图片");

        let mut clipboard = Clipboard::new().expect("打开剪贴板");
        assert!(
            matches!(clipboard.get_text(), Err(arboard::Error::ContentNotAvailable)),
            "只写图片时不应残留文字"
        );
        let files = clipboard.get().file_list().expect("读取剪贴板文件列表");
        assert_eq!(files, vec![file]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn multi_paste_reports_images_that_are_not_on_disk() {
        // 选了多张图但原图全都丢了：要报出原因，而不是悄悄写进一段空文字
        let missing = std::env::temp_dir().join("paste-boss-绝对不会存在的图片.png");
        let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));
        {
            let mut guard = lock_store(&store);
            guard.push_image(4, 4, "data:".to_string(), ImageSource::File(missing.clone()));
            guard.push_image(4, 4, "data:".to_string(), ImageSource::File(missing));
        }

        let err = plan_multi_paste(&store, &[1, 2], None).expect_err("应该报错");
        assert!(err.contains("原图文件已丢失"), "实际错误信息：{err}");
    }



    #[test]
    fn search_matches_text_beyond_the_preview() {
        let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));
        {
            let mut guard = lock_store(&store);
            // 关键字远在预览截断之后，只有搜全文才能命中
            guard.push_text(format!("{}尾巴上的关键词", "铺垫".repeat(TEXT_PREVIEW_CHARS)));
            guard.push_image(
                4,
                4,
                "data:".to_string(),
                ImageSource::Memory(Arc::new(OnceLock::new())),
            );
            guard.push_text("Hello World".to_string());
        }

        let found = search_store(&store, "关键词");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id(), 1);

        // 大小写不敏感；图片不参与文字搜索
        let found = search_store(&store, "  hello  ");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id(), 3);

        // 关键字为空时返回全部（包括图片）
        assert_eq!(search_store(&store, "   ").len(), 3);
        assert_eq!(search_store(&store, "不存在的词").len(), 0);
    }

    #[test]
    fn archive_returns_written_original() {
        // 后台线程写入后，click 取原图应立刻拿到数据
        let archive: ImageArchive = Arc::new(OnceLock::new());
        archive.set(vec![1, 2, 3]).expect("首次写入成功");
        assert_eq!(
            wait_for_archive(&archive).expect("已就绪"),
            &vec![1, 2, 3]
        );
    }

    #[test]
    fn pasted_signature_is_remembered() {
        // paste_item 写回剪贴板后会登记指纹，监听线程据此跳过重复记录
        let store: SharedStore = Arc::new(Mutex::new(ImageStore::default()));
        assert_eq!(stored_signature(&store), None);

        remember_signature(&store, Some(42));
        assert_eq!(stored_signature(&store), Some(42));

        remember_signature(&store, None);
        assert_eq!(stored_signature(&store), None);
    }

    #[test]
    fn store_keeps_newest_first_and_caps_history() {
        let mut store = ImageStore::default();
        for i in 0..(config::DEFAULT_HISTORY_LIMIT + 5) {
            store.push_text(format!("第 {i} 条"));
        }

        assert_eq!(store.items.len(), config::DEFAULT_HISTORY_LIMIT);
        // 最新的排在最前面，最旧的 5 条已被丢弃
        match store.items.front().map(|item| &item.info) {
            Some(ClipboardItemInfo::Text { preview, .. }) => {
                assert_eq!(preview, &format!("第 {} 条", config::DEFAULT_HISTORY_LIMIT + 4))
            }
            _ => panic!("队首应该是最新存入的文字记录"),
        }
        match store.items.back().map(|item| &item.info) {
            Some(ClipboardItemInfo::Text { preview, .. }) => assert_eq!(preview, "第 5 条"),
            _ => panic!("队尾应该是最旧的文字记录"),
        }
    }

    #[test]
    fn set_limit_trims_history_and_clamps_input() {
        let mut store = ImageStore::default();
        for i in 0..(config::MIN_HISTORY_LIMIT * 2) {
            store.push_text(format!("第 {i} 条"));
        }

        store.set_limit(config::MIN_HISTORY_LIMIT);
        assert_eq!(store.items.len(), config::MIN_HISTORY_LIMIT);

        // 越界的值会被收进允许范围，不会出现 0 条这种「历史永远为空」的状态
        store.set_limit(0);
        assert_eq!(store.limit, config::MIN_HISTORY_LIMIT);
    }
}
