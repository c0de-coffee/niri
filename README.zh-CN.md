<div align="right">
  <a href="README.md">English</a> | <strong>简体中文</strong>
</div>

# niri 远程登录 fork

> [!NOTE]
> 这是 [niri-wm/niri](https://github.com/niri-wm/niri)（作者 Ivan Molodetskikh / YaLTeR）的
> fork，由 [c0de-coffee](https://github.com/c0de-coffee) 维护。**niri 本身的介绍、文档和配置说明请看原仓库**：
> https://github.com/niri-wm/niri 。这里只写 fork 新增的内容。
>
> 远程登录相关的问题请在本仓库提，不要报给 niri 上游。
>
> 这个 fork 的工作是用 Claude 完成的。

## 这个 fork 做了什么

让 niri 像 GNOME 一样支持 **RDP 远程登录**：

- 用远程桌面客户端连上这台电脑，先看到登录界面，输入系统账户密码后，进入**自己独立的 niri 会话**。
- 远程会话是无头的：不占用、也不显示在这台电脑的屏幕上。
- 断开连接后会话继续运行，再连上会回到原来的会话。
- 多个账户可以同时远程登录，各用各的会话。
- 服务端用 Arch 官方仓库里的 gnome-remote-desktop 包，远程登录不需要修改它。另有两个可选补丁，分别修复剪贴板里的非英文字符和多显示器的排列（见[已知问题](#已知问题和限制)）。

由两部分组成：

1. **niri**（`remote-desktop` 分支）新增了：
   - mutter 的 `org.gnome.Mutter.RemoteDesktop` 接口：通过 libei 接收键盘鼠标输入，支持剪贴板，同步大写锁定和数字锁定状态。
   - `org.gnome.Mutter.ScreenCast.RecordVirtual`：虚拟屏幕，大小跟着客户端窗口变。客户端开了多显示器时，每块屏幕一个虚拟屏，缩放比例和客户端那块屏一样。
   - `niri --headless`：用显卡渲染，但没有物理屏幕。
   - 在本机正常使用时，和原版 niri 没有区别。
2. **[remote-dm](remote-dm/README.zh-CN.md)**：只负责远程登录的显示管理器。它在 gnome-remote-desktop 面前扮演 GDM，在登录界面面前扮演 greetd。连接流程、客户端设置和排错都在它的 README 里。

## 快速开始（Arch Linux）

1. **安装软件包**：从 [Releases](https://github.com/c0de-coffee/niri/releases) 下载
   `niri-remote-desktop` 和 `remote-dm` 两个包，然后安装：

   ```sh
   sudo pacman -U niri-remote-desktop-*.pkg.tar.zst remote-dm-*.pkg.tar.zst
   ```

   `niri-remote-desktop` 会替换官方的 `niri` 包。也可以用 [`packaging/`](packaging/) 里的 PKGBUILD 自己编译：
   `niri-remote-desktop`、`remote-dm` 编译 Release 版本，`*-git` 编译分支上的最新代码。

2. **配置**：按 [remote-dm 的 README](remote-dm/README.zh-CN.md#安装) 设置 gnome-remote-desktop，再启用 remote-dm。

   如果你用 Nyxuri 管理桌面配置，也可以用我维护的 Nyxuri fork
   [c0de-coffee/Nyxuri-remote](https://github.com/c0de-coffee/Nyxuri-remote)（原版 Nyxuri 没有这个功能）：
   执行 `nyxuri on remote-login` 会自动完成上面的配置，还带一个 Noctalia 设置面板。

3. **连接**：见 [客户端连接](remote-dm/README.zh-CN.md#客户端连接)。用 Windows 自带的远程桌面连接时，要先在 `.rdp` 文件里加一行设置，否则会连不上。

## 已知问题和限制

- **同一个账户不能同时在本机和远程登录**：两边会抢同一条会话总线。远程登录时如果这个账户正在本机登录，输完密码后登录界面会问要不要注销本机会话，确认才注销，否则取消登录。GNOME 46 起的登录界面也是先弹窗问，选择"强制停止"才注销本机会话。
- **多显示器**：客户端开了多显示器时，远程桌面会铺满客户端的每块屏幕，每块屏的缩放比例和客户端那边一样（GNOME 的远程登录也支持多显示器，gnome-remote-desktop 最多支持 16 块）。没打补丁的 gnome-remote-desktop 不会告诉合成器客户端那边几块屏幕谁左谁右、哪块是主屏，GNOME 也有同样的问题。这时 niri 只能按客户端列出屏幕的顺序从左到右排，"焦点移到左边或右边显示器"这类快捷键的方向可能和你看到的相反，登录界面也可能出现在主屏以外的屏幕上。鼠标在屏幕之间移动不受影响。[`patches/gnome-remote-desktop/`](patches/gnome-remote-desktop/) 里的可选补丁 `0002` 能解决这个问题：打上以后，niri 会按客户端显示设置里的排列摆放屏幕，并把窗口开在主屏上。
- **剪贴板里的非英文字符**：在客户端那台电脑上复制文字，粘贴到远程会话里时，英文字母、数字和常用符号（ASCII）以外的字符，比如中文、日文、带音标的字母和 emoji，都会变成 `\u6d4b\u8bd5` 这样的转义形式。这是 gnome-remote-desktop 51 本身的 bug（遇上 FreeRDP 3.32 及以后版本才出现），用 GNOME 也一样，和这个 fork 无关。不处理也能正常远程登录，只是这个问题还在；想修好的话，要用 [`patches/gnome-remote-desktop/`](patches/gnome-remote-desktop/) 里的补丁重新编译 gnome-remote-desktop。在 Arch 上可以用 [`packaging/gnome-remote-desktop/build.sh`](packaging/gnome-remote-desktop/build.sh)，它会把两个补丁都打上。
- 目前只提供 Arch Linux 软件包。

## 仓库里的其他内容

| 目录 | 内容 |
|---|---|
| [`remote-dm/`](remote-dm/) | 远程登录显示管理器 |
| [`packaging/`](packaging/) | Arch 软件包的 PKGBUILD |
| [`patches/`](patches/) | 给 gnome-remote-desktop 的补丁 |
| [`remote-login-tests/`](remote-login-tests/) | 测试脚本（含 systemd-nspawn 测试容器） |

## 许可证和致谢

和 niri 一样使用 GPL-3.0-or-later；remote-dm 使用 GPL-3.0-only（部分代码来自 greetd）。

感谢 Ivan Molodetskikh（YaLTeR）和 niri 的所有贡献者，这里的一切都建立在 niri 之上；也感谢这些上游项目：[Smithay](https://github.com/Smithay/smithay)（包括它的 libei 支持）、[GNOME Remote Desktop](https://gitlab.gnome.org/GNOME/gnome-remote-desktop) 和 [mutter](https://gitlab.gnome.org/GNOME/mutter)（这里的远程登录照着它们做）、[greetd](https://git.sr.ht/~kennylevinsen/greetd)（remote-dm 用了它的登录界面协议和 PAM 会话代码），以及 [FreeRDP](https://github.com/FreeRDP/FreeRDP)。
