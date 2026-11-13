# 考勤管理（Rust + Tauri）

Windows 本地考勤与加班桌面工具。当前实现包含 Tauri 托盘窗口、本地 SQLite、每日考勤计算、Windows 安全事件同步、节假日日历缓存、手动修正、月度统计、备份/恢复和 CSV 导出。

## 开发环境

- Rust stable（MSVC 目标）
- Microsoft C++ Build Tools 和 Windows SDK
- WebView2 Runtime

启动开发版：

```powershell
npm install
npm run dev
```

项目脚本会自动将当前用户的 Rust 工具链目录加入运行环境，因此安装 Rust 后无需依赖已打开终端的 `PATH` 是否刷新。

打包 Windows 安装程序：

```powershell
npm run build
```

程序读取当前用户 SID 对应的 `Microsoft-Windows-Winlogon/Operational` 日志。通知事件 4 记为锁屏、5 记为解锁；该日志通常可由普通用户读取，不要求以管理员身份运行。若设备禁用了该日志，应用会提示读取失败；仍可手动录入。

## 主要规则

- 工作日预计下班时间 = 当日最早解锁 + 9 小时。
- 工作日加班需满 2 小时，之后按完整半小时向下累计。
- 休息日按首次解锁至最后锁屏的跨度，每 30 分钟累计，不扣午休。
- 未锁屏时在次日 02:00 截止并标记异常。
- 默认每月首个自然周六为工作日；手动日期覆盖日历结果。

## 数据

数据库位于 Windows 当前用户的应用数据目录。导出备份生成包含每日记录、原始事件及日历设置的 JSON 文件；恢复会合并备份数据。考勤数据不发送到日历服务。

