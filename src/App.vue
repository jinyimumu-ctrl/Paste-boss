<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';

// 与后端 ClipboardItemInfo 对应的类型（按 kind 区分图片/文字）
interface BaseItem {
  id: number;
}

interface ImageItem extends BaseItem {
  kind: 'image';
  width: number;
  height: number;
  thumbnail: string;
}

interface TextItem extends BaseItem {
  kind: 'text';
  preview: string;
  length: number;
}

type ClipboardItem = ImageItem | TextItem;

// 历史条数上限由后端配置决定（设置里可改），前端按同一个数字裁剪本地列表
const historyLimit = ref(20);

// 最新收到的内容排在最前面
const items = ref<ClipboardItem[]>([]);

/** 按 id 去重合并，保持「新的在前」，并按上限裁剪 */
function mergeItems(incoming: ClipboardItem[]) {
  const merged = new Map<number, ClipboardItem>();
  for (const item of items.value) merged.set(item.id, item);
  for (const item of incoming) merged.set(item.id, item);
  items.value = [...merged.values()]
    .sort((a, b) => b.id - a.id)
    .slice(0, historyLimit.value);
}

const itemById = computed(() => new Map(items.value.map((item) => [item.id, item])));

// ---------------------------------------------------------------------------
// 搜索：走后端，能匹配完整文字（不只是卡片上的预览）
// ---------------------------------------------------------------------------
const query = ref('');
// null 表示「没在搜索」，此时显示全部历史
const searchResults = ref<ClipboardItem[] | null>(null);
const displayItems = computed(() => searchResults.value ?? items.value);
const searchInput = ref<HTMLInputElement | null>(null);

let searchTimer: number | undefined;

async function runSearch(needle: string) {
  try {
    const found = await invoke<ClipboardItem[]>('search_items', { query: needle });
    // 结果回来前输入框可能又改了，过期结果直接丢掉
    if (query.value.trim() === needle) searchResults.value = found;
  } catch (err) {
    console.error('搜索剪贴板历史失败', err);
  }
}

watch(query, (value) => {
  window.clearTimeout(searchTimer);
  const needle = value.trim();
  if (!needle) {
    searchResults.value = null;
    return;
  }
  // 输入过程中不必每个字符都查一次
  searchTimer = window.setTimeout(() => void runSearch(needle), 120);
});

// ---------------------------------------------------------------------------
// 多选：Ctrl+点击逐个切换，Shift+点击从起点连选到当前
// ---------------------------------------------------------------------------
const selectedIds = ref<number[]>([]);
// Shift 连选的起点（最近一次 Ctrl+点击 / 第一次选中的那条）
const anchorId = ref<number | null>(null);

function isSelected(id: number) {
  return selectedIds.value.includes(id);
}

/** 选中项在「拼接顺序」里的序号（1 起），和真正粘贴出去的内容顺序一致 */
function pasteOrder(id: number) {
  const index = [...selectedIds.value].sort((a, b) => a - b).indexOf(id);
  return index >= 0 ? index + 1 : null;
}

function clearSelection() {
  selectedIds.value = [];
  anchorId.value = null;
}

function toggleSelection(id: number) {
  selectedIds.value = isSelected(id)
    ? selectedIds.value.filter((selected) => selected !== id)
    : [...selectedIds.value, id];
}

/** 按当前显示顺序，把 fromId 到 toId 之间的所有记录并入选择 */
function extendSelection(fromId: number, toId: number) {
  const order = displayItems.value.map((item) => item.id);
  const from = order.indexOf(fromId);
  const to = order.indexOf(toId);
  if (from < 0 || to < 0) {
    toggleSelection(toId);
    return;
  }
  const [start, end] = from <= to ? [from, to] : [to, from];
  selectedIds.value = [...new Set([...selectedIds.value, ...order.slice(start, end + 1)])];
}

// 操作提示（粘贴成功/失败原因），几秒后自动消失
const notice = ref('');
let noticeTimer: number | undefined;

function flash(message: string) {
  notice.value = message;
  window.clearTimeout(noticeTimer);
  noticeTimer = window.setTimeout(() => {
    notice.value = '';
  }, 2400);
}

const pastedId = ref<number | null>(null);
// 后端是否真的把内容粘贴进了目标窗口；false 表示只复制到了剪贴板
const pastedLabel = ref<'已粘贴' | '已复制'>('已粘贴');

/** 点击卡片：把原始内容（原图 PNG / 完整文字）写回剪贴板，并粘贴到上一个窗口 */
async function pasteItem(item: ClipboardItem) {
  try {
    const pasted = await invoke<boolean>('paste_item', { id: item.id });
    pastedLabel.value = pasted ? '已粘贴' : '已复制';
    pastedId.value = item.id;
  } catch (err) {
    // 失败时保持原状态，具体原因输出到控制台
    console.error('写回剪贴板失败', err);
  }
}

/** 多选粘贴：文字按复制先后拼成一段，图片作为「文件」一次贴出 */
async function pasteSelection() {
  // 按编号由旧到新排序，和卡片上显示的序号一致
  const ids = [...selectedIds.value].sort((a, b) => a - b);
  if (ids.length === 0) return;

  const imageCount = ids.filter((id) => itemById.value.get(id)?.kind === 'image').length;
  const textCount = ids.length - imageCount;

  try {
    const pasted = await invoke<boolean>('paste_items', { ids });
    flash(pasteSummary(pasted, textCount, imageCount));
    pastedId.value = null;
    // 面板粘贴后不会自动收起，选择也保留：可以接着按 Enter 再贴一次，或按 Esc 取消
  } catch (err) {
    // 例如「原图文件已丢失」：后端的说明会直接显示出来
    console.error('多选粘贴失败', err);
    flash(String(err));
  }
}

