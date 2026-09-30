# moyu

面向程序员的端到端加密(E2EE)命令行聊天工具,底层是 **MLS(RFC 9420)over Nostr**——即 Marmot 协议,由 [MDK(Marmot Development Kit)](https://github.com/marmot-protocol/mdk)(MIT 协议)提供加密与传输引擎。MLS 给出前向保密(Forward Secrecy:即使当前密钥泄露,过去的消息依然读不出来)和后妥协自愈(Post-Compromise Security:成员密钥被轮换后群组会自动"愈合",恢复安全)。moyu 本身**不运营任何服务器**——消息经由去中心化的 Nostr relay(中继)转发,relay 只看得到密文和连接的元数据(谁在什么时候连了哪个 relay),看不到消息内容、群成员或频道名。

英文版:[`README.md`](README.md) · 使用指南:[`docs/user-guide.zh-CN.md`](docs/user-guide.zh-CN.md) · 威胁模型:[`docs/threat-model.md`](docs/threat-model.md) · 自建 relay:[`docs/self-host-relay.md`](docs/self-host-relay.md) · 桌面版安装:[`docs/desktop-install.md`](docs/desktop-install.md)

## 项目状态

**维护态。** moyu 只做安全修复和 MDK 升级,不再计划新功能。上游 MDK 自带的命令行和 TUI(`wn`)已经覆盖同样的聊天核心,新的协议特性也会先落在那边。moyu 在 MDK 之上多出来的部分比较窄:relay、NIP-05 查询和附件都能走 SOCKS5 代理(靠一个很小的 [MDK fork](https://github.com/tsgx1990/mdk))、工作区/频道/邀请码/审批模型、给自建 strfry relay 用的写入策略插件、桌面 GUI,以及中文文档。这是一个单人项目,没有第三方安全审计,依赖它之前请先读下面两节。

## moyu 不是什么

在决定要不要试之前,先看清楚边界:

- **没有第三方安全审计。** MDK 和 moyu 都还没做过审计,线格式(wire format)和分叉收敛(fork-convergence)逻辑仍在随版本演进,升级前请备份数据目录。
- **单设备。** 目前不支持多设备——上游 MDK 的多设备特性(MIP-06)还没落地。新设备无法读取它加入之前的历史消息,这是 MLS 前向保密机制本身带来的代价,不是实现缺陷。
- **桌面安装包未签名。** macOS 会弹 Gatekeeper、Windows 会弹 SmartScreen,需要手动放行一次,步骤见 [`docs/desktop-install.md`](docs/desktop-install.md)。
- **代理是显式的,不含内置 Tor。** 在无法直连 relay 的网络环境里,得自己传 `--socks5 IP:PORT`;moyu 不会自动读取 `HTTP_PROXY`/`ALL_PROXY` 环境变量。
- **没有官方托管的 relay。** 默认连公共 relay(damus / nos.lol / primal),或者[自建一个](docs/self-host-relay.md)。

## 30 秒快速开始

两条命令,两个人:

```sh
# A:有一个工作区 "eng",且是它的管理员
$ moyu invite eng
moyuinv1qqs…                     # 把这一串发给 B(任何渠道都行:聊天软件、邮件、甚至口述)

# B:哪怕还没有 moyu 身份也没关系
$ moyu join "moyuinv1qqs…"
# join 会用邀请码里的 relay 配置好本地设置,必要时创建身份并发布 KeyPackage,
# 然后给 A 发一条加入请求

# A:批准
$ moyu requests                  # 列出谁在等待:npub、工作区、✓ trusted / ⚠ uncredentialed
$ moyu approve all                # (或 `moyu approve <npub>` 只批一个)
```

B 下一次 `moyu recv` 就能在 `eng` 里看到自己,接着用 `moyu chat`/`moyu tui` 聊天。给 `invite eng` 加 `--auto-approve` 可以跳过手动 `approve`(A 仍必须是 `eng` 的管理员,非管理员执行会被直接拒绝)。

只是想加个 1:1 联系人,不需要工作区?去掉工作区名即可:

```sh
$ moyu invite                     # A:打印一个纯联系人邀请码
$ moyu join "<code>"              # B:加入,顺带给 A 发一条打招呼消息
$ moyu recv                       # A:看到 B 的消息,自动记为联系人
```

完整的日常用法(工作区、频道、附件、脚本集成、TUI 按键……)见[使用指南](docs/user-guide.zh-CN.md)。

## 安装

moyu **没有发布到 crates.io**。`cargo install moyu` 或 `cargo install moyu-cli` 装到的是与本项目无关的 crate(或者将来任何人抢注的同名 crate),请只用下面的渠道,或从本仓库源码构建。

预编译二进制,不需要装 Rust 工具链。支持平台:macOS(Apple Silicon + Intel)、Linux(x86_64 + arm64,glibc)、Windows(x64/MSVC)。装好后的可执行文件叫 `moyu`(包/formula 名字是 `moyu-cli`)。

**Homebrew**

```sh
brew install tsgx1990/moyu/moyu-cli
```

**Shell(curl | sh)**

```sh
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/tsgx1990/homebrew-moyu/releases/latest/download/moyu-cli-installer.sh | sh
```

**Windows(PowerShell)**

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/tsgx1990/homebrew-moyu/releases/latest/download/moyu-cli-installer.ps1 | iex"
```

**手动下载** —— 从 [Releases](https://github.com/tsgx1990/homebrew-moyu/releases) 页面下载对应平台的 `moyu-cli-<target>.tar.xz`(Windows 是 `.zip`),解压后 `chmod +x moyu`。如果是用**浏览器**在 macOS 上下载的,需要先清掉隔离标记(用 Homebrew 或 curl|sh 安装则不需要这一步):

```sh
xattr -dr com.apple.quarantine ./moyu
```

**桌面版(GUI)** —— Tauri 2 壳,内嵌上面这个 CLI 作为 sidecar,与独立安装的 CLI 共享同一身份/数据目录。安装包从 `app-v0.2.0` 起发布在同一个 [Releases](https://github.com/tsgx1990/homebrew-moyu/releases) 页(macOS `.dmg` / Linux `.deb` 和 `.AppImage` / Windows NSIS)。**目前未签名**,首次打开需要按系统提示手动放行,步骤见 [`docs/desktop-install.md`](docs/desktop-install.md)。想自己打包或跑开发模式:

```sh
node scripts/package-desktop.mjs        # 免签名打包当前平台
# 开发模式:cd apps/desktop && pnpm install && pnpm tauri dev
```

**从源码构建(贡献者)** —— 普通用户直接装预编译二进制即可。从源码构建需要 Rust 1.97 工具链,以及能拉取 MDK git 依赖的网络:

```sh
# Rust 1.97.1 通过 rust-toolchain.toml 锁定(与 MDK 对齐)
cargo build --workspace
cargo test  --workspace
```

## 常用命令一览

参数细节以 `moyu <命令> --help` 或[使用指南的命令参考](docs/user-guide.zh-CN.md#附录命令参考)为准:

```
# 身份与密钥
moyu init [--import-nsec [nsec1…]] [--relay-preset public] 创建/导入身份(--import-nsec 不带值=隐藏输入提示)
moyu whoami                                                打印当前账户的 label + npub
moyu keypackage publish | rotate                           (重新)发布 / 强制轮换 KeyPackage

# 联系人与 1:1 聊天
moyu add <npub|hex|name@domain> [--label L]   记住一个联系人
moyu chat <peer>                              打开 1:1 聊天并进入 REPL
moyu send <peer> <text> [--file PATH]         一次性发送(脚本友好);--file 附带 E2EE 加密附件
moyu recv [--follow]                          收取待处理邀请与新消息;--follow 持续轮询
moyu tui                                      全屏聊天界面(ratatui)

# 工作区与频道
moyu workspace new|list|add|members|rename|kick|admin|leave
moyu channel   new|new-private|invite|list|rename|archive
moyu post <ws> <channel> [text] [--file PATH]

# 邀请与加入
moyu invite [<workspace>] [--auto-approve | --revoke]   工作区邀请码 7 天后失效;--revoke 作废已发出的码
moyu join <code>
moyu requests | approve <npub|all> [--workspace <ws>] | deny <npub>

# 读取与互动
moyu conversations / history / search / react / reply / download

# 机器人与自动化
moyu op / moyu activity / moyu session

# relay
moyu relay list | add <url> | forget <url>
```

每个一次性命令都支持全局 `--json`(见下文"脚本与机器人")。全局参数:`--data-dir <path>`、`--relay <url>`(可重复,默认几个公共 relay)、`--dev-allow-loopback`(`--relay` 指向 `ws://127.0.0.1:…` 这类回环地址时必须显式加,见「已知局限」)、`--socks5 IP:PORT`。`MOYU_PASSPHRASE` 环境变量供脚本/CI 非交互地传口令,否则走不回显的隐藏输入提示。

## 已知局限(如实说)

- **代理是显式的,没有内置 Tor。** `--socks5 IP:PORT`(全局参数)让 relay 连接、NIP-05 查询**以及附件传输**都走这个 SOCKS5 代理(主机名由代理端解析,不会本地 DNS 泄漏)。不传就是直连。
- **回环 relay 需要显式 `--dev-allow-loopback`。** MDK 默认拒绝拨号到非公网 relay 地址(防 SSRF);这个参数在发行版里也保留,因为通过 `ssh -L` 隧道连自建 relay 本质上就是回环地址,是一个显式、全局、从不被自动推断的开关。
- **单设备。** 新设备读不到加入之前的历史——MLS 前向保密的固有代价。
- **MDK 还年轻,没有第三方审计。** wire format 和 fork-convergence 逻辑仍在变化;git rev 是刻意锁死的,升级要对着 MDK 的 CHANGELOG 走(数据库迁移只进不退,升级前先备份数据目录)。
- **`moyu chat` 一次只显示一个会话。** 1:1 REPL 只打印自己这个 DM;其它会话的消息仍会存下来,`history`/`recv`/`tui` 都能看到(`tui` 同时显示所有会话,包括工作区频道)。

## 更多文档

- 使用指南(安装、日常聊天、工作区、附件、TUI、脚本/机器人、桌面应用、故障排查):[`docs/user-guide.zh-CN.md`](docs/user-guide.zh-CN.md)
- 威胁模型(保护什么、不保护什么、对谁):[`docs/threat-model.md`](docs/threat-model.md)
- 自建 relay:[`docs/self-host-relay.md`](docs/self-host-relay.md)
- 桌面版安装与未签名说明:[`docs/desktop-install.md`](docs/desktop-install.md)
- 英文版 README(状态、目录结构、许可证细节):[`README.md`](README.md)
