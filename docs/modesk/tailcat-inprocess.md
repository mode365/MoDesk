# MoDesk 加密隧道（tailcat / mesh tunnel）in-process 重构

> 状态：进行中（P1 完成、P2 构建接线已落地，待 CI 出包与真机联调）
> 建立日期：2026-11-19
> 分支：v2-brand

## 1. 背景与问题

MoDesk 的"加密隧道"基于 [tailscale/tailcat](https://github.com/tailscale/tailcat)（Tailscale 数据面：WireGuard-over-UDP + DERP 兜底，userspace 实现，无需 root/tun）。原实现把 tailcat 作为**外部 CLI 子进程**（`tailcat serve <port>` / `tailcat forward <addr> 0:<port>`）由 `src/tailcat.rs` spawn。

### 1.1 安卓端故障现象

安卓 APK 作为**主控**连接时，弹出"隧道不可用，是否回退到普通连接"确认框。

### 1.2 根因（已由真机日志确认）

手机日志（`rs_r2026-11-19_21-08-14.log`）关键行：

```
INFO  [src/client.rs:1183] mesh tunnel: relay response advertises tunnel tcpGFwWCBBddplUCLnERuhse77nK1uUMYFKMM9LdZrG1eQkuttcWFrWCBGjgzjhZ5cLHxgrhXwIdt8hN4Ls06xUgoqyxdGQYH3LGFxWCCi-kVhnLt4oV7XI9-jN93Po-h2NyGFhWTOpWN5vwDUJmFpGQEu:21118
ERROR [src/tailcat.rs:390] tailcat forward failed: tailcat binary not found
WARN  [src/client.rs:1242] tailcat forward unavailable
```

排除链：
- APK 打包**正确**：workflow 为每个 ABI 交叉编译 tailcat 并放入 `jniLibs/<abi>/libtailcat.so`，`build.gradle` 已设 `jniLibs { useLegacyPackaging true }`（so 会解压成 `nativeLibraryDir` 下的真实文件）。
- 根因在 `tailcat.rs::android_binary_path()`：它经 **`ndk_context`** 取 JNI 上下文查询 `ApplicationInfo.nativeLibraryDir`，但**全工程从未初始化 `ndk_context`**（无 `init_android_ndk_context` 调用；Kotlin 侧只 `System.loadLibrary`，不传 Context），`android_context()` 返回空 → `attach_current_thread()` 失败 → 路径解析返回 `None` → "binary not found"。

### 1.3 更深层的架构问题（即使路径修好也无法工作）

安卓标准 app（SELinux `untrusted_app` 域）**不允许 exec 子进程**；tailcat 的 WireGuard 隧道在桌面靠 userspace UDP（无 tun）可以跑，但 **spawn `Command::new()` 这一步在安卓会被拒绝**。因此"子进程"模型在安卓根本不可行 → 必须 **in-process（进程内加载库）**。

## 2. 方案决策（已确认）

| 决策点 | 结论 |
| --- | --- |
| 路线 | **全平台统一 in-process**：不再 spawn 子进程；tailcat 编为 C 共享库（c-shared），由 App 进程 dlopen 加载，后台线程/本地桥转发 |
| 绑定层 | 直接使用**上游 `cmd/libtailcat`**（自带版本化 C ABI），不自写绑定 → 代码量最小、合并友好 |
| 版本锁定 | tailcat **main** 分支（v0.7.0 无绑定层）：伪版本 `v0.7.1-0.20261119042437-c14b171db436`（commit `c14b171db436`，2026-11-19）；带绑定层的正式版发布后应升级 |
| Go 工具链 | go.mod 要求 **go ≥ 1.27.4**；本机用 `GOTOOLCHAIN=auto` 自动拉取（不动系统 1.23）；CI 已 setup-go 1.27 |
| 会话接入 | **保持 `client.rs` 零改动**：Rust 桥接层把隧道连接映射为本地 `127.0.0.1:<port>`，远控会话仍连本地端口 |
| 安卓打包 | 不变：`jniLibs/<abi>/libtailcat.so` + `useLegacyPackaging true`（dlopen 会成功——so 在进程自身 nativeLibraryDir） |

## 3. 上游 C ABI 速查（internal/capi, ABI v1）

全部为 `libtailcat`（c-shared）导出函数，错误经 `int32_t` 返回码 + `err **char` 字符串（用 `tc_free` 释放）：

| 函数 | 用途 |
| --- | --- |
| `tc_abi_version` / `tc_build_info` | ABI 版本 / 构建信息 |
| `tc_token_new(ns, out)` / `tc_token_cancel` / `tc_drain` | 取消令牌（超时控制）；Drain 排空 TCP |
| `tc_server_new(config) → handle` | 被控端创建。config JSON：`{"key","preshared_key","region","region_id","derp_map_url","allowed_clients","udp_idle_timeout"}` |
| `tc_server_start(h)` / `tc_server_listen(h, port, TCP) → listener` | 启动/监听（address 由下面 Info 取） |
| `tc_listener_accept(h, token) → conn` | 接受入站连接（阻塞，受 token 超时约束） |
| `tc_client_new(config) → handle` | 主控端创建。config JSON：`{"address":"<对端tc地址>","key","derp_map_url"}` |
| `tc_client_dial(h, token, port, TCP) → conn` | 直连对端端口 |
| `tc_conn_read/write/close_write/readable` | 连接 I/O（handle+token） |
| `tc_info(h)` | JSON 元信息；**server 的 `address` 即 `tcXXX...` 隧道地址**（经 rendezvous 交换给对端，交换机制不变） |
| `tc_close` / `tc_free` | 关闭 handle / 释放字符串 |
| `tc_address_parse` / `tc_address_resolve` / `tc_key_generate` / `tc_client_ping` | 工具函数 |

关键语义：
- `tc_*` 阻塞调用通过 `tc_token_new(纳秒超时)` + `tc_token_cancel` 实现超时控制。
- `tc_conn_readable` 为非消费式就绪探测（连接池过期判断用）。
- server `Info().address` = `TailcatAddr()`，即现行 punch/relay 响应里广播的 `tcXXX:port` 中的地址部分——**rendezvous 交换协议完全不动**。

## 4. Rust 集成设计（src/tailcat.rs 重构）

### 4.1 不变的对外接口（调用方零改动）

`tunnel_switch_on` / `should_use_tunnel` / `set_tunnel_pending` / `current_serve_addr` / `ensure_tunnel_serve(port) → addr` / `ensure_tunnel_forward(addr, port) → local_port` / `stop_tunnel_serve` 及 fallback 确认机制全部保留签名与语义。

### 4.2 替换内部实现

- 加载：`libloading::Library`（或裸 dlopen/LoadLibrary）加载 libtailcat；Windows 同目录 `libtailcat.dll` / macOS `.dylib` / Linux 与 Android `libtailcat.so`（Android 直接 `dlopen("libtailcat.so")`，落在 nativeLibraryDir）。
- `ensure_tunnel_serve`：`tc_server_new("{}")` → `tc_server_start` → `tc_server_listen(port, TCP)` → `tc_info` 取 `tcXXX` 地址；accept 循环线程：`tc_listener_accept` → 启动桥线程把 conn 与**本地回环 TCP（连 `127.0.0.1:<被控端口>`，即 rustdesk 直连监听口）**对接。
- `ensure_tunnel_forward`：`tc_client_new({address})` → `tc_client_dial(port, TCP)` → 桥线程把 conn 与**本地新开监听 `127.0.0.1:0`** 对接并返回该端口 → `client.rs` 连本地端口（与原 forward 子进程行为一致）。
- 生命周期：句柄表 + Drop 兜底 `tc_close`；Go runtime 首次调用自动拉起，无需常驻线程做 keepalive（c-shared 模型）。
- 超时：`tc_token_new(30s)` 对应原 `SPAWN_TIMEOUT_SECS`。

### 4.3 桥线程（双向转发）

```
loop {
  tc_conn_read(conn, buf)   ↔   TcpStream::write
  TcpStream::read           ↔   tc_conn_write(conn, buf)
}
```
任一方向 EOF/错误 → `tc_conn_close_write` + tc_close + 断开本地侧。

## 5. 构建接线（改动点清单）

已落地（P2）：

- [x] `res/tailcat/fetch.ps1` / `fetch.sh` **已删除**：不再下载上游 CLI 产物；改为各 CI job 固定 tailcat main 伪版本源码构建 `-buildmode=c-shared`。`res/tailcat/LICENSE` 保留（上游归属声明）。
- [x] `.github/workflows/flutter-build.yml` 五处：
  - [x] Windows（`build-for-windows-flutter`）：setup-go 1.27.4 → `go mod init` + `go get tailcat@伪版本` → `CGO_ENABLED=1 go build -buildmode=c-shared -o libtailcat.dll` → `res/tailcat/libtailcat.dll`。x86_64 用 runner 自带 MinGW gcc（`C:\msys64\mingw64\bin\gcc.exe`，与本机验证工具链一致）；**arm64 用 llvm-mingw 的 `aarch64-w64-mingw32-clang` 交叉编译**（MinGW gcc 无 arm64-windows 目标；x64 工具链在 windows-11-arm 上走内置模拟）。产物仅依赖 KERNEL32 + UCRT（`api-ms-win-crt-*`，Win10+ 自带），可安全分发。
  - [x] macOS（`build-for-macOS`）：setup-go 1.27.4 → c-shared `libtailcat.dylib` → `RustDesk.app/Contents/MacOS/`（dlopen 位于 exe 同目录）。**必须在下发签名前就位**；`sign-macos-app.sh` 已扩展：递归签 `Contents/MacOS` 下除主程序外的 Mach-O（否则 hardened runtime 的 library validation 会拒绝加载未签名 dylib）。
  - [x] Android per-ABI job（`build-rustdesk-android`）：`CGO_ENABLED=1 GOOS=android`，`CC=${ANDROID_NDK_HOME}/toolchains/llvm/prebuilt/linux-x86_64/bin/<triple>21-clang`（API 21，与 `cargo ndk --platform 21` 对齐；NDK r28c）→ `jniLibs/<abi>/libtailcat.so`。triple 对应：`aarch64-linux-android21` / `armv7a-linux-androideabi21` / `x86_64-linux-android21` / `i686-linux-android21`。
  - [x] Android universal job（`build-rustdesk-android-universal`）：同上，4 个 ABI 循环构建；`ANDROID_NDK_HOME` 由 ubuntu-24.04 镜像默认导出（NDK 27.3）。
  - [x] Linux（`build-rustdesk-linux`）：**在 runner 上（容器外）原生构建** `res/tailcat/libtailcat.so`（ubuntu18.04 构建容器内无 Go）；x86_64 跑在 ubuntu-22.04、aarch64 跑在 ubuntu-22.04-arm，均原生编译。容器内原 fetch.sh 调用已移除。注意：该 so 在 glibc 2.35 宿主上编译，若目标发行版过老（<2.27）需改为容器内装 Go 编译。
- [x] 桌面 CMake / build.py：`flutter/windows/CMakeLists.txt` 安装 `libtailcat.dll`（存在才装，缺失则跳过并日志提示）；`build.py` deb 打包把 `res/tailcat/libtailcat.so` 拷到 `/usr/share/rustdesk/`（与主程序同目录，`current_exe()` 解析后即该目录；`postinst` 的 `/usr/bin/rustdesk` 为符号链接不影响）。rpm 保持原状（此前也未含 tailcat）；**MSI 自动收割**（`preprocess.py` 对 `rustdesk/` 便携目录 glob 生成组件，dll 自动入包并在 Windows 签名步骤随 `.dll` 一起签）；**AppImage / Flatpak 由 deb 转换，自动带入**。
- [x] 保持 `useLegacyPackaging true`（jniLibs 解压为真实文件，dlopen 按 so 名解析）。
- [x] `.gitignore` 增加 `/res/tailcat/libtailcat.{dll,dylib,so,h}` 与旧 CLI 产物，避免误提交。

不在 P2 范围：iOS（此前也无 tailcat 步骤，保持无隧道）；Windows sciter / Linux sciter 构建（此前未含 tailcat，保持原状）。

## 6. 版本与合并策略

- tailcat 依赖固定为伪版本 `v0.7.1-0.20261119042437-c14b171db436`（main, 2026-11-19）。
- 上游发布含 `cmd/libtailcat` 的正式版后，升级到该 tag（预期 API 稳定：ABI v1 + `tc_abi_version` 运行时校验）。
- 与 rustdesk 上游合并：改动收敛在 `src/tailcat.rs`（自有文件）+ workflow 少量行 + build.gradle packaging 注释块，冲突面小。

## 7. 验证计划

- **P1 桌面**（本机）：libtailcat.dll 编出 → Rust 封装 `cargo check` → 本机 E2E：serve/forward 打通 + 加密会话回环（Windows 主控 ↔ Windows 被控）。
- **P2 安卓**（CI + 真机）：CI 出各 ABI c-shared → APK → 真机主控连接走隧道（对照 rs_*.log：不再出现 "binary not found"，出现成功路径日志）。
- **P3 回归**：全部平台隧道回归 + 清理旧子进程代码路径（`TunnelServe/TunnelForward` 的 spawn 逻辑、fetch 脚本旧路径）。

## 8. 进度跟踪

| 日期 | 事项 | 状态 |
| --- | --- | --- |
| 2026-11-19 | 真机日志确认根因（binary not found；排除打包） | ✅ |
| 2026-11-19 | 调研确认上游 `cmd/libtailcat` C ABI（仅 main 有） | ✅ |
| 2026-11-19 | 方案确认：全平台 in-process、锁 main、GOTOOLCHAIN=auto | ✅ |
| 2026-11-19 | 本地编 libtailcat.dll（windows amd64 c-shared，40.4MB；13 导出函数齐全；ABI v1） | ✅ |
| 2026-11-19 | Rust in-process 封装重写 src/tailcat.rs（公共 API 签名零变化）；cargo check 0 error、0 新警告 | ✅ |
| 2026-11-19 | 探针全链路验证（独立 exe + manifest 宿主）：serve→accept→dial→echo 回环穿过 WireGuard 隧道，双 payload 成功，debug/release 双构建均过 | ✅ |
| 2026-11-20 | P2 构建接线落地：workflow 五处（Windows/macOS/Android×2/Linux 全部改为固定伪版本源码构建 c-shared）、`build.py`（deb 带 `libtailcat.so`）、Windows CMake（`libtailcat.dll`）、`sign-macos-app.sh`（签 Contents/MacOS 下额外 Mach-O）、删除 fetch 脚本、gitignore | ✅ |
| 2026-11-20 | 本地校验：workflow YAML 解析通过（18 jobs）、`build.py` 语法通过；run 44634333321（P1 提交 ead5e0410）仍进行中 | ✅ |
| 待办 | 新接线首次 CI 出包验证（需用户授权触发）+ 安卓真机联调（P2 收口） | ⏳ |
| 待办 | 清理与全平台回归（P3） | ⏳ |

### 8.1 已解决/绕开的环境问题

- **wingoes "incoherent Windows version" panic**：tailscale 的 WinHTTP 代理探测库 wingoes 要求宿主 exe manifest 声明 `supportedOS` GUID（Win10 `{8e0f7a12-...}`）。modesk.exe 的 flutter runner 已内嵌该清单（已验证），正式客户端无此问题；独立测试程序需手动注入 manifest（`mt.exe -manifest xx.manifest -outputresource:xx.exe;#1`）。
- **goproxy.cn HTTP/2 断流**：拉 `tailscale.com` 大 zip 时 `INTERNAL_ERROR` 断流；`GODEBUG=http2client=0` 强制 HTTP/1.1 解决。
- **`go mod tidy` 慎用**：在桩模块上 tidy 会把仅 require 的 tailcat 依赖删掉导致 build 找不到包，用 `go get tailcat@main` + 直接 build。
- **wingoes 之外还有调用约定坑**：`let...else` 里的 `unsafe {}` 块需加括号；`tc_check` 等错误路径要注意释放语义。

### 8.2 遗留观察（非阻塞）

- **生产宿主 Go 初始化崩溃（已修复，2026-11-20）**：真实生产宿主（modesk.exe 便携版）双击时带 `libtailcat.dll` 即崩、去掉即正常（A/B 实锤）。procdump 转储解析确认为 `ACCESS_VIOLATION (0xC0000005)` **写访问**非法地址 `0x7ffea27c3de0`（位于 librustdesk.dll 基址下方约 73MB、非任何模块 = Go 堆/mmap 区），即 **Go runtime（c-shared）在宿主进程内初始化时写坏指针**；崩溃线程栈涉及 mswsock/crypt32/combase（网络/代理方向）。根因：`src/tailcat.rs` 的 `tc()` 用默认小栈 `std::thread::spawn` 加载并触发 Go runtime 初始化，而 Go 初始化 + tailscale 网络栈需大栈——P1 探针成功条件即"16MB 栈"，生产宿主此前仅缺这一项。**修复：加载线程改 `thread::Builder::stack_size(16MiB)`。** 修复后 `tailcat_tunnel_e2e`（librustdesk 测试宿主）不再在初始化崩溃，可走到 serve 逻辑（P1 时初始化即崩）。
- **残留：Go 库在 panic 退出阶段仍 `0xc0000005`（观察）**：e2e 测试因 live 网络连不上 DERP 在 `expect("serve failed")` panic 后，进程退出时仍返回 `STATUS_ACCESS_VIOLATION`。属 Go c-shared 在进程异常退出/panic unwind 时与宿主交互问题；**生产正常使用（Go 常驻、无 panic 退出）不触发**，未阻塞，列为观察。
- **首启偶发 dial 超时**：一次 E2E 中 client_dial 30s 超时（`context deadline exceeded`），复跑即通；疑为 DERP 注册传播窗口。生产侧 serve 有 15s 重试冷却+复用逻辑，会话侧 client.rs 有 Retry/fallback 交互。

## 9. 已知问题与备注

- **goproxy.cn HTTP/2 流断**：`tailscale.com` 大包（几十 MB）经 goproxy.cn 会 `stream error: INTERNAL_ERROR` 断流。解决：`GODEBUG=http2client=0` 强制 HTTP/1.1 + 重试。CI（GitHub runner 直连 proxy.golang.org）无此问题。
- 本机 Go 1.23 + `GOTOOLCHAIN=auto` 自动下载 go1.27.4（toolchain 缓存在用户 Go 目录），对系统 Go 无侵入。
- c-shared 要求 `CGO_ENABLED=1`（本机 gcc 14.2 MinGW-w64 已验证可用；Android 需 NDK clang；CI 全具备）。
- Android `dlopen("libtailcat.so")` 无需路径解析（linker 命名空间内按 so 名加载），彻底绕开此次根因中的 `nativeLibraryDir` JNI 查询。

> AI生成