/** 一句话说明这次都送出去了什么 */
function pasteSummary(pasted: boolean, textCount: number, imageCount: number) {
  const parts: string[] = [];
  if (imageCount > 0) parts.push(`${imageCount} 张图片`);
  if (textCount > 0) parts.push(`${textCount} 段文字`);
  return `${pasted ? '已粘贴' : '已复制到剪贴板'}：${parts.join(' + ')}`;
}

/** 卡片点击：Ctrl 多选、Shift 连选，普通点击直接粘贴 */
function onCardClick(item: ClipboardItem, event: MouseEvent) {
  // 用鼠标点了哪张，键盘光标就跟到哪张，之后按 ↑↓ 接着往下走
  cursorIndex.value = displayItems.value.findIndex((entry) => entry.id === item.id);
  if (event.shiftKey) {
    if (anchorId.value === null) anchorId.value = item.id;
    extendSelection(anchorId.value, item.id);
    return;
  }
  if (event.ctrlKey || event.metaKey) {
    toggleSelection(item.id);
    anchorId.value = item.id;
    return;
  }
  clearSelection();
  void pasteItem(item);
}

// ---------------------------------------------------------------------------
// 键盘光标：↑↓ 在卡片间移动，Home / End 跳到首尾，Space 切换多选
// ---------------------------------------------------------------------------
/** 光标停在第几张卡片上（-1 表示还没用键盘选过） */
const cursorIndex = ref(-1);

/** 让光标所在的卡片滚进可视区：历史一多，选中了却看不见就等于没有 */
function scrollCursorIntoView() {
  const item = displayItems.value[cursorIndex.value];
  if (!item) return;
  // 等这一帧把 class 刷上去再滚，滚动位置才对得上
  requestAnimationFrame(() => {
    document
      .querySelector<HTMLElement>(`[data-item-id="${item.id}"]`)
      ?.scrollIntoView({ block: 'nearest' });
  });
}

/** 光标上下移动一格（不会越界，也不会从没有光标的状态直接跳过第一张） */
function moveCursor(step: number) {
  const list = displayItems.value;
  if (list.length === 0) return;
  const from = cursorIndex.value < 0 ? (step > 0 ? -1 : list.length) : cursorIndex.value;
  cursorIndex.value = Math.min(list.length - 1, Math.max(0, from + step));
  scrollCursorIntoView();
}

/** 光标直接跳到某一张（Home / End 用） */
function jumpCursor(index: number) {
  const list = displayItems.value;
  if (list.length === 0) return;
  cursorIndex.value = Math.min(list.length - 1, Math.max(0, index));
  scrollCursorIntoView();
}

// 列表变短（删记录 / 搜索 / 调小上限）时，光标不能停在已经不存在的卡片上
watch(displayItems, (list) => {
  if (cursorIndex.value >= list.length) cursorIndex.value = list.length - 1;
});

/** 键盘：Enter 粘贴、Esc 逐层退出、↑↓ 选卡片、Home/End 跳首尾、Ctrl+F 搜索、Ctrl+A 全选 */
function onKeydown(event: KeyboardEvent) {
  // 录制快捷键时，所有按键都归录制用（Esc 取消）
  if (recordingHotkey.value) {
    onHotkeyCapture(event);
    return;
  }

  // 设置面板打开时只响应 Esc：关掉面板，不触发粘贴/搜索
  if (settingsOpen.value) {
    if (event.key === 'Escape') closeSettings();
    return;
  }

  const inSearch = document.activeElement === searchInput.value;

  if (event.key === 'Escape') {
    if (query.value) {
      query.value = '';
      return;
    }
    if (selectedIds.value.length > 0 || cursorIndex.value >= 0) {
      clearSelection();
      cursorIndex.value = -1;
      return;
    }
    void invoke('hide_panel');
    return;
  }

  const list = displayItems.value;

  // 方向键 / Home / End 只在没在搜索框里打字时生效，否则会抢掉光标移动
  if (!inSearch && list.length > 0) {
    if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveCursor(1);
      return;
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveCursor(-1);
      return;
    }
    if (event.key === 'Home') {
      event.preventDefault();
      jumpCursor(0);
      return;
    }
    if (event.key === 'End') {
      event.preventDefault();
      jumpCursor(list.length - 1);
      return;
    }
    // 空格切换光标所在卡片的多选状态：不用鼠标也能凑出「贴这几条」
    if (event.key === ' ' && cursorIndex.value >= 0) {
      const item = list[cursorIndex.value];
      if (item) {
        event.preventDefault();
        toggleSelection(item.id);
        anchorId.value = item.id;
        return;
      }
    }
  }

  if (event.key === 'Enter') {
    if (selectedIds.value.length > 0) {
      event.preventDefault();
      void pasteSelection();
      return;
    }
    // 没多选时，Enter 直接贴光标当前那一张
    const item = cursorIndex.value >= 0 ? list[cursorIndex.value] : undefined;
    if (item) {
      event.preventDefault();
      void pasteItem(item);
      return;
    }
  }

  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'f') {
    event.preventDefault();
    searchInput.value?.focus();
    return;
  }

  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'a' && !inSearch) {
    event.preventDefault();
    selectedIds.value = displayItems.value.map((item) => item.id);
    anchorId.value = null;
  }
}

