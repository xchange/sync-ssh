# SyncSSH

SyncSSH 是基于 [Aurora Terminal](../aurora-terminal) 0.1.6 开发的 Rust + GPUI 原生 SSH 客户端，面向终端优先的紧凑工作区。当前版本 **0.1.2**。

## 0.1.2 设置更新

设置窗口采用左侧菜单、右侧内容的分栏布局，最大为 960 × 780 px，并随主窗口缩小。菜单包括外观、终端、光标与特效、凭据、备份、终端工具、关于；切换项目会保留未保存的设置草稿，常规设置底部固定提供“保存并应用”和“恢复默认”。备份默认选择位于 S3 左侧的“本地”标签；云服务在右侧直接编辑各供应商配置，底部提供独立的保存、备份与恢复操作；供应商切换及设置菜单切换均保留当前会话内的草稿，恢复验证后的确认也在右侧完成。

光标闪烁默认开启，已有明确保存的闪烁选择继续保留。输入特效控制键入粒子与光标光晕，鼠标特效独立控制移动轨迹；两个开关分别保存并立即应用于全部终端。旧版 `effects` 总开关会自动迁移为相同状态的两个开关，避免升级后重新开启已关闭的特效。

“终端”中的终端字体与备份字体使用可搜索的下拉框，列出系统已安装字体及应用内置字体，无需手动输入名称。按字符检查覆盖范围；主字体缺字时使用备份字体，再尝试系统字体。首次启动时，macOS 使用 `Menlo` + `Monaco`，Windows 使用 `Consolas` + `Lucida Console`；两平台自动补充已安装的系统中日韩字体，并以内置 Nerd Font 补充目录和提示符图标。默认字体依据 [Apple 系统字体列表](https://support.apple.com/en-ie/120414)与 [Windows 系统字体列表](https://learn.microsoft.com/en-us/typography/fonts/windows_11_font_list)选择。Linux 的两个默认值均为 `monospace`，字体安装、匹配与 CJK 覆盖由用户配置。已有主字体与字号等选择会保留，旧设置仅补上缺失的备份字体字段。

## 主要变化

- 软件、可执行文件、应用包和本机数据目录使用独立的 SyncSSH 标识。
- 在 S3 / WebDAV 加密备份基础上，新增阿里云 OSS、华为云 OBS、腾讯云 COS 和火山引擎 TOS。各供应商独立保存凭据、恢复草稿及自动备份开关，可同时备份。
- 移除跳板机配置、依赖检查、多跳连接及其测试夹具；SSH 始终直连目标。
- 新增 / 编辑连接使用统一表单，认证方法只有“密码”和“私钥 / 证书”两个单选项；切换时保留未保存输入。私钥可携带口令和可选的 OpenSSH 用户证书。
- 终端上方只保留紧凑标签栏。云备份、主密码、锁定、特效、SFTP、SSH 隧道和 ZMODEM 操作集中在设置中；新建本地终端、分屏与搜索使用已有入口或快捷键，终端工具页保留会话相关操作。
- 设置入口位于左侧栏底部、版本信息上方。侧栏可折叠为 48 px 图标栏，折叠状态保存在本机；展开宽度为 244 px。
- 正常连接时隐藏连接状态栏与底部状态栏，断线、处理中或错误时按需显示。

保留本地 PTY、真实 SSH、主机指纹验证、分屏、SFTP、SSH 端口转发、重连、终端搜索、ZMODEM、内置 Nerd Font、主题及本地加密凭据库。

## 运行与构建

安装 stable Rust（工作区要求 Rust 1.89 或更新版本），macOS 需要 Xcode Command Line Tools。首次构建需要下载依赖。

```sh
cargo run --locked -p sync-ssh
# 使用独立的测试数据目录
cargo run --locked -p sync-ssh -- --data-dir /absolute/path/to/syncssh-demo
# 发布构建
cargo build --locked --release -p sync-ssh
```

可执行文件为 `target/release/sync-ssh`，Windows 为 `sync-ssh.exe`。GPUI 固定为 0.2.2，gpui-component 固定为 0.5.1，仓库包含锁定依赖和终端组件补丁。

macOS 应用包：

```sh
./scripts/build-macos.sh release
./scripts/build-macos.sh debug
# 第二个参数可指定输出目录，默认 dist
./scripts/build-macos.sh debug ./dist
open dist/SyncSSH.app
```

生成包含第二版应用图标的 `dist/SyncSSH.app`，执行本地 ad-hoc 签名。应用尚未进行 Developer ID 签名或公证。旧的 `bundle-macos.sh [目录] [debug|release]` 接口继续可用。

Windows 单文件应用（在 Windows PowerShell 中执行）：

```powershell
.\scripts\build-windows.ps1 -Mode release
.\scripts\build-windows.ps1 -Mode debug
# 可选 -OutputDirectory 和 -Target（MSVC）
```

输出 `dist/SyncSSH.exe`，图标、版本信息和应用资源嵌入可执行文件；脚本使用 MSVC 静态 CRT，无需随附应用 DLL。

## GitHub 自动发布

下载地址：[最新 Release](https://github.com/xchange/sync-ssh/releases/latest)。推送 `v` 开头的版本标签会触发 `.github/workflows/release.yml`，分别在 macOS Apple Silicon、macOS Intel 和 Windows x64 上构建 release 版本。测试和三个平台的打包全部成功后，自动创建 Release 下载页，上传两个包含 `.app` 的 ZIP、一个 Windows 单文件 EXE，以及 `SHA256SUMS.txt` 校验文件。

发布前修改根目录 `Cargo.toml` 中的工作区版本并更新 `Cargo.lock`，提交并推送代码，再创建完全匹配的标签：

```sh
git tag v0.1.2
git push origin v0.1.2
```

也可在 Actions 中手动运行 **Build and publish Release**，输入已有版本标签。脚本验证标签、应用包版本和平台架构；应用内版本信息从 Cargo 自动生成。发布使用仓库的 `GITHUB_TOKEN`，无需另存个人令牌；失败时不会发布不完整的下载页。已公开的版本不会被重复覆盖，应使用新版本号发布更新。

Linux 需要桌面环境、GPU / Vulkan 和 GPUI 的原生依赖；Windows 需要 Rust MSVC、Visual Studio C++ Build Tools、Windows SDK 及支持 ConPTY 的 Windows 10/11。三平台依赖与验证命令见 `.github/workflows/ci.yml`；本次开发环境为 macOS Apple Silicon。

## 使用

1. 首次运行创建主密码并确认，至少 12 个普通 ASCII 字符（英文、数字、半角符号或空格）；也可以选择仅使用本地终端。
2. 在侧栏“服务器连接”旁点击 `＋`，填写名称、主机、端口、用户名和分组。
3. 在“认证方法”中选择密码或私钥 / 证书，填写对应凭据并加密保存。
4. 点击服务器连接。首次连接需核对 SHA256 主机指纹；验证主机前不会发送登录凭据，主机密钥变化会拒绝连接。
5. 通过左下角“设置”的对应菜单使用云备份、主密码、终端工具和个性化。侧栏折叠后仍可使用底部齿轮进入设置。

私钥和证书旁的“选择文件…”会打开系统文件选择窗口，选择后自动填入路径；取消选择会保留原值。也可直接填写本机文件的绝对路径，不展开 `~`。应用把文件内容保存在加密凭据库，云端恢复无需保留原路径。证书需要对应私钥，不能单独认证。编辑时密码留空保留原密码；私钥路径留空保留原私钥；可显式清除旧口令或证书。选择新私钥不会自动继承旧口令或旧证书。

历史 Agent 凭据仍可用于已保存连接，新的表单只提供上述两种认证方法；编辑历史 Agent 连接时需改用密码或导入私钥。旧凭据库中的跳板字段在读取后忽略，连接使用目标地址直连。

## 本地凭据库与网盘目录

进入 **设置 → 备份 → 本地**（默认标签）。创建或解锁凭据库后，点击“选择保存位置并使用”，把加密文件保存到选定路径，后续修改持续更新同一文件，原文件保留。可选本地磁盘、移动硬盘或任何网盘同步目录；程序只报告本地保存结果，上传与同步由网盘客户端负责。

也可以选择已有 `.vault` 文件，输入该文件的主密码，验证服务器数量后确认切换。切换会关闭当前 SSH 会话，保留原凭据库；恢复副本保存在工作文件旁的 `.previous` 文件中。

保存、密码修改和恢复前检测文件内容是否发生外部变化，冲突时拒绝覆盖，需锁定后重新解锁。所选文件尚未同步、目录被移动或磁盘未挂载时，不会创建空库替代。此功能适合完成同步后切换设备使用，不会合并多设备同时修改的数据。

## 对象存储加密备份

进入 **设置 → 备份**，选择供应商，填写 Bucket、Region、Access Key ID、Secret Access Key，临时凭据另填 Session Token。Endpoint 留空时按供应商与 Region 自动选择；填写自定义地址时使用 HTTPS 服务域名，不要带 Bucket、路径或查询参数。

| 供应商 | 默认 Region | 自动选择的服务 Endpoint |
| --- | --- | --- |
| AWS S3 / S3 兼容 | `us-east-1` | AWS 区域端点；其他兼容服务可自定义 |
| 阿里云 OSS | `cn-hangzhou` | `https://s3.oss-{region}.aliyuncs.com` |
| 华为云 OBS | `cn-north-4` | `https://obs.{region}.myhuaweicloud.com` |
| 腾讯云 COS | `ap-guangzhou` | `https://cos.{region}.myqcloud.com` |
| 火山引擎 TOS | `cn-beijing` | `https://tos-s3-{region}.volces.com` |

四家云供应商使用虚拟主机寻址；腾讯云 Bucket 填完整的 `名称-APPID`，如 `syncssh-1250000000`。通用 S3 自定义端点沿用路径寻址，以支持兼容服务。华为云按其 V4 要求使用 `UNSIGNED-PAYLOAD`；所有流量仍经过 HTTPS，备份内容已在本机加密。阿里云使用 S3 兼容端点与兼容请求标记；账户若未开放 S3 兼容认证，需要先在服务端开通。

端点和兼容要求依据供应商官方文档：[阿里云 OSS](https://help.aliyun.com/zh/oss/developer-reference/use-aws-sdks-to-access-oss)、[华为云 OBS](https://support.huaweicloud.com/obs_faq/obs_faq_0274.html)、[腾讯云 COS](https://intl.cloud.tencent.com/document/product/436/32537?lang=en)、[火山引擎 TOS](https://docs.volcengine.com/docs/torch-object-storage/accessing-tos-using-awss-3-sdk?lang=zh)。

点击“仅保存配置”可保存配置或自动备份开关，不执行上传。点击“保存并备份”上传独立加密快照，成功后回填恢复对象 Key。默认前缀为 `syncssh/backups`，对象名含 UTC 时间和随机标识。恢复需完整对象 Key 与创建备份时的主密码；验证通过后在同一设置内容区确认替换本地凭据库。锁定页的“从云备份恢复”也会直接打开设置中的云备份菜单。

各供应商的自动备份独立执行，保存 / 编辑 / 删除连接时触发；某个目的地失败不会跳过其他目的地，也不影响本机保存。WebDAV 保留 HTTPS 集合地址、用户名 / 密码及独立开关。

这是快照备份和手动恢复，不是多设备实时双向同步。未配置真实云账户时不能验证账户权限、区域可达性或具体服务端兼容差异。

## 快捷键

| 操作 | macOS | Windows / Linux |
| --- | --- | --- |
| 设置 | `⌘,` | `Ctrl+,` |
| 收起 / 展开侧栏 | `⌘B` | `Ctrl+Shift+B` |
| 新建本地终端 | `⌘T` | `Ctrl+Shift+T` |
| 左右分屏 | `⌘D` | `Ctrl+Shift+D` |
| 上下分屏 | `⌘Shift+D` | `Ctrl+Alt+Shift+D` |
| 关闭当前窗格 | `⌘Shift+W` | `Ctrl+Shift+W` |
| 终端搜索 | `⌘F` | `Ctrl+Shift+F` |
| 锁定凭据库 | `⌘Shift+L` | `Ctrl+Shift+L` |
| 复制终端选区 | `⌘C` | `Ctrl+Shift+C` |
| 粘贴 | `⌘V` | `Ctrl+Shift+V` |

一个工作区最多 4 个独立会话。SFTP 和隧道使用当前窗格已验证的 SSH 连接；关闭窗格或锁定凭据库会关闭相应 SSH 连接。锁定时保留本地 Shell。

## 数据与安全

默认数据目录由 `directories::ProjectDirs::from("dev", "SyncSSH", "SyncSSH")` 生成，独立于 Aurora Terminal。可用 `--data-dir` 覆盖。目录中默认包含 `credentials.vault`、恢复副本、`known_hosts.json`、`preferences.json` 和 `vault-location.json`。选择其他凭据库位置后，加密文件与恢复副本位于所选目录，位置指针仍保存在本机；个性化、侧栏状态及位置指针不进入云备份。

凭据库使用 Argon2id 派生密钥和 XChaCha20-Poly1305 加密；主密码不保存或上传。保留上游 v1 加密封装格式，原有 S3 / WebDAV 配置可读取。更换主密码不会重新加密已有云端快照，恢复旧快照仍需旧密码。

SFTP 文件和 ZMODEM 文件不进入凭据库。SFTP 支持队列、目录传输和当前标签内校验续传，已有文件不覆盖；远端可能保留 `.syncssh-upload-*.part` 临时文件。ZMODEM 远端需提供 `rz` / `sz`，本机无需安装。

## 验证

```sh
cargo fmt --all --check
cargo test --locked --workspace
cargo test --locked -p gpui-terminal --lib
# macOS / Windows 原生字体回退验证（主线程运行，不创建窗口）
cargo test --locked -p sync-ssh --test native_font_fallbacks
cargo build --locked -p sync-ssh
# 安装 lrzsz 后，运行实际协议互操作测试
cargo test --locked -p syncssh-session --test zmodem_loopback -- --ignored
```

测试覆盖加密与恢复、供应商配置隔离、S3 兼容签名与寻址、SSH 主机身份验证、密码 / 私钥 / 证书认证、SFTP、隧道、重连、终端布局、字体和搜索。需要本地回环 TCP / PTY 的测试应在允许这些操作的环境中运行。真实云服务需使用各自账户做最终联调。

项目遵循 MIT 许可，保留 Aurora Terminal 的版权声明。终端组件及其补丁说明见 `vendor/gpui-terminal/AURORA_CHANGES.md`，第三方许可证保留在 `vendor/` 和 `crates/syncssh-app/assets/fonts/`。
