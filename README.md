# FxMini

托盘常驻的 Windows 音频增强工具。复用 FxSound 的虚拟声卡驱动与 DSP 引擎，不带 FxSound 的 App 界面，内存占用目标 **< 20 MB**。

> 详细技术方案见 [`docs/设计方案.md`](docs/设计方案.md)。

仓库：<https://github.com/lsqmxqn/fxmini>

### 关联的上游项目

FxMini 是 FxSound 开源实现的**再包装**，不重复造轮子。两处依赖都在上游的公开仓库里，`vendor/` 中的源码即取自它们：

| 项目 | 用到的部分 | 许可 |
|---|---|---|
| [`fxsound2/fxsound-app`](https://github.com/fxsound2/fxsound-app) | DSP 引擎（`dsp/`）+ 辅助层（`audiopassthru/` 的 support 部分） | AGPL-3.0 |
| [`fxsound2/fxsound-driver`](https://github.com/fxsound2/fxsound-driver) | 虚拟声卡驱动 `fxvad`（签名二进制，装/卸与分发时才需要） | AGPL-3.0 |

FxMini 自己写的只有胶水：虚拟声卡的安装卸载、WASAPI 回环链路、托盘、小面板。

---

## 为什么是"复用 DSP"而不是"重写"

`fxsound2/fxsound-app` 以 AGPL-3.0 开源了**完整的 DSP 引擎源码**（`dsp/`，约 9000 行 C++）。它的公开 API 只有 79 行、约 30 个方法，干净到可以当库用。

这带来两个结果：

- 不需要逆向或逼近 FxSound 的音效算法
- 声音和 FxSound 官方**逐位一致**，`.fac` 预设生态直接用

需要自己写的只有胶水：虚拟声卡的安装卸载、WASAPI 回环链路、托盘、小面板。

---

## 前置条件

本机的 MSVC 与 Windows SDK 已经装好了，但**需要显式初始化环境**才能用：

| 组件 | 位置 |
|---|---|
| MSVC 工具集 | `D:\Program Files\VisualStudio\VC\Tools\MSVC\14.42.34433` |
| Windows SDK | `C:\Program Files (x86)\Windows Kits\10`（10.0.26100.0） |
| Rust | 1.94.1，host `x86_64-pc-windows-msvc` |

**为什么需要额外脚本**：这台机器的 Visual Studio 装在 `D:\Program Files\VisualStudio` 且没有向安装器注册，`vswhere.exe` 查不到、注册表里也没有条目。后果有两个：

1. `cargo` 找不到 `link.exe`，连 hello world 都链不出来
2. `cc` crate 找不到 `cl.exe`，DSP 根本编不了

还有第二个更阴的坑：**Git for Windows 在 `/usr/bin` 里带了一个同名的 `link.exe`**，它是 coreutils 的硬链接工具，不是链接器。PATH 顺序不对时 rustc 会调到它，报出莫名其妙的 `link: extra operand ... Try 'link --help'`。

两个问题都由 `toolchain.ps1` / `toolchain.sh` 解决——它们会自动挑选**真正完整**的工具集版本（本机 `14.50.35717` 有头文件但没有 `lib\x64`，必须跳过），然后把 MSVC 的 bin 目录前置到 PATH。

```powershell
# PowerShell（推荐：一条命令，自动处理工具链与 cargo 的 PATH）
.\build.ps1

# 等价的手工两步
. .\toolchain.ps1
cargo build --release --bin dspcheck
```

```bash
# Git Bash / MSYS
source toolchain.sh
cargo build --release --bin dspcheck
```

**前置条件只此一步。** 两个静态库所需的宏定义已经写进 `build.rs`，与上游两个 `.vcxproj` 严格一致，不需要手工设置。

---

## 快速开始

```bash
# 1. 初始化工具链（必须先做，见上一节）
source toolchain.sh                  # 或 PowerShell: . .\toolchain.ps1

# 2. 跑 M1 冒烟测试
cargo run --release --bin dspcheck

# 指定预设（用仓库自带的）
cargo run --release --bin dspcheck -- assets/presets/Gaming.fac
```

`vendor/` 里那 127 个上游单元（DSP 94 + 辅助层 33）**已经随仓库提交**，clone 下来直接就能编，不需要先准备 `fxsound-app`。只有打算跟上游同步时才需要重新 vendor：

```bash
git clone https://github.com/fxsound2/fxsound-app ../fxsound-app
```

```powershell
.\vendor.ps1 -Source ..\fxsound-app -Dest .     # 幂等：只覆盖不删除，含上游补丁
```

`dspcheck` 不涉及驱动和 WASAPI，纯离线跑一段 1 kHz 正弦。它断言三件事：

1. 真实 `.fac` 能被解析并填充 EQ 与音效状态
2. `set_power(true)` 能读回 on（上游 getter 是反的，这里能验证封装没退化）
3. 引擎确实改变了信号（说明处理真的生效了）

输出会打印 31 段 EQ 的频率/增益、五个音效槽在两种值域下的读数、以及处理前后的 RMS 变化。**如果它报 FAIL，后面全白搭**——所以这一步必须最先做。

实测输出（`Music.fac`）：

```
engine created, 5 effect slots reported
preset name      : 音乐
power            : true  (round-trip of set_power(true))
EQ bands         : 31
effects
                  get 0-1   set 0-10  fac Main
  Fidelity          0.394       3.94        50
  Bass              0.472       4.72        60
signal check (1 kHz sine at -6 dBFS)
  RMS in        : 0.353553
  RMS out       : 0.588367
  change        : +4.42 dB
OK: engine compiled, parsed a real preset, and altered the signal
```

---

## 运行

```powershell
.\build.ps1 --release --bin fxmini
.\target\release\fxmini.exe            # 托盘图标出现
.\target\release\fxmini.exe --panel    # 顺带直接打开调音面板
```

托盘右键菜单：启用音效 / 输出走 FxMini 增强 / 调音面板 / 预设 / 开机自启 / 安装与卸载虚拟声卡驱动 / 退出。

### 开机自启默认开启，而且不是可选项

FxMini 必须**接管系统默认输出设备**，增强才会在链路里：

```
默认输出 = 虚拟声卡 → 回环采集 → DSP → 物理声卡
```

虚拟声卡是死胡同，没人从它取数据就是彻底的静音。所以一旦 FxMini 持有默认输出，登录时它没起来 = 这台机器没有声音。自启因此默认开启（`config.rs` 的 `autostart: true`），启动时由 [`src/autostart.rs`](src/autostart.rs) 把「期望状态」与注册表对账：

- `HKCU\...\Run` 里没有条目 → 写入；
- 条目指向**另一个位置**的 exe（换目录、开发时跑 `target/`）→ 重写，否则开机启动的是一个已不存在的路径，而且悄无声息；
- 用户在**任务管理器 → 启动**里关掉了（状态存在 `Explorer\StartupApproved\Run`，Windows 会因此不启动它，尽管条目还在）→ **原样保留，不抢**；`is_enabled()` 会把这个状态算进去，所以托盘勾选不会撒谎；
- 从托盘再打开时会一并清掉任务管理器的禁用标记，否则"重新勾上"根本不会生效。

### 没有声音时

```powershell
.\target\release\fxmini.exe --restore-output
```

把默认输出切回真实声卡后退出。正常退出会自己归还，异常退出会在下次启动时自动修复，所以这条命令基本用不上——它主要给卸载脚本用：托盘程序没有 IPC，卸载只能强杀，而强杀会跳过归还路径。

---

## 打包（M6）

```powershell
.\package.ps1              # 构建 + 组装 + 压缩
.\package.ps1 -NoBuild     # 只重新打包
```

产出 `dist/FxMini-<version>-win64.zip`，内容是：

```
FxMini\
  fxmini.exe                   主程序（含图标与版本资源）
  driver\fxvad.inf             虚拟声卡驱动三件套（FxSound 签名版，原样分发）
  driver\fxvad.sys
  driver\fxvadntamd64.cat
  README.txt                   双语说明
  LICENSE.txt                  AGPL-3.0-or-later + 商标说明
  install.ps1                  单用户安装：复制、建快捷方式、启动
  uninstall.ps1                反向操作，包含归还默认输出
SHA256SUMS.txt
```

两件在打包时**强制检查**而不是假设的事：

1. **exe 里有图标和版本资源**。没有 `rc.exe` 时构建照样成功，只是产出一个没脸的 exe —— 正是这一次要修的缺陷。脚本读不到 `ProductName`/`FileVersion` 就直接失败；`tools/inspect_resources.py` 可以进一步 dump 出 `RT_ICON`/`RT_GROUP_ICON`/`RT_VERSION` 并把图标存成 PNG 看。
2. **exe 不导入 VC++ 运行库**。`.cargo/config.toml` 打开 `target-feature=+crt-static`，`build.rs` 检测同一个设置并让 vendored C++ 用 `/MT`（MSVC 不支持一个进程里混两种 CRT）。检查方式是 `dumpbin /dependents` 里不能出现 `VCRUNTIME140`/`MSVCP140`。这个依赖在构建日志里完全看不见，只会在别人机器上表现为"程序打不开"。

驱动**不**内嵌进 exe，而是留在旁边的 `driver\` 目录：它是带自己许可证的签名二进制，让它在磁盘上保持可见比塞进我们的可执行文件更合适。`driver.rs` 本来就按「exe 同目录 → `driver\` → `resources\fxvad\`」的顺序找它。

图标由 `build.rs` 在编译期用 Rust 画出来（16/24/32/48/64/128/256 七个尺寸打包成 `.ico`），绘图代码与托盘图标**是同一份**（`src/ui/icon_raster.rs`，被 `build.rs` `include!`）——两份实现迟早会画得不一样，而这件事只有在用户桌面上才看得见。

---

## 目录结构

```
fxmini/
├── .cargo/config.toml             静态 CRT：让发布 exe 不依赖 VC++ 运行库
├── .gitattributes                 行尾归一化；.fac 与驱动二进制不做文本处理
├── .gitignore                     忽略 target/、dist/、构建日志与 vendor 的遗留目录
├── Cargo.toml
├── LICENSE                        AGPL-3.0 全文
├── build.rs                       编译 dfxdsp + dfxutil 两个静态库；生成并嵌入图标与版本资源
├── build.ps1                      一条命令：工具链 + cargo
├── package.ps1                    M6 打包：组装 dist/FxMini + zip + SHA256
├── toolchain.ps1                  PowerShell 环境初始化（自动定位工具集）
├── toolchain.sh                   Git Bash 等价版本
├── vendor.ps1                     vendor 脚本（幂等，默认只覆盖不删除，含上游补丁）
├── patches/README.md              vendor 时打在源码上的上游缺陷修复记录
├── assets/presets/                17 个内置 .fac，include_bytes! 进二进制
├── capi/
│   ├── dfxdsp_capi.h              C ABI 声明
│   └── dfxdsp_capi.cpp            包装 C++ class DfxDsp
├── tools/
│   └── inspect_resources.py       校验 exe 里的 RT_ICON / RT_VERSION（M6 自检）
├── vendor/
│   ├── dsp/                       上游 DSP 源码树（94 单元 + 196 头文件）
│   ├── sources-dsp.txt            94 个单元清单，从 DfxDsp.vcxproj 导出
│   ├── audiopassthru/             上游辅助层（33 单元 + include/）
│   ├── sources-audiopassthru.txt  33 个单元清单，从 audiopassthru.vcxproj 导出
│   └── LICENSE.fxsound-app        AGPL-3.0
├── src/
│   ├── main.rs                    日志、单实例互斥、启动参数、托盘消息循环
│   ├── lib.rs
│   ├── app.rs                     托盘 / 引擎 / 配置之间的接线
│   ├── autostart.rs               HKCU\...\Run 对账：期望状态 ↔ 注册表实际状态
│   ├── config.rs                  设置持久化与 %APPDATA%\FxMini 路径
│   ├── device.rs                  端点枚举、默认设备切换、IMMNotificationClient 热插拔
│   ├── routing.rs                 接管/归还系统默认输出（音频真正生效的前提）
│   ├── driver.rs                  虚拟声卡检测 / 安装 / 卸载
│   ├── engine.rs                  音频线程：回环采集 → DSP → 渲染
│   ├── preset.rs                  .fac 解析、内置预设、用户预设目录
│   ├── ffi.rs                     C ABI 的 Rust 绑定 + RAII 封装 Dsp
│   ├── ui/
│   │   ├── icon_raster.rs         图标绘制（托盘与 exe 图标共用同一份代码）
│   │   ├── icon.rs                包成 tray_icon::Icon
│   │   ├── tray.rs                托盘菜单
│   │   └── panel.rs               调音小面板（egui，常驻线程 + 通道唤醒）
│   └── bin/
│       ├── dspcheck.rs            M1 冒烟测试（离线 DSP）
│       ├── audiochk.rs            M2 冒烟测试（真实回环链路）
│       └── audioenv.rs            音频环境诊断 + --route-test / --restore-output
└── docs/设计方案.md               完整技术方案
```

`vendor/` 下**不包含**上游的设备层（`audiopassthru/src/AudioPassthru`、`src/sndDevices`）——FxMini 自己实现音频环，那部分是死代码。

---

## 上游的坑（都已在 `capi` / `build.rs` 里抹平）

**API 层面**

1. **返回码是反的**：上游 `#define OKAY 0`（`codedefs.h:95`），而且 `NOT_OKAY` 在 debug 构建里展开成一个函数调用。C ABI 统一成 `DFXDSP_OK = 0` / `DFXDSP_ERR = -1`。
2. **样本类型名不符实**：签名写 `short int *`，实际永远是 32 位浮点（`DfxDspPrivate.cpp:184`）。C ABI 直接暴露 `float*`。
3. **`isPowerOn()` 是反的**：它读 BYPASS 键，非零返回 true，即**被旁通时报"开"**（`DfxDspPrivate.cpp:216`）。而 `powerOn(true)` 把 BYPASS 设为 0，所以 getter 和 setter 自相矛盾。C ABI 已反转修正，`dspcheck` 里有回归检查。
4. **音效 getter/setter 值域不对称**（上游设计，非笔误）：

   | | 值域 | 内部 |
   |---|---|---|
   | `getEffectValue()` | **0.0 – 1.0** | 归一化 |
   | `setEffectValue()` | **0.0 – 10.0** | 存 `value / 10` |

   `.fac` 的 `Main`（0–127）由加载器直接写入归一化字段，所以 `Main / 127 == getEffectValue()`，经 setter 还原则是 `Main / 12.7`。

**构建层面**

5. **不能 glob 源码树**：`dsp/` 里有 123 个 `.c/.cpp`，但上游工程只编 **94 个**。剩下的 `Lex32org.c` 之类的 "org" 变体引用的是旧版结构体（`c_Lex.h` 里已经没有 `pre_dly_start_l`），编它直接 C2039。两份清单都由 `vendor.ps1` 从对应 `.vcxproj` 导出。
6. **DSP 不自包含**：它引用辅助层的 `reg*` / `mth*` / `pstr*` / `file*`，缺了会有 **14 个** LNK2019。所以还要编第二批 33 个单元（`github` 上游把这批放在 `audiopassthru` 工程里）。
7. **`UNICODE` / `_UNICODE` 藏在 `<CharacterSet>` 里**，不在 `<PreprocessorDefinitions>`。只看后者会漏，然后宽字符串调用点全部 C2664。
8. **别加 `WIN32_LEAN_AND_MEAN`**：它把 `objbase.h` 从 `windows.h` 剔出去，`pstr.cpp` 的 `CoCreateGuid` 就变 C3861。上游没定义它。
9. **PowerShell 5.1 的 `$ErrorActionPreference='Stop'` 会把原生命令的 stderr 变成终止性错误**。`cargo` 把进度写到 stderr，所以 `. .\toolchain.ps1; cargo build` 这个最自然的写法会莫名其妙死在 `Compiling ...` 上（报 `NativeCommandError`）。两个修法：`toolchain.ps1` 现在会在结束时把 `$ErrorActionPreference` 还原给调用方；`build.ps1` 再把「工具链 + cargo」包成一条命令。

**源码补丁层面**（`vendor.ps1` 每次 vendor 时按唯一锚点幂等施加，诊断见 [`patches/README.md`](patches/README.md)）

10. **`preset_list_handle_` 从未初始化**：构造函数漏了这一个成员，析构函数却对它调 `prelstFreeUp()`——释放的是堆里的野指针。实测 5 次运行里有 4 次在退出时崩溃（退出码 139，Windows 访问违例）。修法是构造函数补一行 `preset_list_handle_ = NULL;`。打完补丁后连续 14 次运行退出码全 0。

> `patches/README.md` 另有一节**「已知但故意不修」**的上游缺陷。目前记录了一条：`.fac` 读取链路上 `valsRead()` 的 10 处提前 `return` 会泄漏 handle（其中 8 处还泄漏已打开的 `FILE*`），且 `loadPreset()` 把所有失败原因抹平成 `NOT_OKAY`。这类问题只记录、不进补丁表，并写明**什么条件下才值得动手**——避免补丁膨胀，同时不丢失已有结论。

另外两个数值语义要记牢：

- `num_frames` 是**帧数**不是总采样数（依据 `sndDevicesDoCapture.cpp:385`：`*ip_numSampleSets = capturedFramesCount`）
- `loadPreset` 之后 `getNumEqBands()` 返回的是引擎当前段数（默认 31），**不是** `.fac` 里声明的段数（内置预设都是 10 段）。预设曲线会被映射到 31 段网格上——做 UI 时以 `getNumEqBands()` 为准，别照搬 `.fac`。

---

## 里程碑

| | 内容 | 状态 |
|---|---|---|
| M0 | 环境（MSVC）、vendor 源码 | ✅ 完成 |
| M1 | DSP 离线跑通，音色与 FxSound 一致 | ✅ **通过**：94+33 单元全部编译链接，真实预设加载并改变信号 +4.42 dB，连续 14 次运行退出码全 0（含退出时析构） |
| M2 | 驱动安装/卸载 + WASAPI 回环链路 | ✅ 完成。真机验证：1 kHz 正弦输入 0.1 → 输出 `peak 0.200`（DSP 真处理，非直通），`drop 0`、`clipped 0`。驱动**安装/卸载**路径仍需管理员，按既定计划未实机执行 |
| M3 | 托盘常驻 + 预设切换 + 开机自启 | ✅ 完成。托盘态私有内存 **15.9 MB**（目标 < 20 MB），空闲 CPU 1.25% |
| M4 | 点托盘弹出调音小面板 | ✅ 完成。面板为**常驻线程**（winit 的事件循环是进程级单例，不能一窗一线程），关闭后释放 GL 上下文与字体图集 |
| M5 | 健壮性：热插拔、采样率不匹配、单声道、崩溃恢复 | ✅ 基本完成。采样率错配/单声道有单测并有配置开关；热插拔走 `IMMNotificationClient`；崩溃恢复＝`previous_default_id` 标记 + 下次启动自动归还。热插拔真机场景待补 |
| M6 | 打包分发 | ✅ 完成。`package.ps1` 产出 `FxMini-<version>-win64.zip`（exe + 驱动三件套 + 安装/卸载脚本 + 许可 + SHA256）|

### 「音频没生效」这一类的坑（M2 之后补的）

真正让功能生效的那一步——**把系统默认输出设备指向虚拟声卡**——在设计里写了、代码也写了，但**全项目没有一个调用点**。症状是最难查的一种：进程正常、日志正常、预设已加载、图也建起来了，只是听不出任何区别。根因是虚拟声卡是个死胡同，没人往它写数据，引擎处理的是静音。现在由 [`src/routing.rs`](src/routing.rs) 接管与归还，见 [`docs/设计方案.md`](docs/设计方案.md) 第 6 节「坑 2」。

顺带发现 `device.rs` 里手写的 `IPolicyConfig` 少解引用一层（把接口指针当成了 vtable），这段代码是**第一次真正执行**，一跑就段错误。COM 是两级间接：接口指针指向对象，对象首字才是 vtable。

---

## 许可

本项目的 DSP 部分衍生自 [`fxsound2/fxsound-app`](https://github.com/fxsound2/fxsound-app)（Copyright © 2025 FxSound LLC，AGPL-3.0），因此整体以 **AGPL-3.0-or-later** 授权，全文见 [`LICENSE`](LICENSE)。自用无碍；若要分发，你的修改也需以 AGPL-3.0 开源。

`vendor/` 下的源码是上游文件的逐字副本（外加 [`patches/README.md`](patches/README.md) 记录的补丁），各自保留原许可；`vendor/LICENSE.fxsound-app` 即上游随源码附带的许可证文本。

驱动二进制来自 FxSound 的签名包，由 `package.ps1` 原样分发，装/卸需要管理员权限。**"FxSound" 名称与图标归其所有者所有**——FxMini 是独立的、与 FxSound 无关联的项目，请不要把它当作 FxSound 的官方产品。