// ---------------------------------------------------------------------------
// 设置面板：版本号 / 呼出快捷键 / 开机自启 / 历史上限 / 数据目录
// ---------------------------------------------------------------------------
/** 与后端 settings::AppStatus / lib.rs 的 AppStatus 对应 */
interface AppStatus {
  version: string;
  autostart: boolean;
  autostart_supported: boolean;
  exe_path: string;
  /** 是否处于「暂停记录」状态 */
  paused: boolean;
  /** 历史条数上限 */
  history_limit: number;
  /** 当前生效的全局快捷键（例如 Alt+V） */
  hotkey: string;
  /** 历史 / 配置 / 日志所在的数据目录 */
  data_dir: string;
  /** 面板是否常驻显示（切窗口 / 按 Win 键都不自动收起） */
  pinned: boolean;
}

/** 设置面板里可选的条数（后端还会把越界值收进 5..=200） */
const HISTORY_LIMITS = [20, 50, 100, 200];

/** 当前配置不在预设里时（例如手改过 config.json），也把它列出来 */
const limitOptions = computed(() => {
  const current = status.value?.history_limit ?? HISTORY_LIMITS[0];
  return HISTORY_LIMITS.includes(current)
    ? HISTORY_LIMITS
    : [...HISTORY_LIMITS, current].sort((a, b) => a - b);
});

const status = ref<AppStatus | null>(null);
const settingsOpen = ref(false);
const autostartBusy = ref(false);
/** 是否暂停记录（托盘菜单也能切，所以单独存一份，跟着事件更新） */
const paused = ref(false);
/** 清空历史正在进行中：防止连点 */
const clearBusy = ref(false);
/** 正在录制新的呼出快捷键 */
const recordingHotkey = ref(false);
/** 录制过程中的提示（例如「已被其它程序占用」） */
const hotkeyError = ref('');
/** 正在提交新快捷键：防止按住组合键时连点 */
const hotkeyBusy = ref(false);
/** 面板是否常驻：开启后点别的窗口 / 按 Win 键都不自动收起 */
const pinned = ref(false);
/** 正在切换常驻：防止连点 */
const pinnedBusy = ref(false);

/** 拉取程序状态：版本号、开机自启、暂停状态、历史条数上限 */
async function loadStatus() {
  try {
    const next = await invoke<AppStatus>('app_status');
    status.value = next;
    paused.value = next.paused;
    pinned.value = next.pinned;
    // 上限可能被改过，本地列表跟着裁一次，和磁盘上的记录保持一致
    historyLimit.value = next.history_limit;
    mergeItems([]);
  } catch (err) {
    console.error('读取程序状态失败', err);
  }
}

/** 打开设置面板时顺便刷新一次状态（自启可能在别处被改过） */
async function openSettings() {
  settingsOpen.value = true;
  await loadStatus();
}

function closeSettings() {
  settingsOpen.value = false;
}

/** 切换开机自启：以后端（注册表）的结果为准，失败就把开关拨回去 */
async function toggleAutostart(event: Event) {
  const enabled = (event.target as HTMLInputElement).checked;
  if (autostartBusy.value) return;
  autostartBusy.value = true;
  try {
    status.value = await invoke<AppStatus>('set_autostart', { enabled });
    flash(enabled ? '已开启开机自启' : '已关闭开机自启');
  } catch (err) {
    console.error('切换开机自启失败', err);
    flash(String(err));
    await loadStatus();
  } finally {
    autostartBusy.value = false;
  }
}

/** 暂停 / 恢复记录（设置面板里的开关） */
async function togglePaused(event: Event) {
  const enabled = (event.target as HTMLInputElement).checked;
  try {
    const applied = await invoke<boolean>('set_paused', { paused: enabled });
    paused.value = applied;
    flash(applied ? '已暂停记录' : '已恢复记录');
  } catch (err) {
    console.error('切换暂停状态失败', err);
    flash(String(err));
    // 开关拨回真实状态
    paused.value = !enabled;
  }
}

/**
 * 打开 / 关闭「常驻显示」。
 *
 * 常驻 = 面板一直待在屏幕上，点到别的程序（包括按 Win 键开开始菜单）也不收起，
 * 只能再按一次呼出快捷键、按 Esc 或点关闭按钮收起。
 */
async function setPinned(next: boolean) {
  if (pinnedBusy.value) return;
  pinnedBusy.value = true;
  try {
    const applied = await invoke<boolean>('set_pinned', { pinned: next });
    pinned.value = applied;
    if (status.value) status.value = { ...status.value, pinned: applied };
    flash(applied ? '已常驻：切换窗口不会自动收起' : '已恢复：点其它窗口自动收起');
  } catch (err) {
    // 后端会说清楚是「没写进配置文件」还是别的原因，本次运行已经生效
    console.error('切换常驻失败', err);
    flash(String(err));
    await loadStatus();
  } finally {
    pinnedBusy.value = false;
  }
}

/** 标题栏上的图钉按钮 */
function togglePinned() {
  void setPinned(!pinned.value);
}

