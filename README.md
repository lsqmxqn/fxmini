<div align="center">

<img src="docs/icon-preview.png" width="112" alt="FxMini">

# FxMini

**只有 FxSound 的声音，没有 FxSound 的 App。**

托盘常驻的 Windows 音频增强工具。复用 FxSound 开源的 DSP 引擎与虚拟声卡驱动，
换掉它那套常驻的图形界面：没有主窗口，没有账号，没有联网，空闲内存约 **16 MB**。

[![License](https://img.shields.io/badge/license-AGPL--3.0--or--later-2b6cb0?style=flat-square)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-0a7ea4?style=flat-square)](#系统要求)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-dea584?style=flat-square)](Cargo.toml)
[![AI](https://img.shields.io/badge/AI-DeepSeek--V4.1--Flash-6b46c1?style=flat-square)](#ai-生成声明)
[![Built with WorkBuddy](https://img.shields.io/badge/built%20with-WorkBuddy-16a34a?style=flat-square)](https://www.workbuddy.cn)

</div>

---

## 这是什么

FxSound 的效果很好，但它的官方 App 是一个常驻的图形程序：内存占用三位数（MB）、带账号与联网、要一直开着主界面。如果你只想"让系统的声音变好听一点"，这些都不需要。

FxMini 把 FxSound 的两样东西留下来——**DSP 引擎**和**虚拟声卡驱动**——其余的换成一个小托盘程序：

- 系统里所有应用的声音都过增强，不只是某一个播放器（靠接管系统默认输出设备实现）；
- 需要调音时点托盘弹出一个面板，关掉就释放；
- 平时是一个图标，占用可以忽略。

因为 DSP 用的是上游**同一份源码**，声音与 FxSound 是**逐位一致**的，`.fac` 预设生态可以直接用。

仓库：<https://github.com/lsqmxqn/fxmini>

> 想了解实现细节：架构与选型见 [`docs/设计方案.md`](docs/设计方案.md)，踩坑与验证数据见 [`docs/开发笔记.md`](docs/开发笔记.md)。

---

## 特性

| | |
|---|---|
| **全系统生效** | 接管系统默认输出设备，所有应用的声音都经过增强，而不是只处理某一个播放器 |
| **音质与 FxSound 一致** | 直接用上游 DSP 源码编译，不是近似实现；`.fac` 预设可直接使用 |
| **占用小** | 空闲内存约 **16 MB**，空闲 CPU 约 **1.25%**；目标是最低限度的常驻 |
| **无主窗口** | 平时只有托盘图标；调音面板按需弹出，关闭后立即释放 GL 上下文与字体图集 |
| **31 段均衡器 + 5 个音效槽** | Fidelity / Surround / Ambience / DynamicBoost / Bass，带实时频谱显示 |
| **17 个内置预设** | Rock、Jazz、Movie、Gaming 等；自定义 `.fac` 丢进用户目录即可出现在菜单里 |
| **不会把你留在静音状态** | 崩溃或被强杀后，下次启动自动把默认输出交还给真实声卡；卸载脚本也这么做 |
| **开机自启（默认开）** | 默认开启，而且不是便利功能——见下面的说明 |
| **免安装、单文件** | 静态 CRT，不依赖 VC++ 运行库；打包成便携 zip，解压即用 |

### 关于开机自启

FxMini 必须**接管系统默认输出设备**，增强才会在音频链路里：

```
默认输出 = 虚拟声卡 ──> FxMini 回环采集 ──> DSP 增强 ──> 物理声卡
```

虚拟声卡是个死胡同，没有人从它取数据就是彻底的静音。所以一旦 FxMini 持有默认输出，**登录时它没起来就等于这台机器没有声音**。自启因此默认开启，启动时会把「期望状态」与注册表对账：

- 没有启动项 → 写入；
- 启动项指向**另一个位置**的 exe（换过目录、或开发时跑 `target/`）→ 重写，否则开机启动的是一个不存在的路径，而且悄无声息；
- 你在**任务管理器 → 启动**里手动关掉了 → **原样保留，不抢**，托盘里的勾选状态也会如实反映。

---

## 工作原理

```
任意播放器 / 系统声音
        │
        ▼
 ┌──────────────────┐
 │  FxMini 虚拟声卡  │   ← 系统默认输出被指向这里
 └──────────────────┘
        │  WASAPI 回环采集
        ▼
 ┌──────────────────┐
 │   FxSound DSP    │   31 段 EQ + 5 个音效槽
 └──────────────────┘
        │
        ▼
   真实声卡 → 扬声器 / 耳机
```

FxMini 自己写的是中间那段胶水：虚拟声卡的安装卸载、WASAPI 回环链路、托盘、面板，以及最关键的——**默认设备的接管与归还**。DSP 与驱动来自上游。

---

## 系统要求

- **Windows 10 / 11，x64**
- 安装虚拟声卡驱动时需要**管理员权限**（驱动是 FxSound 的签名包，由 Windows 校验）
- 使用预编译包不需要任何运行时依赖
- **从源码构建**另外需要 MSVC 工具集 + Windows SDK + Rust；仓库里的脚本会自动定位它们，见[从源码构建](#从源码构建)

---

## 安装

预编译包是 `FxMini-<版本>-win64.zip`，解压后是一个 `FxMini\` 目录：

```
FxMini\
  fxmini.exe               主程序
  driver\                  虚拟声卡驱动（fxvad.inf / .sys / .ntamd64.cat）
  README.txt               说明
  LICENSE.txt              许可
  install.ps1              单用户安装：复制到 %LOCALAPPDATA%、建快捷方式、启动
  uninstall.ps1            反向操作，含归还默认输出
```

**首次使用**：

1. 解压到任意目录（例如 `%LOCALAPPDATA%\Programs\FxMini`），双击 `fxmini.exe`，托盘出现图标；
2. 右键托盘 → **安装虚拟声卡驱动…**，在 UAC 弹窗里允许；
3. 右键托盘 → **输出走 FxMini 增强**；
4. 播放一段音乐，右键托盘 → **调音面板…** 调整音效；或从 **预设** 子菜单里挑一个。

想装到 `%LOCALAPPDATA%\Programs\FxMini` 并创建开始菜单快捷方式，用管理员 PowerShell 跑一次 `install.ps1` 即可（它**不**安装驱动，驱动始终在第 2 步由你确认）。

**卸载**：跑 `uninstall.ps1`，或手动删除目录。卸载脚本会先把默认输出交还给真实声卡——否则你会留下一台没有声音的机器。

---

## 使用

### 托盘菜单

| 菜单项 | 说明 |
|---|---|
| 启用音效 | 总开关，关掉即旁通 DSP |
| 输出走 FxMini 增强 | 把系统默认输出指向虚拟声卡。**已经生效时这一项会置灰**，所以"右键看看它灰没灰"就是"音频到底走了增强没有"的答案 |
| 调音面板… | 打开调音窗口（已打开时再次点击会聚焦到现有窗口） |
| 预设 ▸ | 切换 `.fac` 预设，当前生效的一项带勾 |
| 开机自启 | 见上文说明 |
| 安装 / 卸载虚拟声卡驱动… | 会触发 UAC；不适用的那一项自动置灰 |
| 重新扫描预设 | 重新读取用户预设目录 |
| 退出 | 归还默认输出后退出 |

### 命令行

| 参数 | 用途 |
|---|---|
| `--panel` | 启动时直接打开调音面板 |
| `--restore-output` | 把默认输出交还给真实声卡后退出。**"突然没声音"时的救命命令** |
| `--install-driver` | 安装虚拟声卡驱动（须以管理员身份运行，由主程序通过 UAC 自行调用，一般不用手敲） |
| `--remove-driver` | 卸载虚拟声卡驱动（同上） |

### 数据与配置

全部在 `%APPDATA%\FxMini\` 下：

```
config.json     设置（是否启用、当前预设、开机自启、崩溃恢复标记）
presets\        内置预设会解包到这里；把你自己的 .fac 丢进来即可被识别
fxmini.log      运行日志，排查问题先看它
```

---

## 疑难排解

**突然没有声音了**
FxMini 退出时会把默认输出交还给真实声卡；如果是被强杀或崩溃，勾选状态可能停在虚拟声卡上。跑一次 `fxmini.exe --restore-output` 即可。正常退出后再次运行 FxMini 也会自动修复。

**播放器/系统里多了一个 FxMini 设备**
这是正常的——虚拟声卡就是以这个身份出现在系统里的。不需要手动改默认设备，托盘菜单的"输出走 FxMini 增强"会处理。

**开机后没有声音**
先确认 FxMini 有没有启动（托盘有没有图标）。如果没有，检查**任务管理器 → 启动**里 FxMini 是否被禁用；被禁用时 FxMini 不会强行打开它，只会如实显示。

**自己编译的 exe 没有图标**
`build.rs` 在编译期调用 `rc.exe` 把图标与版本资源写进 exe。找不到 `rc.exe` 时构建仍然成功，只是产出一个没有图标的 exe（会给出警告）。安装 Windows SDK，或用 `FXMINI_RC` 环境变量指向 `rc.exe` 的绝对路径。

**系统里还有一个 FxSound 的虚拟设备**
如果之前装过官方 FxSound，可能同时存在两个虚拟声卡。建议先卸载官方的那个，避免选错设备。

---

## 从源码构建

```powershell
# 一条命令：初始化工具链 + 构建（推荐）
.\build.ps1
.\build.ps1 --release --bin fxmini

# 等价的 PowerShell 手工两步
. .\toolchain.ps1
cargo build --release --bin fxmini
```

```bash
# Git Bash / MSYS
source toolchain.sh
cargo build --release --bin fxmini
```

> **为什么需要 `toolchain.ps1`**：本机的 Visual Studio 装在非默认路径且未向安装器注册，`cargo` 找不到 `link.exe`、`cc` 找不到 `cl.exe`。脚本负责定位真正完整的工具集并把它的 bin 前置到 PATH。细节与另外几个构建陷阱见 [`docs/开发笔记.md`](docs/开发笔记.md) 第 2 节。

`vendor/` 下的 127 个上游单元（DSP 94 + 辅助层 33）**已经随仓库提交**，clone 下来直接就能编，不需要先准备 `fxsound-app`。只有打算跟上游同步时才需要重新 vendor：

```bash
git clone https://github.com/fxsound2/fxsound-app ../fxsound-app
```

```powershell
.\vendor.ps1 -Source ..\fxsound-app -Dest .   # 幂等：只覆盖不删除，含上游补丁
```

### 打包

```powershell
.\package.ps1              # 构建 + 组装 dist/FxMini + 压缩
.\package.ps1 -NoBuild     # 只重新打包
```

产出 `dist/FxMini-<版本>-win64.zip` 与 `dist/SHA256SUMS.txt`。脚本含两项强制自检：exe 必须带图标与版本资源、且不得依赖 VC++ 运行库（后者在构建日志里完全看不见，只会在别人机器上表现为"程序打不开"）。

---

## 开发与测试

```bash
cargo check --all-targets      # 零告警是硬要求
cargo test --lib               # 单元测试（另有 1 个真实开窗的测试默认忽略）
cargo test --lib -- --ignored  # 跑那个会真的打开一个窗口的面板测试
```

三个诊断二进制，按"从离线到真机"的顺序：

| 命令 | 作用 |
|---|---|
| `cargo run --release --bin dspcheck` | 纯离线跑一段 1 kHz 正弦，验证 DSP 能编译、能解析真实 `.fac`、且确实改变了信号。**不涉及驱动与 WASAPI，报 FAIL 后面全白搭** |
| `cargo run --release --bin audiochk -- --seconds 5` | 跑真实回环链路，报告丢帧、削波与峰值 |
| `cargo run --release --bin audioenv` | 打印音频环境诊断；`--route-test 5` 实测默认设备切换与归还，`--restore-output` 手工撤销一次接管 |

---

## 项目结构

```
fxmini/
├── build.rs                   编译两个上游静态库；生成并嵌入图标与版本资源
├── .cargo/config.toml         静态 CRT：让发布 exe 不依赖 VC++ 运行库
├── build.ps1 / toolchain.*    一条命令构建；工具链环境初始化（PowerShell + Bash）
├── package.ps1                M6 打包：组装 dist/FxMini + zip + SHA256
├── vendor.ps1                 拉取上游源码（幂等，含补丁）
├── assets/presets/            17 个内置 .fac，include_bytes! 进二进制
├── capi/                      DSP 的 C ABI 封装（上游是 C++ class）
├── tools/
│   └── inspect_resources.py   校验 exe 里的 RT_ICON / RT_VERSION
├── vendor/                    上游源码快照（dsp + audiopassthru 的 support 层）
├── src/
│   ├── main.rs                入口：日志、启动参数、单实例、托盘消息循环
│   ├── app.rs                 托盘 / 引擎 / 配置之间的接线
│   ├── engine.rs              音频线程：回环采集 → DSP → 渲染
│   ├── routing.rs             接管与归还系统默认输出
│   ├── device.rs              端点枚举、默认设备切换、热插拔通知
│   ├── driver.rs              虚拟声卡检测 / 安装 / 卸载
│   ├── preset.rs              .fac 解析、内置预设、用户预设目录
│   ├── autostart.rs           开机自启与注册表对账
│   ├── config.rs              设置持久化（%APPDATA%\FxMini）
│   ├── ffi.rs                 C ABI 的 Rust 绑定 + RAII 封装
│   ├── ui/                    托盘、调音面板、图标绘制
│   └── bin/                   dspcheck / audiochk / audioenv 三个诊断工具
└── docs/                      设计方案、开发笔记
```

`vendor/` 下**不包含**上游的设备层（`audiopassthru/src/AudioPassthru`、`src/sndDevices`）——FxMini 自己实现音频环，那部分是死代码。

---

## AI 生成声明

FxMini 是一个 **AI 生成的项目**，请在评估与使用它时把这一点考虑进去。

| | |
|---|---|
| 生成方式 | [WorkBuddy](https://www.workbuddy.cn) 智能体（agentic coding）：由人类给出目标、审阅产出并验收 |
| 驱动模型 | **DeepSeek-V4.1-Flash** |
| 生成时间 | 2026 年 9 月 |
| 生成范围 | `src/`、`build.rs`、`package.ps1`、`vendor.ps1`、`toolchain.*`、`tools/`、`docs/` —— 即除 `vendor/` 之外的全部内容 |
| 非生成部分 | `vendor/` 逐字复制自上游项目；分发包 `driver/` 中的驱动是 FxSound 的签名二进制，本仓库不含其源码 |

这意味着：

- 代码经过自动化测试（`cargo test`、`dspcheck`、`audiochk`）与真机音频链路验证，但**没有经过安全审计**；
- 设计取舍与踩坑记录是真实的、可追溯的，见 [`docs/开发笔记.md`](docs/开发笔记.md)——可以据此判断实现是否可靠，而不必相信文档的结论；
- 如果你要把它用在关键场景，建议自行复跑测试，并至少读一遍 [`src/routing.rs`](src/routing.rs)：**它会改动你系统的默认音频设备**。

---

## 许可

[**AGPL-3.0-or-later**](LICENSE)，全文见 [`LICENSE`](LICENSE)。

### 为什么是这个协议

这不是一个自由的选项，而是由依赖链决定的：

- `vendor/dsp/` 与 `vendor/audiopassthru/` 是 [`fxsound2/fxsound-app`](https://github.com/fxsound2/fxsound-app) 的源码，授 AGPL-3.0；上游源码头写明 *"either version 3 of the License, or (at your option) any later version"*。
- 这份代码被**静态链接进同一个 `fxmini.exe`**，因此整体构成 AGPL 的衍生作品，按 AGPL §5 必须以 AGPL-3.0 分发。
- 换成 MIT / Apache-2.0 不成立（与 AGPL 的传染性冲突）；换成 GPL-3.0 也不成立（GPL 比 AGPL 宽松，不能作为 AGPL 代码的下游许可）。

因此 `Cargo.toml` 中的 `license = "AGPL-3.0-or-later"` 采用与上游一致的表述（`-or-later` 即来自上游源码头的授权措辞）。

**对你的影响**：自己使用、修改、编译都没有任何限制；一旦**分发**（包括把修改版部署成供他人使用的网络服务），你必须以 AGPL-3.0 提供完整的对应源码并保留版权声明。

### 第三方与商标

- `vendor/` 下是上游文件的逐字副本（外加 [`patches/README.md`](patches/README.md) 记录的补丁），各自保留原许可；`vendor/LICENSE.fxsound-app` 是上游随源码附带的许可证全文。
- 分发包 `driver/` 中的虚拟声卡驱动来自 FxSound 的**签名二进制包**，不由本仓库以 AGPL 授权。它们按原样分发，装卸需要管理员权限；再分发涉及的商标与签名问题需自行确认。
- **"FxSound" 名称与图标归其所有者所有。** FxMini 是独立的、与 FxSound 无关联的项目，不是 FxSound 的官方产品。

---

## 致谢

FxMini 不重复造轮子，声音与驱动都来自上游：

| 项目 | 用到的部分 | 许可 |
|---|---|---|
| [`fxsound2/fxsound-app`](https://github.com/fxsound2/fxsound-app) | DSP 引擎（`dsp/`）+ 辅助层（`audiopassthru/` 的 support 部分） | AGPL-3.0 |
| [`fxsound2/fxsound-driver`](https://github.com/fxsound2/fxsound-driver) | 虚拟声卡驱动 `fxvad`（签名二进制） | AGPL-3.0 |

上游公开了完整的 DSP 源码（约 9000 行 C++），公开 API 只有约 30 个方法——干净到可以当库用。这是 FxMini 得以存在的前提。
