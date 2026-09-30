# 发版手册

两条流水线,全部跑在 GitHub 托管的 runner 上。本文是打 tag 前后要做的事。

## 一次发版长什么样

| 产物 | 触发 | 工作流 | 落点 |
|---|---|---|---|
| CLI(5 个目标 + shell/powershell 安装脚本) | 在 `main` 上打 tag **`cli/vX.Y.Z`** | `.github/workflows/cli-release.yml`(cargo-dist 生成) | 本仓库的 Release(直接发布) |
| 桌面(macOS `.dmg` ×2 / Linux `.deb`+`.AppImage` / Windows NSIS) | 打 tag **`app-vX.Y.Z`** | `.github/workflows/release-desktop.yml` | 本仓库的 **draft** Release,人工检查后 publish |

两条硬约束:tag 里的版本号必须分别等于 workspace `Cargo.toml` 的 `version`
和 `apps/desktop/src-tauri/tauri.conf.json` 的 `version`,不等就在构建矩阵
之前失败退出。README 的安装命令走 `releases/latest/download/moyu-cli-installer.sh`,
所以 "Latest" 必须一直是 CLI 的 Release。GitHub 默认会把刚 publish 的 Release
标成 Latest,**publish 桌面 draft 时一定带 `--latest=false`**:

```sh
gh release edit app-vX.Y.Z --draft=false --latest=false
```

要是已经被桌面版抢走了,用 `gh release edit cli/vX.Y.Z --latest` 钉回来。

源码和发行物都在本仓库,创建 Release 只用工作流自带的 `GITHUB_TOKEN`,**不需要
配置任何仓库 secret**。不提供 Homebrew:tap 必须是一个单独的 `homebrew-` 前缀
仓库,还得有一个能往里推送的长期令牌,而本项目只保留一个仓库、不持有长期发布令牌。
0.2.0 及更早的发行物发在 `tsgx1990/homebrew-moyu`,0.3.0 发出、文档改好之后
删除该仓库。

## 哪个 job 跑在哪台机器上

| 构建腿 | runner |
|---|---|
| CLI `aarch64-apple-darwin` / 桌面 `macos-aarch64` | `macos-14` |
| CLI `x86_64-apple-darwin` / 桌面 `macos-x64` | `macos-15-intel` |
| CLI `x86_64-unknown-linux-gnu` / 桌面 `linux-x64` | `ubuntu-22.04` |
| CLI `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` |
| CLI `x86_64-pc-windows-msvc` / 桌面 `windows-x64` | `windows-2022` |

GitHub 已预告:托管的 Intel macOS 镜像会随 macOS 15 镜像一起退役(预计 2027
年秋)。届时 x86_64 的 macOS 产物要改成在 arm64 runner 上交叉构建
(`scripts/build-sidecar.mjs` 已支持 `MOYU_SIDECAR_TARGET`),或者停止提供。

## 本仓库不设自托管 runner

公开仓库的工作流可以被任何人的 pull request 触发。自托管 runner 以机器主人的
身份执行工作流里的代码,挂在公开仓库上就等于允许陌生人在那台机器上跑代码。
所以**不要给本仓库注册自托管 runner**,发版的每一条腿都用托管 runner。

发版工作流的其它加固:

- 所有 action 都钉到 commit SHA(`cli-release.yml` 的 SHA 写在
  `dist-workspace.toml` 的 `[dist.github-action-commits]` 里,改完要重跑
  `dist generate`)。
- 发版构建不恢复构建缓存,每次从锁定的依赖冷编译。
- PR 只跑 `plan`(`pr-run-mode = "plan"`),碰不到任何持有发布令牌的 job;来自
  fork 的 PR 本来也拿不到 secret。
- 桌面发版分两段:构建 job 只有只读令牌(它要跑 `pnpm install` 和所有 crate 的
  build 脚本),安装包以 artifact 形式交给单独的 `release` job;后者持有
  `contents: write`,不运行任何项目代码,四条腿都成功才一次性建 draft。
- 尚未做到的:cargo-dist 生成的 CLI 构建 job 在环境变量里带着能写本仓库的
  `GH_TOKEN`,同时编译所有依赖的 build 脚本。cargo-dist 没有给内置 job 配权限的
  选项,要收紧只能手改生成文件并放弃 `dist generate --check`。

## 什么时候打包

打包只在推 tag 时自动发生(`cli/vX.Y.Z` 打 CLI,`app-vX.Y.Z` 打桌面)。PR 和推
main 只跑 CI(检查、测试、e2e)和 CLI 的 `plan`,不打任何安装包。桌面全平台打包
要四十分钟左右(macOS Intel 那条腿最慢),所以平时不跑。

改了打包相关的东西(`release-desktop.yml`、tauri 配置、sidecar 脚本),又想在打
tag 之前确认,由维护者决定手动触发一次(可选的冒烟):

```sh
gh workflow run release-desktop.yml --ref main -f legs=macos   # 或 legs=all
gh run list --workflow=release-desktop.yml --limit 1
gh run view <run-id> --json conclusion,jobs   # 结论只信这个,别接 tail
```

`workflow_dispatch` 只构建、不上传任何东西。不做冒烟的代价是打包问题要到打 tag
后才暴露:桌面四条腿没全成功就不会建 draft,修好后删掉 tag 重打,或者直接发下一
个补丁版本。cargo-dist 侧没有等价入口,本机跑
`dist build --artifacts=local --target <triple>` 与 runner 上会做的事一致。

## 步骤清单

1. 版本号四处一致:workspace `Cargo.toml`、`apps/desktop/package.json`、
   `apps/desktop/src-tauri/Cargo.toml`、`tauri.conf.json`;`CHANGELOG.md` 有该
   版本一节;四道门(fmt / build / test / clippy)与本地 e2e 绿。
2. 可选:手动冒烟(上一节),由维护者决定。
3. CLI:`git tag cli/vX.Y.Z && git push origin cli/vX.Y.Z`,
   `gh run list --workflow=cli-release.yml` 盯到 `host` 与 `announce` 绿。
4. 桌面:`git tag app-vX.Y.Z && git push origin app-vX.Y.Z`;draft Release 出来
   后核对资产(2 个 `.dmg`、`.deb`、`.AppImage`、`-setup.exe`),写安装说明(免签名,
   见 `docs/desktop-install.md`),用 `gh release edit app-vX.Y.Z --draft=false
   --latest=false` publish。
5. 确认 Latest 仍是 CLI:`gh release view --json tagName` 应为 `cli/vX.Y.Z`。
6. 验证安装:`curl | sh` 安装脚本、PowerShell 安装脚本、下载的 tarball 校验和。
7. 发版附带的 `source.tar.gz` 是本仓库在该 tag 的文件树(`git archive`)。发版前
   确认没有把不该公开的文件提交进来(`scripts/privacy-check.sh`)。