/** 设置面板里的开关 */
function onPinnedSwitch(event: Event) {
  void setPinned((event.target as HTMLInputElement).checked);
}

/** 改历史条数上限：后端立刻按新上限裁剪，并把设置写进 config.json */
async function changeHistoryLimit(event: Event) {
  const limit = Number((event.target as HTMLSelectElement).value);
  try {
    const applied = await invoke<number>('set_history_limit', { limit });
    historyLimit.value = applied;
    if (status.value) status.value = { ...status.value, history_limit: applied };
    // 上限调小时，本地列表也要裁掉多出来的部分
    mergeItems([]);
    if (query.value.trim()) void runSearch(query.value.trim());
    flash(`历史最多保留 ${applied} 条`);
  } catch (err) {
    console.error('修改历史条数失败', err);
    flash(String(err));
    await loadStatus();
  }
}

// ---------------------------------------------------------------------------
// 呼出快捷键：录制 → 校验 → 交给后端注册（后端顺手写进 config.json）
// ---------------------------------------------------------------------------
/** 把 KeyboardEvent 的主按键转成后端认识的名字；null 表示这个键不支持当快捷键 */
function hotkeyKeyName(event: KeyboardEvent): string | null {
  // 用 code 而不是 key：中文输入法、非英文布局下 key 会变，code 始终是物理键位
  const code = event.code;
  if (/^Key[A-Z]$/.test(code)) return code.slice(3);
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
  const named: Record<string, string> = {
    Space: 'Space',
    Tab: 'Tab',
    Enter: 'Enter',
    Escape: 'Esc',
    Backspace: 'Backspace',
    Insert: 'Insert',
    Delete: 'Delete',
    Home: 'Home',
    End: 'End',
    PageUp: 'PageUp',
    PageDown: 'PageDown',
    ArrowUp: 'Up',
    ArrowDown: 'Down',
    ArrowLeft: 'Left',
    ArrowRight: 'Right',
  };
  return named[code] ?? null;
}

/** 把当前按下的组合拼成 `Ctrl+Alt+V`；组合还不完整（比如只按了 Ctrl）时返回 null */
function hotkeyFromEvent(event: KeyboardEvent): string | null {
  const key = hotkeyKeyName(event);
  if (!key) return null;
  // 和系统规则一致：光靠 Shift、或者干脆没有修饰键，系统根本注册不上
  if (!event.ctrlKey && !event.altKey && !event.metaKey) {
    hotkeyError.value = '至少要包含 Ctrl、Alt 或 Win 其中之一';
    return null;
  }
  const parts: string[] = [];
  if (event.ctrlKey) parts.push('Ctrl');
  if (event.altKey) parts.push('Alt');
  if (event.shiftKey) parts.push('Shift');
  if (event.metaKey) parts.push('Win');
  parts.push(key);
  return parts.join('+');
}

function startRecordingHotkey() {
  hotkeyError.value = '';
  recordingHotkey.value = true;
}

function stopRecordingHotkey() {
  recordingHotkey.value = false;
  hotkeyError.value = '';
}

/** 录制状态下收到按键：Esc 取消，其余组合键立刻提交 */
function onHotkeyCapture(event: KeyboardEvent) {
  event.preventDefault();
  event.stopPropagation();
  if (event.key === 'Escape') {
    stopRecordingHotkey();
    return;
  }
  const spec = hotkeyFromEvent(event);
  if (spec) void applyHotkey(spec);
}

/** 提交新快捷键：成功后退出录制并刷新界面；失败（多半是被占用）留在录制状态让用户换一个 */
async function applyHotkey(spec: string) {
  if (hotkeyBusy.value) return;
  hotkeyBusy.value = true;
  try {
    const applied = await invoke<string>('set_hotkey', { spec });
    if (status.value) status.value = { ...status.value, hotkey: applied };
    recordingHotkey.value = false;
    hotkeyError.value = '';
    flash(`呼出快捷键已改为 ${applied}`);
  } catch (err) {
    console.error('修改呼出快捷键失败', err);
    hotkeyError.value = String(err);
  } finally {
    hotkeyBusy.value = false;
  }
}

// ---------------------------------------------------------------------------
// 数据目录 / 日志：出了问题用户得能把现场找出来
// ---------------------------------------------------------------------------
async function openDataDir() {
  try {
    await invoke('open_data_dir');
  } catch (err) {
    console.error('打开数据目录失败', err);
    flash(String(err));
  }
}

async function openLogFile() {
  try {
    await invoke('open_log_file');
  } catch (err) {
    console.error('打开日志文件失败', err);
    flash(String(err));
  }
}

/** 删除一条记录（卡片右下角的 ✕），图片的原图文件由后端一并删掉 */
async function deleteItem(item: ClipboardItem, event: MouseEvent) {
  // 别让这次点击冒泡成「粘贴这一条」
  event.stopPropagation();
  try {
    await invoke('delete_item', { id: item.id });
    items.value = items.value.filter((existing) => existing.id !== item.id);
    if (searchResults.value) {
      searchResults.value = searchResults.value.filter((existing) => existing.id !== item.id);
    }
    selectedIds.value = selectedIds.value.filter((id) => id !== item.id);
    if (pastedId.value === item.id) pastedId.value = null;
    flash('已删除 1 条');
  } catch (err) {
    console.error('删除记录失败', err);
    flash(String(err));
  }
}

