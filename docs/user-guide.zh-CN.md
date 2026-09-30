# moyu 使用指南

面向已经装好 `moyu` 的用户。安装步骤见 [`README.zh-CN.md`](../README.zh-CN.md)；本文只讲怎么用。

术语先说清楚：**MLS**(RFC 9420,群组端到端加密协议)负责加密;**Nostr** 是承载协议,消息经由若干 **relay**(中继服务器)转发,relay 只转发密文和公开的路由信息,看不到明文。**KeyPackage** 是你账户对外发布的"可被邀请入群"的一次性公开凭证,过期或用掉一次就需要补发。下文命令都以 `moyu` 开头,全局参数(`--data-dir`、`--relay`、`--socks5` 等)可以放在子命令前也可以放在后面。

## 安装与首次运行

```sh
moyu init                       # 交互式:选 relay、设口令
moyu init --import-nsec         # 导入已有 Nostr 身份(见下)
```

`init` 会生成(或导入)你的 Nostr 密钥,用你选的口令加密后存到本地(Argon2id + XChaCha20-Poly1305),发布你的 relay 列表和第一个 KeyPackage,并把 relay 集合持久化到 `<data-dir>/config.json`——此后大多数命令不用再重复传 `--relay`。

非交互场景(没有 TTY,或带了任意 relay 参数)`init` 不会弹交互提示;脚本/CI 用环境变量 `MOYU_PASSPHRASE` 传口令,而不是走隐藏输入提示。

### 选择 relay

