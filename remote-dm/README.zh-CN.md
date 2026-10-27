<div align="right">
  <a href="README.md">English</a> | <strong>简体中文</strong>
</div>

# remote-dm

只负责远程登录的显示管理器：用 RDP 连上这台电脑，看到登录界面，登录后进入自己的无头会话。
可以多人同时登录，断开后再连会回到原来的会话。

gnome-remote-desktop（下面简称 grd）的远程登录模式本来要配合 GDM 使用。remote-dm 只做了其中 grd 需要的那部分，不需要装 GDM：

- 对 **grd 的系统服务**，它实现 GDM 的远程显示接口（`org.gnome.DisplayManager` 的 `RemoteDisplayFactory`、`RemoteDisplay`），所以 grd 不用修改。
- 对**登录界面**，它使用 **greetd 的协议**，所以任何 greetd 登录界面都能用（默认是 regreet，也支持 [Noctalia Greeter](https://github.com/noctalia-dev/noctalia-greeter)）。
- 会话都是没有座位（seat）的远程 logind 会话。登录界面和用户会话默认都跑无头 niri。合成器必须实现 mutter 的 `RemoteDesktop`、`ScreenCast` 接口，包括虚拟屏幕（`RecordVirtual`），比如[我 fork 的这个 niri](../README.zh-CN.md)。

## 一次连接的过程

```
RDP 客户端 ──① 连接──▶ grd 系统服务（端口 3389，先验证 RDP 用户名和密码）
                         │
                         ▼
                     remote-dm ──② 启动──▶ 登录界面会话（rdm-greeter-N 账户）
                         │                  客户端第一次重定向，连到这里，看到登录界面
                         │ ③ 输入系统账户密码
                         ▼
                     用户会话（该账户自己的无头 niri）
                     客户端第二次重定向，连到这里；登录界面会话结束
```

1. grd 的系统服务接受 RDP 连接，向 remote-dm 请求一个远程显示（`CreateRemoteDisplay`）。
2. remote-dm 用一个空闲的登录界面账户（`rdm-greeter-N`，专门给登录界面用的系统账户，见[配置](#配置)）启动登录界面会话，里面运行无头 niri、`grd --handover`、登录界面和 `remote-dm session-manager`。grd 把连接交给这个会话。
3. 用户在登录界面登录。remote-dm 用 PAM 验证密码，然后启动用户会话（无头 niri 和 `grd --handover`）；如果这个用户已经有远程会话，就直接用那个。
4. grd 把连接交给用户会话，登录界面会话结束。

每次交接，RDP 客户端都要按 grd 发来的"服务器重定向"重新连接一次。Windows 自带的远程桌面连接（mstsc）默认不跟随重定向，要改 `.rdp` 文件，见下面的[客户端连接](#客户端连接)。
客户端收到重定向后 30 秒内没有连回来，grd 就会放弃这个连接，remote-dm 也会结束为它启动的登录界面会话。

## 安装

### 1. 安装软件包

在 Arch Linux 上，最省事的是直接装本仓库 [Releases](https://github.com/c0de-coffee/niri/releases) 里编好的包（同时装上带远程桌面的 niri）：

```sh
sudo pacman -U niri-remote-desktop-*.pkg.tar.zst remote-dm-*.pkg.tar.zst
```

也可以用本仓库 `packaging/` 里的 PKGBUILD 自己编译。`remote-dm` 编译 Release 版本，`remote-dm-git` 编译 `remote-desktop` 分支上已提交的最新代码：

```sh
cd packaging/remote-dm && makepkg -si
```

其他发行版：

```sh
./install.sh              # 编译，然后安装到 /usr 和 /etc（要 root）
DESTDIR=pkg ./install.sh  # 打包用
```

### 2. 配置

先给 grd 的系统服务设置 TLS 证书和 RDP 用户名密码，再启用服务：

```sh
sudo grdctl --system rdp set-tls-cert /path/to/cert.crt
sudo grdctl --system rdp set-tls-key /path/to/cert.key
sudo grdctl --system rdp set-credentials 用户名 密码   # RDP 用户名和密码，见下面"两层密码"
sudo grdctl --system rdp enable
sudo systemctl enable --now remote-dm
```

`remote-dm.service` 和 `gdm.service` 冲突，两个都要占用 `org.gnome.DisplayManager`，所以 GDM 必须先停用。本机登录可以用 greetd 等其他显示管理器。

想把登录界面换成 Noctalia Greeter，在 `/etc/remote-dm/config.toml` 里写：

```toml
[greeter]
command = "exec niri --headless --session -c /etc/remote-dm/greeter-noctalia.kdl"
```

如果你用 Nyxuri 管理 niri 桌面的配置，也可以用我维护的 Nyxuri fork
[c0de-coffee/Nyxuri-remote](https://github.com/c0de-coffee/Nyxuri-remote)（原版 Nyxuri 没有这个功能）：
执行 `nyxuri on remote-login` 会自动完成上面这些配置，登录界面用的是 Noctalia Greeter。

### 卸载

```sh
sudo systemctl disable --now remote-dm   # 会结束所有远程会话
sudo grdctl --system rdp disable
sudo systemctl enable gdm                # 要换回 GDM 的话
```

用 Nyxuri 配置的，执行 `nyxuri off remote-login`，会恢复成配置前的样子。

## 客户端连接

### 两层密码

连接时要输入两次密码，是两组不同的账号：

1. **RDP 用户名和密码**：客户端连接时问的那一组，只用来进入 grd，跟系统账户无关。用 `grdctl --system rdp set-credentials` 设置；用 Nyxuri 的话，也可以在 Noctalia 的"远程登录"面板里改。
2. **系统账户密码**：连上以后，在登录界面里选择账户、输入它的登录密码，跟在本机登录时一样。

### Windows 远程桌面连接（mstsc）

mstsc 默认不跟随服务器重定向，直接连会卡在第一步或者反复断开。需要用 `.rdp` 文件连接：

1. 打开"远程桌面连接"，填好计算机地址，点"显示选项"，在"常规"选项卡里点"另存为"，保存成 `.rdp` 文件。
2. 用记事本打开这个文件，在末尾加上这一行：

   ```
   use redirection server name:i:1
   ```

3. 以后双击这个 `.rdp` 文件连接。第一次连接会提示证书不受信任，核对证书指纹（见[排错](#证书提示)）后选择连接。

**多显示器**：想让远程桌面铺满客户端的所有屏幕，在"显示"选项卡里勾选"将我的所有监视器用于远程会话"并全屏连接，或者在 `.rdp` 文件里再加一行：

```
use multimon:i:1
```

几块屏幕谁左谁右、哪块是主屏，要 gnome-remote-desktop 打上我们的可选补丁，才会跟着 Windows 显示设置里的排列走，见这个 fork 的 README 里的[已知问题](../README.zh-CN.md#已知问题和限制)。

### FreeRDP

FreeRDP 默认就会跟随重定向：

```sh
xfreerdp3 /v:电脑地址 /u:RDP用户名 /p:RDP密码
# 多显示器加 /multimon，全屏加 /f
```

`sdl-freerdp3` 的参数一样。

### 1Remote

[1Remote](https://github.com/1Remote/1Remote) 要用 mstsc.exe 模式连接：

1. 在连接设置里启用 mstsc.exe 模式。
2. 在额外指令（附加 `.rdp` 设置）里加上：

   ```
   use redirection server name:i:1
   ```

   要用多显示器，再加一行：

   ```
   use multimon:i:1
   ```

### 其他客户端

Remmina、macOS 的 Windows App、手机上的远程桌面客户端都还没测试过。客户端必须支持 RDP 的服务器重定向（server redirection），否则进不了登录界面后面的会话。

### 断开、重连和注销

- 直接关掉客户端窗口只是断开，会话还在运行。再连接、在登录界面登录同一个账户，就会回到原来的会话。
- 想彻底结束会话，在远程桌面里注销就行：点桌面上的"注销"（比如 Noctalia 电源菜单里的"注销"），或者按 niri 的退出快捷键（默认 `Mod+Shift+E`）。也可以在这台电脑上执行 `loginctl terminate-user 用户名`。

### 账户已经在本机登录时

一个账户不能同时有本机会话和远程会话：两边的桌面会抢同一条会话总线，结果远程那边看到的是灰屏。GDM 也不允许这样。

所以远程登录时，如果这个账户正在本机登录，输完密码后登录界面会问要不要注销本机会话。输入 `yes` 会注销本机会话（**本机上没保存的内容都会丢失**），然后进入远程会话；输入其他内容就取消登录，本机会话不受影响。

## Kerberos 单点登录

配置了 Kerberos 后，grd 已经验证过用户身份，会把用户名告诉 remote-dm（`preauthenticated-user`）。remote-dm 就像 GDM 一样跳过登录界面，用 `remote-dm-autologin` 这个 PAM 服务（不需要密码）直接启动用户会话，或者回到已有的会话。如果这个用户正在本机登录，还是会先显示登录界面来提问。

需要一个 Kerberos 域，以及 grd 的 `TERMSRV/<主机名>` 主体的 keytab：

```sh
sudo grdctl --system rdp set-kerberos-keytab /path/to/krb5.keytab
sudo grdctl --system rdp set-auth-methods credentials,kerberos
```

想始终显示登录界面，在 `[session]` 里设置 `preauthenticated-pam-service = ""`。

## 配置

所有选项和默认值见 [`data/config.toml`](data/config.toml)，安装后在 `/etc/remote-dm/config.toml`，全部可选。

登录界面会话不是用你的账户跑的，而是用专门的系统账户（登录界面账户）。每个停在登录界面的连接要单独占一个登录界面账户（同一个账户的多个会话会共用一条会话总线，会互相干扰）。安装时默认建 4 个登录界面账户（`rdm-greeter-1` 到 `rdm-greeter-4`），所以**最多 4 个连接同时停在登录界面**，第 5 个会被拒绝。登录进自己的会话后，登录界面会话就结束了，账户空出来给下一个连接用，所以同时在线的远程用户数不受这个限制。要更多登录界面账户，在 `sysusers.d/remote-dm.conf` 里加。

## 排错

先看这几处日志：

```sh
journalctl -b -u remote-dm               # remote-dm
journalctl -b -u gnome-remote-desktop    # grd 系统服务
journalctl -b _UID=$(id -u 用户名)       # 该用户会话里的 niri、grd 等
loginctl list-sessions                   # 当前的会话，rdm-greeter-N 是登录界面会话
```

**连上马上断开，或者反复断开重连**：多半是客户端不跟随服务器重定向。mstsc 要在 `.rdp` 文件里加 `use redirection server name:i:1`。

**连不上**：
- 检查端口：`ss -ltnp | grep 3389` 应该能看到 gnome-remote-desktop 在监听。
- 有防火墙的话，要放行 TCP 3389。
- `systemctl status remote-dm gnome-remote-desktop`，两个服务都要在运行。

<a id="证书提示"></a>**证书提示**：证书是自签名的，第一次连接时客户端会提示不受信任。用 `sudo grdctl --system status` 看 "TLS fingerprint"（用 Nyxuri 的话，Noctalia 的"远程登录"面板里也能看到），和客户端显示的指纹一致就可以信任。

**登录后灰屏或黑屏**：
- 这个账户是不是正在本机登录？见[账户已经在本机登录时](#账户已经在本机登录时)。
- 在该用户的日志里找 `gnome-remote-desktop-daemon` 的报错，以及 niri 有没有启动。

**登录界面一直留着**：客户端收到重定向后没有连回来时，30 秒后 grd 会放弃连接，remote-dm 随即结束这个登录界面会话。

**remote-dm 起不来，日志里有 "Two services allocated for org.gnome.DisplayManager"**：GDM 还在运行。停用 GDM 后重启电脑。

**重连后看不到新装的字体、程序之类**：重连回到的是原来的会话，里面的程序不会重新加载。要一个全新的会话，先执行 `loginctl terminate-user 用户名` 结束旧会话。

## 安装的文件

| 文件 | 说明 |
|---|---|
| `/usr/bin/remote-dm` | 主程序，也包括 `remote-dm session-manager` 和 `remote-dm session-worker` |
| `/usr/lib/systemd/system/remote-dm.service` | systemd 服务 |
| `/usr/lib/sysusers.d/remote-dm.conf` | 登录界面账户 `rdm-greeter-1` … `-4` |
| `/usr/lib/tmpfiles.d/remote-dm.conf` | 这些账户的主目录 |
| `/usr/share/dbus-1/system.d/org.gnome.DisplayManager.conf` | 只有 root 能占用这个名字，只有 grd 的系统账户能调用它 |
| `/etc/pam.d/remote-dm`、`/etc/pam.d/remote-dm-greeter` | 用户会话和登录界面会话的 PAM 服务 |
| `/etc/pam.d/remote-dm-autologin` | grd 已经验证过的用户（单点登录）用的 PAM 服务 |
| `/etc/remote-dm/config.toml` | 配置文件（全部可选） |
| `/etc/remote-dm/greeter.kdl` | 登录界面会话的 niri 配置（regreet） |
| `/etc/remote-dm/greeter-noctalia.kdl` | 同上，用 Noctalia Greeter |

## 许可证

GPL-3.0-only：`src/pam/` 和 `src/scrambler.rs` 拷贝自 [greetd](https://git.sr.ht/~kennylevinsen/greetd)。
