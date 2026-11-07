# Paste-boss

Windows 上的剪贴板历史与多选粘贴工具。默认按 `Alt+V` 呼出面板（可在设置里改），点一下卡片就粘进刚才那个窗口。

技术栈：Tauri 2 + Vue 3 + Rust。程序常驻托盘，纯本地运行，不联网。

## 功能

- **自动记录**：复制的文字和截图自动入库，最新在最前，条数上限可调（默认 20，范围 5–200）。
- **一键粘贴**：单击卡片 = 把原内容写回剪贴板并直接 `Ctrl+V`，不用再切窗口、再按一次。
- **多选粘贴**：`Ctrl+点击` 多选、`Shift+点击` 连选，`Enter` 一次性贴出多条；
  文字按复制先后拼成一段，图片作为「文件列表」一次贴出，文字 + 图片混选时先贴文字再贴图片。
- **纯键盘操作**：`↑` `↓` 在卡片间移动、`Home` / `End` 跳到首尾、`Space` 切换多选、`Enter` 粘贴，
  全程不用鼠标；列表自动跟着光标滚动。
- **搜索**：`Ctrl+F`，匹配完整文字内容（不只是卡片上的预览）。
- **可换的呼出快捷键**：设置里点「修改」后直接按下想要的组合键（必须含 `Ctrl` / `Alt` / `Win`）；
  组合键被别的程序占用时会当场提示「换一个」，不会静默失效。设置写在 `config.json`，下次启动照旧。
- **托盘菜单**：显示面板 / 暂停记录 / 打开数据目录 / 清空历史 / 设置 / 退出。
- **常驻显示**：标题栏右侧的 `📌` 一键常驻，之后点别的窗口、按 `Win` 键都不会把面板收回；
  再点一次恢复「点别处自动收起」、粘贴完仍会留在屏幕上。设置写在 `config.json`，重启照旧。
- **自由摆放**：拖系统标题栏或面板顶部的空白处即可移动窗口，拖边框 / 四角可以缩放（最小 420×320）；
  点标题栏、拖窗口、拖边框缩放都不会再把面板误收起。
- **删除与清理**：卡片右下角 `✕` 删除单条；设置或托盘菜单可一键清空全部历史（图片原图一并删除）。
- **隐私保护**：托盘 / 设置里的「暂停记录」；密码管理器、银行控件标记为「不要记录」的剪贴板内容一律不入库。
- **开机自启**：可选，写当前用户注册表 `Run` 键，不需要管理员权限，移动 exe 后会自动修正路径。
- **单实例**：重复启动不会开第二个进程，而是把已经开着的面板呼出来（避免互抢全局快捷键）。
- **可排查**：设置里能一键打开数据目录和日志（`logs\paste-boss.log`，超过 1MB 自动滚动）。

## 快捷键

| 按键 | 作用 |
| --- | --- |
| `Alt+V` | 呼出 / 收起面板（默认值，可在设置里改成别的组合） |
| `Ctrl+F` | 聚焦搜索框 |
| `↑` / `↓` | 光标在卡片间移动（自动滚动跟随） |
| `Home` / `End` | 跳到第一张 / 最后一张 |
| `Space` | 把光标所在的卡片加入 / 移出多选 |
| `Ctrl+点击` / `Shift+点击` | 多选 / 连选 |
| `Ctrl+A` | 全选当前列表（搜索时只选结果） |
| `Enter` | 粘贴所选；没有多选时粘贴光标所在的那一条 |
| `Esc` | 逐层退出：关设置 / 取消录制 → 清搜索 → 取消多选 → 收起面板 |

## 开发

```bash
npm install
npm run tauri dev      # 开发模式，前端热更新，终端里能看到 Rust 日志
```

只跑前端：`npm run dev`；只做类型检查 + 打包前端：`npm run build`。

后端检查与测试：

```bash
cd src-tauri
cargo check --all-targets
cargo test --lib
```

## 构建发行版

```bash
_build.cmd           # 等价于 npm run tauri build，只是先把 Windows SDK 的 bin 加进 PATH
```

`_build.cmd` 里做的事只有两件：把 `Windows Kits\10\bin\...\x64`（里面有 `rc.exe`，
`tauri-winres` 打包资源要用）临时加到 `PATH` 最前面，然后在仓库根目录执行
`npm run tauri build`，额外参数会原样透传（例如 `_build.cmd --no-bundle` 只出 exe、不打安装包）。

> ⚠️ **不要用裸 `cargo build --release`**。那样会缺 `tauri/custom-protocol` feature，
> Tauri 的 `build.rs` 会把它当成开发模式，产出的 exe 启动后去找 `http://localhost:1420`，
> 表现为「双击打不开、只显示无法连接」。发行版请一律走 `npm run tauri build`
> （生成的安装包在 `src-tauri/target/release/bundle/`）。

> ℹ️ 打 MSI 需要 tauri-cli 现场下载 WiX 工具链（`github.com/wixtoolset`）。网络不通时会一直卡在
> `Downloading ...wix-binaries.zip`，这时用 `_build.cmd --no-bundle` 只出绿色版 exe 即可，
> 产物在 `src-tauri/target/release/paste-boss.exe`，直接复制到 `release-package/` 就能发。

## 数据与隐私

所有数据都在本机，程序不发起任何网络请求。

| 路径 | 内容 |
| --- | --- |
| `%APPDATA%\com.tauri-app.paste-boss\history.json` | 历史记录（文字全文、图片尺寸与缩略图） |
| `%APPDATA%\com.tauri-app.paste-boss\blobs\` | 图片原图 `blob-<id>.png` |
| `%APPDATA%\com.tauri-app.paste-boss\config.json` | 用户配置（历史条数上限、呼出快捷键、是否常驻显示） |
| `%APPDATA%\com.tauri-app.paste-boss\logs\paste-boss.log` | 运行日志 |

清空历史：面板 ⚙ 设置 → 「清空历史」，或托盘右键菜单 → 「清空历史」；
也可以退出程序后直接删掉上面那个目录。数据目录和日志都能在设置里一键打开。

## 目录结构

```
src/                 Vue 3 前端（面板、设置、多选与搜索）
src-tauri/src/
  lib.rs             命令、剪贴板监听线程、多选粘贴
  history.rs         历史落盘与恢复
  config.rs          用户配置（历史条数上限、呼出快捷键、是否常驻显示）
  privacy.rs         暂停记录 + 「不要记录」标记识别
  logging.rs         日志落盘 + panic 钩子
  single_instance.rs 单实例锁与「唤起已有窗口」
  hotkey.rs          全局快捷键（可换、冲突检测）、贴边弹出、模拟 Ctrl+V
  tray.rs            托盘图标与菜单
  settings.rs        开机自启（注册表 Run 键）
```

## 许可证

本项目以 [MIT](LICENSE) 许可证发布。
第三方组件的许可声明见 [THIRD-PARTY-NOTICE.md](THIRD-PARTY-NOTICE.md)。

---

最后更新：2026-11-07