/** 清空全部历史：删掉就找不回来了，先让用户确认一次 */
async function clearHistory() {
  if (clearBusy.value) return;
  if (!window.confirm('清空全部剪贴板历史？删除后无法恢复。')) return;
  clearBusy.value = true;
  try {
    await invoke('clear_history');
    items.value = [];
    // 正在搜索结果里清空：结果也一起清掉，避免看到已经删掉的卡片
    searchResults.value = query.value.trim() ? [] : null;
    clearSelection();
    cursorIndex.value = -1;
    pastedId.value = null;
    flash('历史已清空');
  } catch (err) {
    console.error('清空历史失败', err);
    flash(String(err));
  } finally {
    clearBusy.value = false;
  }
}

onMounted(async () => {
  window.addEventListener('keydown', onKeydown);
  // 先把程序状态拿出来，标题旁边的版本号和设置面板才不会一直空着
  void loadStatus();

  // 先开始监听，再拉历史，避免这两步之间产生的内容被漏掉
  await listen<ClipboardItem>('new-item', (event) => {
    console.log('前端收到新内容', event.payload);
    mergeItems([event.payload]);
    // 正在搜索时，新内容也可能命中关键字
    if (query.value.trim()) void runSearch(query.value.trim());
  });

  // 托盘菜单里也能切「暂停记录」，面板上的状态提示跟着变
  await listen<boolean>('pause-changed', (event) => {
    paused.value = event.payload;
  });

  // 托盘菜单「设置…」：后端负责把窗口叫出来，这里只负责把面板打开
  await listen('open-settings', () => {
    void openSettings();
  });

  // 托盘菜单「清空历史」：后端已经清完并通知了，这里同步本地列表
  await listen('history-cleared', () => {
    items.value = [];
    searchResults.value = query.value.trim() ? [] : null;
    clearSelection();
    cursorIndex.value = -1;
    pastedId.value = null;
    flash('历史已清空');
  });

  try {
    const history = await invoke<ClipboardItem[]>('get_items');
    mergeItems(history);
  } catch (err) {
    console.error('拉取剪贴板历史失败', err);
  }
});

onBeforeUnmount(() => {
  window.removeEventListener('keydown', onKeydown);
  window.clearTimeout(searchTimer);
  window.clearTimeout(noticeTimer);
});
</script>

