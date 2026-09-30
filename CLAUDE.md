# moyu — 项目工作规则

moyu = 端对端加密 CLI 聊天工具(**MLS over Nostr**,面向程序员)。本文件是给在本仓库里工作的 AI 编码助手(以及任何贡献者)看的工作纪律,优先级高于默认行为。人读的版本见 [`CONTRIBUTING.md`](CONTRIBUTING.md)。

## 技术底座(已定,勿轻易推翻)
- **加密核心**:MLS(RFC 9420),经 **MDK**(`marmot-protocol/mdk`,MIT)/ OpenMLS。提供 PFS + PCS(前向保密 + 后妥协自愈)。
- **承载**:Nostr(Marmot 风格),`rust-nostr`(nostr 0.44.x)。
- **语言** Rust,工具链 **pin 1.97.1**(对齐 MDK,见 `rust-toolchain.toml`)。
- **分层**:`moyu-core`(无头核心,封装 MDK 的 `MarmotApp`/`AppClient`)+ `moyu-cli`(前端)。UI 只驱动 core,不直接依赖 MDK crate。
- **文档**:威胁模型 `docs/threat-model.md`;MDK 调用点 `docs/mdk-api-map.md`;发版 `docs/release.md`。
- **许可证红线**:MDK / wn-tui = MIT 可依赖可抄(抄了要在 `THIRD-PARTY-NOTICES.md` 留版权声明);**whitenoise-rs = AGPLv3,只能读设计,不能抄代码/链接**。

## 这是公开仓库:什么绝不能提交
仓库对所有人可读,提交即永久公开(删不干净)。以下内容**不得出现在任何被跟踪的文件或提交信息里**:
- 密钥、令牌、口令、`nsec`;运行时数据(身份、MLS 状态、SQLCipher 数据库、`moyu-state.json`)。
- 个人信息:本机绝对路径(`/Users/<名字>/…`、`/home/<名字>/…`)、机器名、个人邮箱、真实姓名、自用的域名/IP/服务器、账单或订阅细节。示例路径用 `<data-dir>`、`~/…` 或占位用户名。
- 别人的真实标识:真实人物的 npub、邮箱、域名。测试用的公钥要现生成。
- 维护者的工作记录:进度台账、交接记录、内部计划/规格、AI 会话状态。这些只放在本地(`PROGRESS.md`、`HANDOFF.md`、`CLAUDE.local.md`、`docs/superpowers/`、`docs/design/`、`.remember/`、`.claude/` 都已在 `.gitignore`),**不要 `git add -f` 它们**。
- 代码注释写"代码为什么这样",不写内部里程碑代号、评审编号或"某天某人要求"。

机制:`scripts/privacy-check.sh`(gitleaks + 本仓库的隐私规则,见 `.gitleaks.toml`)是 pre-commit hook 和 CI 的共同入口。每个克隆启用一次:`git config core.hooksPath .githooks`。**提交前它必须是绿的;不要用 `--no-verify` 绕过。**

另外:本仓库**不设自托管 runner**(公开仓库的工作流能被任何人的 PR 触发);工作流里的 action 一律钉 commit SHA;不要把 `${{ github.* }}` 直接拼进 `run:` 脚本。

## 提交前的门禁
- 四道门:`cargo fmt --all --check`、`cargo build`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test -p moyu-core -p moyu-cli`。桌面壳另有 `pnpm typecheck` / `pnpm test` 和 `src-tauri` 的 clippy。
- 改动涉及协议行为(收发、邀请、成员变更、附件)时跑本地 E2E:`scripts/e2e-local.sh`(CI 也跑)。
- `moyu init` 会往 relay **发布 KeyPackage(对外动作)**;测试一律用**本地 relay**(`--dev-allow-loopback`),不要往公共 relay 发测试身份。
- 安全相关改动额外自查:密钥零化、错误路径、人读输出是否走 `hprintln!`/`heprintln!`(crate 级 deny 了裸 `println!`)。
- 提交信息动词开头、说清做了什么;小步提交优于大提交。

## MDK 依赖
- MDK 以 **git rev 锁定**,指向 fork `tsgx1990/mdk`(上游 tag + SOCKS5 代理补丁,见根 `Cargo.toml` 的注释块)。上游有等价功能后换回上游、撤掉 fork。
- **改 MDK 只能走 fork** 并推上去,绝不堆不推的本地改动;能在 moyu 侧解决就不改 MDK。
- **升级 MDK 的门禁**:fork 分支 rebase 到新的上游 tag,`cargo check -p marmot-app` + `cargo fmt --check` 绿 → 换根 `Cargo.toml` 的九个 rev,并对齐 `nostr` 版本与 `rust-toolchain.toml` → 四道门 → 本地 E2E。MDK 的数据库迁移只进不退,CHANGELOG 必写升级须知。
- **供应链门禁**:`deny.toml` + `.github/workflows/security.yml`(cargo-deny,主工作区 + 桌面壳两遍);新增 git 依赖必须进 `[sources].allow-git`,GPL 家族许可证命中即阻断。
