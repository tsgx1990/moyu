# moyu 桌面版 —— 安装与未签名说明

moyu 桌面版(`apps/desktop`,Tauri 2 GUI 壳,内嵌 `moyu-cli` 作为 sidecar)通过
`.github/workflows/release-desktop.yml` 在打 `app-v*` tag 时构建,产物发布到公有仓
[`tsgx1990/homebrew-moyu`](https://github.com/tsgx1990/homebrew-moyu/releases)
的 Releases 页面(与 CLI 共用同一个公有分发仓库,tag 前缀不同:CLI 是
`cli/v*`,桌面版是 `app-v*`)。

## 下载

去 [Releases](https://github.com/tsgx1990/homebrew-moyu/releases) 页面找最新的
`app-v*` tag,按平台下载:

| 平台 | 产物 |
|---|---|
| macOS(Apple Silicon / Intel) | `.dmg` |
| Linux(x86_64) | `.deb` 或 `.AppImage` |
| Windows(x64) | NSIS 安装包 `*-setup.exe` |

**v1 不做任何代码签名**(见下方"签名推迟计划"),所以三个平台的系统防护机制都会
拦一下——这是预期行为,不代表安装包有问题。只从上面这个官方 Releases 页面下载;
未签名意味着系统没有办法替你验证来源,来路不明的构建产物不要装。

## macOS:Gatekeeper 拦截

macOS 会把从浏览器下载的未签名 / 未公证 App 标记为"来自身份不明的开发者",双击
直接打开会被 Gatekeeper 拒绝。按系统版本选绕行方式:

**方式一:系统设置放行(推荐,macOS 15 Sequoia 及以后唯一的图形界面方式)**

1. 把 `.dmg` 里的 `moyu.app` 拖到「应用程序」,双击打开一次(会被拒绝,点「完成」)
2. 打开「系统设置 → 隐私与安全性」,滚动到「安全性」区域,找到
   "已阻止 moyu.app…"的提示 → 点「仍要打开」(Open Anyway)
3. 弹窗里确认——此后正常双击即可启动,这一步只需做一次

> 注意:老教程里的「按住 Control 点击 → 打开」绕行方式,macOS 15(Sequoia)起
> 已对未签名 App **失效**(苹果移除了这条路径);macOS 14 及更早版本仍可用。

**方式二:终端清除隔离标记(所有版本可用)**

```sh
xattr -dr com.apple.quarantine /Applications/moyu.app
```

> 诚实声明:目前实测覆盖的是「本机构建的 .app 可运行」;**经浏览器下载(带
> quarantine 标记)的副本**在部分 macOS 版本上对"仅 ad-hoc 签名"的应用弹的
> 不是"身份不明的开发者"而是 **"moyu.app 已损坏,无法打开"** ——遇到这个
> 对话框时方式一没有入口,直接用方式二(始终有效)。该路径会在首个
> `app-v*` 试发布时用真实下载实测,并据结果更新本节。

## Windows:SmartScreen 警告

安装包是未签名的(没有 Authenticode 证书),双击 `*-setup.exe` 会先弹出
「Windows 已保护你的电脑」(SmartScreen)。绕行:

1. 弹窗里点「更多信息」(More info)
2. 再点「仍要运行」(Run anyway)

安装完成后应用本身正常启动,不会重复弹这个警告。

## Linux

- **`.deb`**:`sudo dpkg -i moyu_*.deb`(或 `sudo apt install ./moyu_*.deb` 让
  apt 顺带装依赖)
- **`.AppImage`**:`chmod +x moyu_*.AppImage` 后直接运行;部分发行版需要先装
  `libfuse2`(AppImage 运行时依赖)

Linux 没有 macOS/Windows 那种系统级签名门禁,这两种格式都能直接跑,不需要额外
绕行步骤。

## 本地一键打包(免签名)

不经 CI、在本机为**当前平台**打一个可安装包(macOS `.dmg` / Linux `.deb`+`.AppImage` /
Windows NSIS `.exe`):

```sh
node scripts/package-desktop.mjs          # release 包
node scripts/package-desktop.mjs --debug  # 调试包(编译快,产物大)
# 或在 apps/desktop 下:pnpm package
```

脚本做的事:装前端依赖 → release 构建 sidecar(`build-sidecar.mjs`,与 GUI 同
commit 版本锁定)→ vite 生产构建 → `tauri build` 出包,结束后列出产物路径
(`apps/desktop/src-tauri/target/release/bundle/`,设了 `CARGO_TARGET_DIR` 则在
对应目录)。

**免签名是硬约束,不是默认值**:脚本会把环境里所有签名凭据变量
(`APPLE_*`/`TAURI_SIGNING_*`/`WINDOWS_CERTIFICATE*`)剥掉再构建,有签名账号的
机器也不会误签。macOS 产物带的是 **ad-hoc 签名**(`signingIdentity: "-"`,免费、
无需任何账号)——这是 Apple Silicon 上二进制能启动的最低要求,不是分发签名;
分发出去的副本仍会遇到上文的 Gatekeeper 提示。另:脚本会拒绝环境里的全局
`RUSTFLAGS`(会静默覆盖 `.cargo/config.toml` 的 Windows /STACK 修复,见该文件注释)。

## 签名推迟计划

v1 未签名是有意的取舍(优先把功能做出来),但已经在计划表里,后续会补:

- **macOS**:申请 Apple Developer ID Application 证书($99/年),构建时
  `codesign` + `notarytool` 提交苹果公证并 staple 到 `.dmg`。完成后上面的
  Gatekeeper 绕行步骤就不再需要。
- **Windows**:接入 Azure Trusted Signing(约 $10/月)做 Authenticode 签名。
  完成后 SmartScreen 的警告会消失(或至少显示可信的发布者名称)。

这两项都需要额外的账号/证书成本,暂未启动,时间线视用户规模和预算决定。本文档
会在签名上线后同步更新、去掉过时的绕行说明。

## 版本一致性

`release-desktop.yml` 在构建矩阵跑之前会先校验:tag 的版本号(去掉 `app-v`
前缀)必须等于 `apps/desktop/src-tauri/tauri.conf.json` 里的 `version` 字段,
不一致直接失败退出——避免出现"tag 写的版本号和实际打包出来的应用版本号对不上"
这种事故(CLI 侧 `cli/v*` tag 也吃过这个教训,见 `dist-workspace.toml` 的注释)。