<template>
  <div class="app-container">
    <!-- data-tauri-drag-region：这条空白处可以直接按住拖动窗口
         （系统标题栏太窄，给一块更大的抓取区；搜索框、按钮这些交互元素不加） -->
    <header class="header" data-tauri-drag-region>
      <h1 data-tauri-drag-region>
        📋 Paste-boss
        <span class="version-badge">v{{ status?.version ?? '…' }}</span>
      </h1>
      <input
        ref="searchInput"
        v-model="query"
        class="search"
        type="search"
        placeholder="搜索文字内容… (Ctrl+F)"
      />
      <span class="status" :class="{ paused }" data-tauri-drag-region>
        {{ paused ? '已暂停记录' : '监听中...' }} · {{ items.length }} 条 · Ctrl+点击多选 ·
        {{ status?.hotkey ?? 'Alt+V' }} 呼出
      </span>
      <!-- 常驻：开启后点别的窗口 / 按 Win 键都不收起，只能手动关 -->
      <button
        class="icon-btn pin"
        :class="{ active: pinned }"
        type="button"
        :title="
          pinned
            ? '已常驻：切窗口、按 Win 键都不会收起（点这里恢复自动收起）'
            : '常驻显示：切窗口、按 Win 键都不自动收起'
        "
        :aria-pressed="pinned"
        @click="togglePinned"
      >
        📌
      </button>
      <button
        class="icon-btn"
        type="button"
        title="设置（呼出快捷键 / 开机自启 / 历史 / 数据目录）"
        @click="openSettings"
      >
        ⚙
      </button>
    </header>
    
    <main class="main-content">
      <div v-if="displayItems.length === 0" class="empty-state">
        <p v-if="query.trim()">没有匹配「{{ query.trim() }}」的文字</p>
        <p v-else>
          等待内容... 截一张图或复制一段文字试试（{{ status?.hotkey ?? 'Alt+V' }} 显示/隐藏窗口）
        </p>
      </div>

      <div v-else class="item-grid">
        <article
          v-for="(item, index) in displayItems"
          :key="item.id"
          :data-item-id="item.id"
          class="item-card"
          :class="{
            pasted: pastedId === item.id,
            selected: isSelected(item.id),
            cursor: index === cursorIndex,
          }"
          title="单击直接粘贴 · Ctrl+单击多选 · Shift+单击连选 · ↑↓ 选择、Space 多选、Enter 粘贴"
          @click="onCardClick(item, $event)"
        >
          <span class="index-badge" :class="{ picked: isSelected(item.id) }">
            {{ isSelected(item.id) ? pasteOrder(item.id) : index + 1 }}
          </span>
          <span v-if="pastedId === item.id" class="pasted-flag">{{ pastedLabel }}</span>

          <!-- 悬停时出现的删除按钮：点它不会触发卡片本身的粘贴 -->
          <button
            class="card-delete"
            type="button"
            title="删除这一条"
            @click="deleteItem(item, $event)"
          >
            ✕
          </button>

          <div v-if="item.kind === 'image'" class="card-body">
            <img class="thumb-blur" :src="item.thumbnail" alt="" aria-hidden="true" />
            <img class="thumb" :src="item.thumbnail" :alt="`截图 ${index + 1}`" />
          </div>
          <div v-else class="card-body text-body">
            <p class="text-preview">{{ item.preview }}</p>
          </div>

          <span class="meta">
            <template v-if="item.kind === 'image'">{{ item.width }} × {{ item.height }}</template>
            <template v-else>文字 · {{ item.length }} 字</template>
          </span>
        </article>
      </div>
    </main>

    <footer v-if="selectedIds.length > 0" class="selection-bar">
      <span class="selection-summary">
        已选 {{ selectedIds.length }} 条 · 文字按复制先后拼成一段，图片作为文件一次贴出；
        文字 + 图片混选时先贴文字、再贴图片
      </span>
      <div class="selection-actions">
        <button class="btn primary" type="button" @click="pasteSelection">
          粘贴 {{ selectedIds.length }} 条 (Enter)
        </button>
        <button class="btn" type="button" @click="clearSelection">取消 (Esc)</button>
      </div>
    </footer>

    <!-- 设置面板：版本 / 呼出快捷键 / 开机自启 / 暂停记录 / 历史条数 / 数据目录 / 清空历史 -->
    <div v-if="settingsOpen" class="modal-mask" @click.self="closeSettings">
      <section class="modal">
        <header class="modal-header">
          <h2>设置</h2>
          <button class="icon-btn" type="button" title="关闭 (Esc)" @click="closeSettings">
            ✕
          </button>
        </header>

        <div class="modal-body">
          <div class="setting-row">
            <div class="setting-text">
              <span class="setting-title">版本</span>
            </div>
            <span class="setting-value">v{{ status?.version ?? '—' }}</span>
          </div>

          <!-- 呼出快捷键：点「修改」后直接按下组合键，后端注册成功才算改好 -->
          <div class="setting-row">
            <div class="setting-text">
              <span class="setting-title">呼出快捷键</span>
              <span class="setting-desc" :class="{ error: !!hotkeyError }">
                {{
                  recordingHotkey
                    ? hotkeyError || '按下新的组合键，Esc 取消'
                    : '必须包含 Ctrl / Alt / Win，例如 Ctrl+Alt+V'
                }}
              </span>
            </div>
            <button
              v-if="!recordingHotkey"
              class="btn"
              type="button"
              @click="startRecordingHotkey"
            >
              {{ status?.hotkey ?? 'Alt+V' }} · 修改
            </button>
            <button
              v-else
              class="btn primary"
              type="button"
              :disabled="hotkeyBusy"
              @click="stopRecordingHotkey"
            >
              {{ hotkeyBusy ? '正在注册…' : '录制中 · Esc 取消' }}
            </button>
          </div>

          <label
            class="setting-row"
            :class="{ disabled: !status || !status.autostart_supported }"
          >
            <div class="setting-text">
              <span class="setting-title">开机自启</span>
              <span class="setting-desc">
                {{
                  status && !status.autostart_supported
                    ? '当前平台不支持（仅 Windows）'
                    : `隐藏在托盘，按 ${status?.hotkey ?? 'Alt+V'} 呼出面板`
                }}
              </span>
            </div>
            <input
              class="switch"
              type="checkbox"
              :checked="status?.autostart ?? false"
              :disabled="autostartBusy || !status || !status.autostart_supported"
              @change="toggleAutostart"
            />
          </label>

          <label class="setting-row">
            <div class="setting-text">
              <span class="setting-title">暂停记录</span>
              <span class="setting-desc">暂停期间复制的内容不保存</span>
            </div>
            <input class="switch" type="checkbox" :checked="paused" @change="togglePaused" />
          </label>

          <label class="setting-row" :class="{ disabled: pinnedBusy }">
            <div class="setting-text">
              <span class="setting-title">常驻显示</span>
              <span class="setting-desc">
                开启后切到别的窗口、按 Win 键都不会自动收起
              </span>
            </div>
            <input
              class="switch"
              type="checkbox"
              :checked="pinned"
              :disabled="pinnedBusy"
              @change="onPinnedSwitch"
            />
          </label>

          <div class="setting-row">
            <div class="setting-text">
              <span class="setting-title">历史条数</span>
            </div>
            <select
              class="select"
              :value="status?.history_limit ?? 20"
              @change="changeHistoryLimit"
            >
              <option v-for="limit in limitOptions" :key="limit" :value="limit">
                最多 {{ limit }} 条
              </option>
            </select>
          </div>

          <!-- 数据目录：历史 / 配置 / 日志都在这里，出问题让用户能自己找现场 -->
          <div class="setting-row">
            <div class="setting-text">
              <span class="setting-title">数据目录</span>
              <span class="setting-desc" :title="status?.data_dir || ''">
                {{ status?.data_dir || '—' }}
              </span>
            </div>
            <div class="setting-actions">
              <button class="btn" type="button" @click="openDataDir">打开</button>
              <button class="btn" type="button" @click="openLogFile">日志</button>
            </div>
          </div>

          <div class="setting-row">
            <div class="setting-text">
              <span class="setting-title">清空历史</span>
            </div>
            <button
              class="btn"
              type="button"
              :disabled="clearBusy || items.length === 0"
              @click="clearHistory"
            >
              清空
            </button>
          </div>
        </div>
      </section>
    </div>

    <div v-if="notice" class="notice">{{ notice }}</div>
  </div>
