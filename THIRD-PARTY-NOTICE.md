# Paste-boss 第三方组件与许可声明

Paste-boss 本体以 MIT 许可证发布（见 `LICENSE`）。
下面是它运行时依赖的第三方开源组件及其许可证。打包分发本程序时，请把本文件一并附带。

## 运行时（Rust）

| 组件 | 用途 | 许可证 | 主页 |
| --- | --- | --- | --- |
| Tauri（`tauri`、`tauri-runtime-wry`、`tauri-plugin-opener`） | 桌面窗口框架、WebView 宿主、系统托盘 | MIT OR Apache-2.0 | https://tauri.app |
| WRY / TAO | 跨平台 WebView 与窗口（Tauri 的底层实现） | MIT OR Apache-2.0 | https://github.com/tauri-apps/wry |
| arboard | 剪贴板读写（文字 / 图片） | MIT OR Apache-2.0 | https://github.com/1Password/arboard |
| clipboard-win | Windows 剪贴板底层格式（文字 + 文件列表、格式检测） | BSL-1.0 | https://github.com/DoumanAsh/clipboard-win |
| image | PNG 编解码、缩略图缩放 | MIT OR Apache-2.0 | https://github.com/image-rs/image |
| base64 | 缩略图转 data URL | MIT OR Apache-2.0 | https://github.com/marshallpierce/rust-base64 |
| serde / serde_json | 历史与配置的序列化 | MIT OR Apache-2.0 | https://serde.rs |

## 运行时（前端）

| 组件 | 用途 | 许可证 | 主页 |
| --- | --- | --- | --- |
| Vue 3 | 面板界面 | MIT | https://vuejs.org |
| @tauri-apps/api | 前端调用 Rust 命令、监听事件 | MIT OR Apache-2.0 | https://tauri.app |

## 构建与开发（不进入发行包）

| 组件 | 用途 | 许可证 |
| --- | --- | --- |
| Vite | 前端构建 | MIT |
| TypeScript | 类型检查 | Apache-2.0 |
| vue-tsc | Vue 单文件组件类型检查 | MIT |
| @vitejs/plugin-vue | Vite 的 Vue 支持 | MIT |

## 系统组件

| 组件 | 说明 |
| --- | --- |
| Microsoft Edge WebView2 Runtime | 由微软随 Windows 10/11 提供；安装包会在缺失时引导安装。不属于本项目分发内容，受微软许可约束。 |

## 说明

- BSL-1.0（Boost Software License）允许商业使用和再分发，唯一要求是保留版权声明，
  因此本文件必须随程序一起分发。
- 若你修改了依赖清单，请同步更新本文件。