- 默认公共 relay(去中心化,damus / nos.lol / primal),不用任何参数即用得上,也可以显式 `moyu init --relay-preset public` 跳过交互提示。
- 自定义 / 自建 relay:`moyu init --relay wss://relay.example.com`。自建教程见[网络与代理](#网络与代理)一节。
- 已有身份想补充一个 relay:`moyu relay add wss://relay.example.com`;不想再用某个 relay:`moyu relay forget wss://relay.example.com`;查当前持久化的集合:`moyu relay list`。
- 邀请码自带发出方的 relay 集合,`moyu join <code>` 会自动把它并入你的集合,所以受邀双方通常不用手动对齐 relay。

### `--import-nsec` 的三种用法

`moyu init --import-nsec ...` 导入一个已有的 Nostr 私钥(`nsec1…` 或 64 位十六进制),而不是新生成一个:

1. `moyu init --import-nsec`(不带值)——不回显地提示你输入私钥,推荐用法。
2. 用管道从 stdin 喂一行:`echo "$NSEC" | moyu init --import-nsec`。
3. `moyu init --import-nsec nsec1…`(内联值)——私钥会经过 shell 历史记录和进程列表,**不推荐**,仅为已经把私钥存在变量里的脚本保留。

## 网络与代理

- `--socks5 IP:PORT`(全局参数,必须是 IP 字面量,不能是域名)让 **relay 连接、NIP-05 查询、附件传输** 全部走这个 SOCKS5 代理,主机名交给代理端解析(不会有本地 DNS 泄漏)。例:`--socks5 127.0.0.1:1080` 指向本机的 Tor / `ssh -D` 等 SOCKS5 端口。不传就是直连;moyu 不会自动读取 `HTTP_PROXY`/`ALL_PROXY` 环境变量,也不内置 Tor。
- `--dev-allow-loopback`(全局参数)是拨号到 `127.0.0.0/8`/`::1`/`localhost` 这类回环地址 relay 的前提条件——MDK 默认拒绝(防 SSRF)。发行版里也保留这个参数,因为通过 `ssh -L` 隧道连自建 relay 本质上就是回环地址。除了本地测试和这种隧道场景,不要对回环地址开这个开关。
- 自建 relay(自己的 VPS 上跑 strfry + moyu 的 Marmot-only 写策略插件)完整步骤见 [`docs/self-host-relay.md`](self-host-relay.md);一句话版:装好后 `moyu init --relay wss://relay.example.com`,之后发的邀请码会自动带上这个 relay。

## 日常聊天

```sh
moyu add alice@nostr.example --label alice   # 也可以是 npub1… 或十六进制公钥
moyu chat alice                              # 进入 REPL;或用 `moyu tui` 全屏界面
moyu send alice "review my PR?"              # 一次性发送,脚本友好
moyu recv --follow                           # 持续轮询,收到就打印
```

- `moyu send <peer> [message]`:消息省略(或传 `-`)时从 stdin 整体读入。
- `moyu history <group> [--limit N] [--before CURSOR]`:离线读本地已解密的历史,最新的一页在前;翻页用上一页返回的 `next_cursor`。
- `moyu search "deploy" [--group G] [--limit N]`:离线子串搜索所有已解密历史,不区分大小写。
- `moyu reply <group> <message_id> [text]`:带引用的线程回复(kind-9,携带父消息的 `e`/`q` 标签)。
- `moyu react <group> <message_id> [emoji] [--remove]`:kind-7 表情回应,`--remove` 撤回自己发的那个。
- `<group>` 参数(`history`/`search --group`/`react`/`reply`/`download` 都要用到)是工作区名/前缀,或者群组 id 的十六进制前缀——`moyu conversations`、`moyu workspace list`、`moyu recv --json` 都能拿到这个值。
- `moyu conversations`:一条命令列出所有会话(DM + 工作区/频道树 + 还没聊过的联系人),GUI/机器人拿它来初始化侧边栏。

## 工作区与频道

工作区(workspace)是持久化的多人群组,里面可以再开频道(channel)。

```sh
moyu workspace new eng                       # 新建工作区,起初只有你自己
moyu channel new eng backend                 # 公开频道
moyu channel new-private eng sec alice bob   # 私有频道:嵌套的独立 MLS 群组,
                                              #   不邀请就看不到
moyu post eng backend "deploy at 3?"         # 或: ci-log | moyu post eng backend -
moyu workspace members eng                   # 查看成员,管理员会被标出来
moyu workspace admin add eng alice           # 治理操作要求管理员权限
moyu workspace rename eng engineering        # 改名
moyu workspace kick eng bob                  # 踢人(仅管理员)
moyu workspace leave eng --transfer-to alice # 唯一管理员离开前必须先移交
```

频道管理:`moyu channel invite <ws> <slug> <member>`(邀请进私有频道,对方必须已经是工作区成员)、`moyu channel list <ws>`、`moyu channel rename <ws> <slug> <name>`、`moyu channel archive <ws> <slug>`。踢人和管理员变更是 MLS 群状态,非管理员发起的变更会被其他成员的客户端拒绝;改名和归档是群内消息,所有成员的客户端会收敛到同一份视图并显示一条治理提示——命令行只允许管理员发,但这是客户端一侧的规则,不是密码学保证(详见[威胁模型](threat-model.md))。

### 邀请与加入

```sh
moyu invite eng                  # 打印工作区邀请码(要求你是 eng 的管理员)
moyu join "moyuinv1qqs…"         # 用邀请码加入;没有身份会先 init 一个
moyu requests                    # 列出待处理的加入请求
moyu approve all                 # 或 `moyu approve <npub>` 只批一个
moyu deny <npub>                 # 本地忽略这条请求,不发送任何事件
```

`moyu invite`(不带工作区名)打印的是 1:1 联系人邀请码,不需要管理员权限。`moyu requests` 给每条请求打上徽章:✓ **trusted**(对方出示了你确实发过的邀请密钥)或 ⚠ **uncredentialed**(未持有你发出的密钥,需要你自行判断是否批准)。

`moyu invite eng --auto-approve` 让加入请求自动通过,跳过手动 `approve` 这一步——但仍要求你本人是该工作区的管理员,非管理员执行 `--auto-approve` 会被直接拒绝(不是静默失效)。

工作区邀请码是"持有即可用"的凭证,请把它当口令对待:

- **7 天后失效。** 以请求到达你这台设备的时间为准;过期的码不再显示 ✓ trusted,也不会自动通过,但请求仍会列在 `moyu requests` 里,你可以手动 `approve`。
- **可以作废。** `moyu invite eng --revoke` 让你为 `eng` 发出的所有邀请码立刻失效(已经加入的人不受影响)。码泄露了就用它。
- **被踢的人不会被码放回来。** 被 `workspace kick` 移除的成员即使还留着有效的 `--auto-approve` 码,也不会再被自动加入;他可以重新申请,但必须由管理员手动 `approve`。
- **一条加入请求只处理一次。** 请求被批准后就结束了,之后这个人被踢或自己退出,旧请求不会把他重新拉回来。
- `moyu join <code>` 会把码里带的 relay 加进你的 relay 集合并告诉你加了哪些;不想保留就 `moyu relay forget <url>`。

## 附件

```sh
moyu send alice "the diff" --file ./pr.patch     # 文字变成说明/caption,文件端到端加密后上传
moyu post eng backend "build artifact" --file ./out.tar.gz
moyu download eng 4b37…sha256… --out ./           # 按 ciphertext_sha256 下载并解密
```

附件在客户端加密后上传到 Blossom blob 服务器(默认 `https://blossom.primal.net`,`--blossom <URL>` 可覆盖),服务器只看得到密文。`moyu download <group> <hash>` 里的 `<hash>` 是 `recv --json` 的 `attachments[]` 中给出的 `ciphertext_sha256`(也接受 `plaintext_sha256`)。

## TUI 用法与按键

```sh
moyu tui
```

`moyu tui` 是全屏聊天界面(ratatui),**所有会话(含工作区频道)同时在一屏里实时显示**,打开时自动回填历史。按键:

- `↑` / `↓`:切换选中的会话
- 直接输入文字,`Enter` 发送到当前选中的会话
- `Backspace` 删字符,`Esc` 清空输入框(打开花名册悬浮层时 `Esc`/`Enter` 用于关闭它)
- `Ctrl-R`:打开当前会话的成员花名册悬浮层
- `Ctrl-C`:退出

TUI 里能看到机器人(🤖)消息和加入请求提示行,但**没有**群组管理、搜索、设置这些功能——要管理工作区/频道、搜索历史,用 CLI 子命令(见上文)或桌面应用。

## 脚本与机器人

每个一次性命令都支持全局 `--json`,输出恰好一个机器可读对象(或 `recv` 的逐行 JSONL 流),每个对象带 schema 版本号 `"v"`;失败时输出 `{"ok":false,"error":…}` 并以非零状态码退出——脚本判断成功与否看退出码,不用解析文本。

```sh
ci-summary | moyu op eng ci --type ci --status failed --name build --fail "3 tests failed"
# 队友的 `moyu recv --follow`(本机解密后):
# [a1b2c3d4] 🤖 ci-bot #ci [ci·failed] build: 3 tests failed ✗ (48.2s)
```

- `moyu op <ws> <channel> [text] --type T --status S [--name][--run-id][--ok|--fail][--duration-ms][--preview][--details JSON]`:结构化的 bot **OPERATION** 事件(kind-1202),给 CI/部署/git/监控这类场景用,协议层面就标记为机器人事件而不是人类消息。
- `moyu activity <ws> <channel> [text] [--status S] [--extra JSON]`:轻量的 bot **ACTIVITY** 一行播报(kind-1201),渲染时带 🤖 标记。
- `moyu send`/`moyu post` 的消息参数省略或传 `-` 时从 stdin 整段读入,方便接管道:`ci-log | moyu post eng backend -`。
- `moyu session`:长驻的 stdio JSON 命令/事件循环(框架化 JSONL),桌面应用就是走这个协议驱动 CLI 的;启动即锁定,驱动进程要发 `unlock` 命令,不会走终端口令提示。

更完整的 GitHub Actions / git `post-receive` hook / 通用 webhook 桥接示例见仓库的 [`examples/integrations/`](../examples/integrations/)(英文,命令本身语言无关)。

## 桌面应用

`apps/desktop` 是 Tauri 2 壳(React + TypeScript),内部把打包的 `moyu` 二进制当 `moyu session` sidecar 启动,通过框架化 stdio JSONL 驱动——GUI 本身不含任何加密代码,webview 没有 shell 权限,文件路径和口令不经过 webview。它和单独安装的 CLI 共享同一个操作系统数据目录,所以两边看到的身份和历史是同一份(用同一个口令解锁)。

安装包目前**未签名**,macOS/Windows 首次打开会被系统拦一下,绕行步骤见 [`docs/desktop-install.md`](desktop-install.md)。

应用内有一个「网络 / 代理设置」面板:一个 SOCKS5 代理输入框(格式 `IP:PORT`,例如 `127.0.0.1:1080`,留空即直连)和一个「允许 loopback relay(仅本地测试)」开关(勾选后才能在新建身份时填 `ws://127.0.0.1:PORT` 这类地址)。保存会重启后台会话以让新参数生效,口令不会被缓存。

功能上桌面版与 CLI 基本对等:引导(新建/导入/邀请码加入)、DM + 工作区/频道聊天(带 Markdown 渲染)、附件收发、加入请求审批、工作区/频道治理、KeyPackage 轮换、客户端搜索、未读提醒和系统通知。

## 密钥与安全

- **KeyPackage 轮换**:日常命令运行时会在约 60 天(MLS 规范上限 84 天+1 小时前)自动补发;`moyu keypackage rotate` 强制立即轮换(比如从备份恢复身份之后),`moyu keypackage publish` 只是(重新)发布当前的。一个长时间运行的 `tui`/`chat` 会话每 6 小时重新检查一次 KeyPackage 是否需要刷新。
- **数据目录与备份**:身份、加密的 MLS 状态、本地消息库都在一个按操作系统区分的目录里,**备份它就是备份你的身份,绝不要放进代码仓库**:

  | 系统 | 默认 `--data-dir` |
  |---|---|
  | macOS | `~/Library/Application Support/chat.moyu.moyu` |
  | Linux | `~/.local/share/moyu` |
  | Windows | `%APPDATA%\moyu\moyu\data` |

  想跑多个独立身份,给每个用不同的 `--data-dir`(`scripts/e2e-local.sh` 就是这么做多账号联调的)。
- **升级须知**:MDK 的本地数据库迁移只进不退——升级前先备份数据目录,以防要回滚版本。moyu 对 MDK 的依赖锁在一个具体的 git rev 上,升级是刻意为之的动作而不是自动跟随。
- **口令忘了不可恢复**:私钥用 Argon2id 派生的密钥加密存放,口令是唯一的解密入口,没有后门或找回流程——弄丢口令等于弄丢这份身份和历史。
- moyu 本身没有做过第三方安全审计;想了解具体保护了什么、没保护什么,看 [`docs/threat-model.md`](threat-model.md)。

## 故障排查

- **连不上 relay / 消息发不出去**:先 `moyu relay list` 确认持久化的 relay 集合是你以为的那些;公共 relay 有时会限流或丢弃 Marmot 用到的事件类型,自建 relay(见上文)通常能解决。
- **所在网络无法直连 relay**:加全局 `--socks5 IP:PORT` 指向本机的 SOCKS5 代理(Tor/`ssh -D` 等),它同时覆盖 relay 连接、NIP-05 查询和附件传输;记得写 IP 不是域名。
- **口令忘了**:见上文"密钥与安全"——没有找回手段。
- **报错说 loopback relay 被拒绝**:给 `--relay` 传的是 `127.0.0.1`/`::1`/`localhost` 这类地址,而没有加 `--dev-allow-loopback`。这个校验是为了防止生产环境误连回环地址,不是 bug;仅本地测试/`ssh -L` 隧道场景才需要这个参数。
- **Windows**:预编译二进制和安装包自 `cli/v0.2.0`/`app-v0.2.0` 起提供;桌面安装包未签名,双击 `*-setup.exe` 会先弹 SmartScreen,点「更多信息」→「仍要运行」即可,详见 [`docs/desktop-install.md`](desktop-install.md)。

## 附录:命令参考

以下摘要严格取自 `moyu <命令> --help`;完整参数说明以命令行自身的 `--help` 为准。全局参数(任何子命令都能加):`--data-dir <path>`、`--relay <url>`(可重复)、`--dev-allow-loopback`、`--socks5 <ip:port>`、`--json`、`--blossom <url>`;`MOYU_PASSPHRASE` 环境变量非交互地提供口令。

| 命令 | 摘要 |
|---|---|
| `init [--import-nsec [nsec]] [--relay-preset public]` | 创建(或用 `--import-nsec` 导入)身份并发布首个 KeyPackage |
| `whoami` | 打印当前账户的 label 与 npub |
| `keypackage publish` | (重新)发布当前 KeyPackage |
| `keypackage rotate` | 强制轮换 KeyPackage |
| `add <npub\|hex\|name@domain> [--label L]` | 添加联系人 |
| `chat <peer>` | 打开(必要时创建)1:1 聊天并进入 REPL |
| `send <peer> [message] [--file PATH]` | 一次性发送(消息省略/`-` 则读 stdin;配 `--file` 时该参数变附件说明) |
| `recv [--follow]` | 接收待处理邀请、同步、打印新消息;`--follow` 持续轮询 |
| `tui` | 全屏聊天界面,`Ctrl-C` 退出 |
| `workspace new <name>` | 新建工作区(初始只有自己) |
| `workspace list` | 列出所属的所有工作区 |
| `workspace add <ws> <npub\|hex\|name@domain>` | 邀请成员入工作区 |
| `workspace members <ws>` | 列出成员(管理员被标出) |
| `workspace rename <ws> <name>` | 重命名工作区 |
| `workspace kick <ws> <member>` | 移除成员(仅管理员) |
| `workspace admin list\|add\|remove <ws> [<member>]` | 查看/晋升/降级管理员(仅管理员) |
| `workspace leave <ws> [--transfer-to member]` | 离开工作区(唯一管理员须先移交) |
| `channel new <ws> <name>` | 新建公开频道 |
| `channel new-private <ws> <name> [members...]` | 新建私有频道(独立嵌套 MLS 群组) |
| `channel invite <ws> <slug> <member>` | 邀请成员进私有频道 |
| `channel list <ws>` | 列出频道(公开在前,🔒 私有在后) |
| `channel rename <ws> <slug> <name>` | 重命名频道 |
| `channel archive <ws> <slug>` | 归档频道 |
| `post <ws> <channel> [message] [--file PATH]` | 发消息到工作区频道 |
| `download <group> <hash> [--out PATH]` | 按内容哈希下载并解密附件 |
| `react <group> <message_id> [emoji] [--remove]` | kind-7 表情回应,`--remove` 撤回 |
| `reply <group> <message_id> [message]` | 线程化回复(kind-9,带父消息引用) |
| `search <query> [--group G] [--limit N]` | 离线子串搜索本地历史(默认 50 条) |
| `history <group> [--before cursor] [--limit N]` | 离线读一个群组的本地历史,最新页在前(默认 50 条) |
| `conversations` | 一次列出所有 DM + 工作区/频道 + 未聊过的联系人 |
| `session` | 长驻 stdio JSON 命令/事件循环(GUI 驱动 CLI 用) |
| `op <ws> <channel> [text] --type T --status S [...]` | 结构化 bot OPERATION 事件(kind-1202) |
| `activity <ws> <channel> [text] [--status S] [--extra JSON]` | 轻量 bot ACTIVITY 播报(kind-1201) |
| `invite [workspace] [--auto-approve]` | 打印邀请码(不带工作区名 = 1:1 联系人邀请);工作区码 7 天后失效 |
| `invite <workspace> --revoke` | 作废为该工作区发出的所有邀请码 |
| `join <code>` | 用邀请码加入 |
| `requests` | 列出待处理的工作区加入请求(带信任徽章) |
| `approve <npub\|all> [--workspace <ws>]` | 批准一个或全部加入请求(仅管理员);同一个人同时申请了多个工作区时必须用 `--workspace` 指明 |
| `deny <npub>` | 本地忽略一条加入请求 |
| `relay list` | 列出持久化的 relay 集合 |
| `relay add <url>` | 添加一个 relay(幂等) |
| `relay forget <url>` | 按 URL 移除一个 relay |