</template>

<style>
/* 基础暗黑风格重置 */
:root {
  background-color: #1a1a1a;
  color: #e5e5e5;
  font-family: 'Segoe UI', Tahoma, Geneva, Verdana, sans-serif;
  margin: 0;
  padding: 0;
}
body {
  margin: 0;
  overflow: hidden; /* 防止出现不必要的滚动条 */
}
.app-container {
  display: flex;
  flex-direction: column;
  height: 100vh;
}
.header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  padding: 16px 24px;
  border-bottom: 1px solid #333;
  background-color: #222;
}
.header h1 {
  flex: 0 0 auto;
  font-size: 16px;
  margin: 0;
  font-weight: 600;
}
/* 搜索框：占满标题和状态之间的空隙 */
.search {
  flex: 1 1 160px;
  min-width: 0;
  padding: 6px 10px;
  border: 1px solid #333;
  border-radius: 6px;
  background-color: #161616;
  color: #e5e5e5;
  font-size: 12px;
  font-family: inherit;
  outline: none;
}
.search:focus {
  border-color: #4ade80;
}
.status {
  font-size: 12px;
  color: #4ade80; /* 绿色状态提示 */
}
/* 暂停记录时状态提示变黄：一眼看出「现在不记录」 */
.status.paused {
  color: #f0b429;
}
.main-content {
  flex: 1;
  padding: 24px;
  overflow-y: auto; /* 允许滚动 */
}
.empty-state {
  display: flex;
  justify-content: center;
  align-items: center;
  height: 100%;
  color: #666;
}
.empty-state p {
  margin: 0;
}
.item-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(170px, 1fr));
  gap: 16px;
  align-content: start;
}
.item-card {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 8px;
  border: 1px solid #333;
  border-radius: 8px;
  background-color: #222;
  cursor: pointer;
  transition: border-color 0.15s ease;
}
.item-card:hover {
  border-color: #4ade80;
}
/* 已写回剪贴板的那一条 */
.item-card.pasted {
  border-color: #4ade80;
}
.pasted-flag {
  position: absolute;
  top: 6px;
  right: 6px;
  z-index: 2;
  height: 20px;
  padding: 0 6px;
  border-radius: 10px;
  background-color: rgba(74, 222, 128, 0.18);
  color: #4ade80;
  font-size: 12px;
  line-height: 20px;
}
.index-badge {
  position: absolute;
  top: 6px;
  left: 6px;
  z-index: 2;
  min-width: 20px;
  height: 20px;
  padding: 0 6px;
  border-radius: 10px;
  background-color: rgba(0, 0, 0, 0.7);
  color: #4ade80;
  font-size: 12px;
  line-height: 20px;
  text-align: center;
}
/* 卡片右下角的删除按钮：平时隐藏，鼠标移到卡片上才出现，避免误点 */
.card-delete {
  position: absolute;
  right: 6px;
  bottom: 6px;
  z-index: 2;
  display: flex;
  align-items: center;
  justify-content: center;
  width: 20px;
  height: 20px;
  padding: 0;
  border: 1px solid #3a3a3a;
  border-radius: 4px;
  background-color: rgba(0, 0, 0, 0.75);
  color: #999;
  font-size: 11px;
  line-height: 1;
  font-family: inherit;
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.15s ease, color 0.15s ease, border-color 0.15s ease;
}
.item-card:hover .card-delete {
  opacity: 1;
}
.card-delete:hover {
  border-color: #f87171;
  color: #f87171;
}
/* 缩略图区域：固定 4:3，保证所有卡片整齐 */
.card-body {
  position: relative;
  box-sizing: border-box;
  width: 100%;
  aspect-ratio: 4 / 3;
  overflow: hidden;
  border-radius: 4px;
  background-color: #111;
}
/* 背景：同一张图放大模糊铺满，避免出现大片空白 */
.thumb-blur {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
  filter: blur(14px) saturate(1.15);
  opacity: 0.5;
  transform: scale(1.15);
}
/* 前景：完整等比显示，不裁剪、不变形 */
.thumb {
  position: relative;
  width: 100%;
  height: 100%;
  object-fit: contain;
}
/* 文字卡片 */
.text-body {
  display: flex;
  padding: 8px;
  background-color: #161616;
}
.text-preview {
  flex: 1;
  min-width: 0;
  margin: 0;
  overflow: hidden;
  display: -webkit-box;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 5;
  word-break: break-all;
  font-size: 12px;
  line-height: 1.5;
  color: #ccc;
}
.meta {
  font-size: 12px;
  color: #888;
  text-align: center;
}
/* 卡片里不要选中文字：Shift+点击是「连选」，不是拖选文本 */
.item-card {
  user-select: none;
}
/* 多选中的卡片 */
.item-card.selected {
  border-color: #4ade80;
  background-color: rgba(74, 222, 128, 0.08);
  box-shadow: inset 0 0 0 1px rgba(74, 222, 128, 0.55);
}
/* 键盘光标所在的卡片：和「已多选」区分开，边框用蓝色，两个状态可以同时存在 */
.item-card.cursor {
  border-color: #60a5fa;
  box-shadow: inset 0 0 0 1px rgba(96, 165, 250, 0.75);
}
.item-card.cursor.selected {
  border-color: #60a5fa;
}
/* 选中的卡片把序号换成「拼接顺序」，看得见就会按预期顺序粘出去 */
.index-badge.picked {
  background-color: #4ade80;
  color: #10231a;
  font-weight: 600;
}
/* 底部多选操作条 */
.selection-bar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 10px 24px;
  border-top: 1px solid #333;
  background-color: #1f2a23;
  color: #9fd4b3;
  font-size: 12px;
}
.selection-actions {
  display: flex;
  flex: 0 0 auto;
  gap: 8px;
}
.btn {
  padding: 6px 12px;
  border: 1px solid #3d5245;
  border-radius: 6px;
  background-color: #253027;
  color: #cdead8;
  font-size: 12px;
  font-family: inherit;
  cursor: pointer;
}
.btn:hover {
  border-color: #4ade80;
}
.btn.primary {
  border-color: #4ade80;
  background-color: #4ade80;
  color: #10231a;
  font-weight: 600;
}
/* 粘贴结果/失败原因的短暂提示 */
.notice {
  position: fixed;
  top: 64px;
  left: 50%;
  z-index: 10;
  transform: translateX(-50%);
  padding: 6px 14px;
  border-radius: 999px;
  background-color: rgba(0, 0, 0, 0.82);
  color: #9fd4b3;
  font-size: 12px;
  pointer-events: none;
}
/* 标题旁边的版本号 */
.version-badge {
  margin-left: 8px;
  padding: 1px 6px;
  border-radius: 999px;
  background-color: rgba(74, 222, 128, 0.14);
  color: #4ade80;
  font-size: 11px;
  font-weight: 600;
  vertical-align: middle;
}
/* 只有图标的小按钮（设置 / 关闭） */
.icon-btn {
  flex: 0 0 auto;
  width: 26px;
  height: 26px;
  padding: 0;
  border: 1px solid #333;
  border-radius: 6px;
  background-color: #161616;
  color: #9fd4b3;
  font-size: 14px;
  font-family: inherit;
  line-height: 1;
  cursor: pointer;
}
.icon-btn:hover {
  border-color: #4ade80;
  color: #4ade80;
}
/* 常驻按钮：默认灰着，开启后高亮成蓝色（和绿色的设置按钮区分开） */
.icon-btn.pin {
  color: #8a8a8a;
}
.icon-btn.pin:hover {
  border-color: #4ca6ff;
  color: #4ca6ff;
}
.icon-btn.pin.active {
  border-color: #4ca6ff;
  color: #4ca6ff;
  background-color: rgba(76, 166, 255, 0.16);
}
/* 设置面板 */
.modal-mask {
  position: fixed;
  inset: 0;
  z-index: 20;
  display: flex;
  align-items: center;
  justify-content: center;
  background-color: rgba(0, 0, 0, 0.55);
}
.modal {
  width: min(420px, calc(100vw - 48px));
  max-height: calc(100vh - 64px);
  overflow-y: auto;
  border: 1px solid #333;
  border-radius: 10px;
  background-color: #1f1f1f;
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.55);
}
.modal-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 16px;
  border-bottom: 1px solid #333;
}
.modal-header h2 {
  margin: 0;
  font-size: 14px;
  font-weight: 600;
}
.modal-body {
  padding: 8px 16px 16px;
}
/* 一行设置：左边说明，右边控件 */
.setting-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 12px 0;
  border-bottom: 1px solid #2a2a2a;
}
.setting-row:last-of-type {
  border-bottom: none;
}
.setting-row.disabled {
  opacity: 0.55;
}
.setting-text {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.setting-title {
  font-size: 13px;
  color: #e5e5e5;
}
.setting-desc {
  font-size: 11px;
  line-height: 1.5;
  color: #888;
  word-break: break-all;
}
/* 录制快捷键时的问题提示（例如组合键已被占用） */
.setting-desc.error {
  color: #f87171;
}
.setting-value {
  flex: 0 0 auto;
  font-size: 13px;
  color: #4ade80;
}
/* 一行里放两个按钮（数据目录：打开 / 日志） */
.setting-actions {
  display: flex;
  flex: 0 0 auto;
  gap: 8px;
}
/* 开关：把 checkbox 画成一个滑块 */
.switch {
  flex: 0 0 auto;
  position: relative;
  width: 38px;
  height: 20px;
  margin: 0;
  appearance: none;
  border: 1px solid #3d5245;
  border-radius: 999px;
  background-color: #253027;
  cursor: pointer;
  transition: background-color 0.15s ease, border-color 0.15s ease;
}
.switch::after {
  content: '';
  position: absolute;
  top: 2px;
  left: 2px;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background-color: #8aa294;
  transition: transform 0.15s ease, background-color 0.15s ease;
}
.switch:checked {
  border-color: #4ade80;
  background-color: rgba(74, 222, 128, 0.28);
}
.switch:checked::after {
  transform: translateX(18px);
  background-color: #4ade80;
}
.switch:disabled {
  cursor: not-allowed;
}
/* 设置面板里的下拉框：和开关、按钮保持同一套暗色风格 */
.select {
  flex: 0 0 auto;
  padding: 4px 8px;
  border: 1px solid #3d5245;
  border-radius: 6px;
  background-color: #253027;
  color: #e5e5e5;
  font-size: 12px;
  font-family: inherit;
  cursor: pointer;
}
.btn:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}
</style>
