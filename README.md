# SyncSSH

一款简洁的 SSH 客户端，让终端拥有更多空间。

SyncSSH 将服务器连接、文件传输和凭据备份放在同一个桌面应用中。支持 Windows 和 macOS，提供可折叠侧栏、多标签与分屏，适合日常开发和远程服务器管理。

## 功能

- **SSH 连接管理**：按名称、主机或分组查找服务器，支持密码、私钥和 OpenSSH 用户证书认证，私钥与证书可直接选择本机文件。
- **终端工作区**：本地终端、多个标签、左右或上下分屏、终端搜索，以及连接断开后的重连。
- **文件与网络工具**：SFTP 文件和目录传输、断点续传、SSH 端口转发与 ZMODEM 传输。
- **个性化体验**：可搜索的系统字体选择、备份字体、中文显示支持、主题、光标和独立的输入与鼠标特效设置。
- **加密凭据库**：服务器密码、私钥和备份配置由主密码保护，可随时锁定。
- **灵活备份**：支持本地文件、网盘同步目录，以及 S3、阿里云 OSS、华为云 OBS、腾讯云 COS、火山引擎 TOS 和 WebDAV。

## 下载

在 [GitHub Releases](https://github.com/xchange/sync-ssh/releases/latest) 下载适合设备的版本：

| 平台 | 文件 | 使用方式 |
| --- | --- | --- |
| macOS · Apple Silicon | `macos-arm64.zip` | 解压后将 `SyncSSH.app` 放入“应用程序” |
| macOS · Intel | `macos-x86_64.zip` | 解压后将 `SyncSSH.app` 放入“应用程序” |
| Windows · x64 | `windows-x86_64.exe` | 单文件应用，下载后运行 |

## 开始使用

1. 启动应用，设置主密码；也可以先仅使用本地终端。
2. 点击左侧“服务器连接”旁的 `＋`，填写服务器地址与用户名。
3. 选择密码或私钥 / 证书认证，保存连接。
4. 打开连接，首次连接时核对服务器主机指纹。

主密码至少为 12 个普通 ASCII 字符，可使用英文、数字、半角符号和空格。

设置位于侧栏底部。常用快捷键如下：

| 操作 | macOS | Windows |
| --- | --- | --- |
| 打开设置 | `⌘,` | `Ctrl+,` |
| 收起 / 展开侧栏 | `⌘B` | `Ctrl+Shift+B` |
| 新建本地终端 | `⌘T` | `Ctrl+Shift+T` |
| 左右分屏 | `⌘D` | `Ctrl+Shift+D` |
| 上下分屏 | `⌘Shift+D` | `Ctrl+Alt+Shift+D` |
| 搜索终端 | `⌘F` | `Ctrl+Shift+F` |
| 锁定凭据库 | `⌘Shift+L` | `Ctrl+Shift+L` |

## 备份与恢复

在 **设置 → 备份** 中选择保存方式。默认使用本地备份，可以将加密凭据库保存到本地磁盘、移动硬盘或网盘同步目录；也可以配置云端备份，并在需要时恢复。

主密码不会保存或上传，恢复备份需要创建备份时使用的主密码。请妥善保管密码。

使用网盘同步目录时，建议完成同步后再切换设备。应用会检测外部修改，避免直接覆盖其他设备保存的内容；多设备同时编辑的数据不会自动合并。

## 从源码构建

需要 Rust 1.89 或更新版本。macOS 需要 Xcode Command Line Tools；Windows 需要 MSVC 工具链、Visual Studio C++ Build Tools 和 Windows SDK。

```sh
cargo run --locked -p sync-ssh
```

生成 macOS 应用包：

```sh
./scripts/build-macos.sh release
```

生成 Windows 单文件应用：

```powershell
.\scripts\build-windows.ps1 -Mode release
```

产物位于 `dist` 目录。两个脚本均支持 `debug` 模式。

## 许可证

[MIT](LICENSE)
