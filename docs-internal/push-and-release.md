# 同步上游、推送 fork、发布 Release

<!-- 本文件只存在于 personal fork 的 local 分支，不属于上游 wezterm -->

## 分支模型

```
upstream/main ──merge --ff-only──▶ main（纯镜像，永不提交自己的改动）
                                     │
                                     └──rebase──▶ local（功能提交，日常构建）
```

- `main` 只能 `git merge --ff-only upstream/main`。一旦在上面提交过东西，
  ff-only 就会失败，镜像资格就没了。
- `local` 承载全部功能，每次同步后 `git rebase main`。

### rebase 还是 merge？

两者都能把上游带进来，区别只在历史形状：

- **rebase**：把 `local` 的 24 个提交「搬」到新 main 之上，历史是一条直线，
  `git log` 读起来像从头就长在新代码上。代价是每个提交的哈希都变，
  force-push 不可避免。
- **merge**：生成一个合并提交，历史分叉再汇合，哈希不变。代价是每同步一次
  就多一个合并节点，`git log --graph` 会越来越乱。

这个仓库选 rebase，理由和绝大多数「长期维护的 fork」一致：

1. 没人从 `local` 拉分支协作，改写历史零风险（它不跟踪任何远端）。
2. 冲突按**每个提交**逐个解决，而不是一次性解决一大坨；哪个功能和新上游
   打架一眼就能看到，必要时可以临时 `git rebase --skip` 跳过某个提交。
3. 将来若想把某个功能提给上游，一条干净的线性历史可以直接挑（`cherry-pick`）。

**行规**：这是「长命 fork + 主线镜像」的标准做法，Linux 发行版维护、各公司的
vendor fork、Chromium 的下游分支都是 rebase（或 rebase-like 的 patch stack）。
反过来，多人共享的开发分支**绝不能** rebase —— 那会让别人的本地历史和远端
对不上。判断标准只有一条：**这个分支除了我还有别人在用吗？**

`main` 不需要改写历史（它只做 fast-forward），所以它天然可以用 merge 语义推。

## 推送

`main` 必须 fast-forward，直接推：

```sh
git push origin main
```

`local` 是第一次推，用 `-u` 建立跟踪：

```sh
git push -u origin local
```

之后再同步上游、rebase 完，必须带 `--force-with-lease`（比 `--force` 安全：
远端有人改过就拒绝，不会静默覆盖）：

```sh
git push --force-with-lease origin local
```

## 本地构建与安装

```sh
cd ~/github/wezterm
. scripts/dev-env.sh          # 这台机器缺 -dev 包，用它指向 ~/.local/wezterm-sysroot
cargo build --release
```

装到 PATH（`~/.local/bin` 已在前面的 `dev-env.sh` 里）：

```sh
V=~/.local/opt/wezterm-fork-$(date +%Y%m%d)
mkdir -p $V
cp target/release/{wezterm,wezterm-gui,wezterm-mux-server,strip-ansi-escapes} $V/
ln -sfn $V/wezterm ~/.local/bin/wezterm
ln -sfn $V/wezterm-gui ~/.local/bin/wezterm-gui
ln -sfn $V/wezterm-mux-server ~/.local/bin/wezterm-mux-server
ln -sfn $V/strip-ansi-escapes ~/.local/bin/strip-ansi-escapes
```

`wezterm` 会到自己所在的**同一目录**找 `wezterm-gui`，所以四件套必须放在一起，
不能只软链 `wezterm` 一个。

## Windows 构建与 Release

本地不交叉编译（`x86_64-pc-windows-gnu` 需要 mingw 工具链，且 `openssl-sys`
会现场编译 C 代码，缺工具链直接失败）。交给 GitHub Actions：

```sh
git tag fork-v20261125
git push origin fork-v20261125
```

`.github/workflows/fork_windows.yml` 被 tag 触发，产出
`WezTerm-windows-fork-v20261125.zip`（四个 exe + conpty/angle/mesa 的 DLL）
和对应的 `.sha256`，挂到该 tag 的 Release 上。

为什么不直接用上游的 `gen_windows_tag.yml`：它的 upload 作业有
`if: github.repository == 'wezterm/wezterm'`，在 fork 上跑完 build 就静默结束，
Release 页面什么都不会出现。新文件名不是 `gen_*.yml`，所以
`ci/generate-workflows.py` 重跑时不会把它删掉。

不做安装包：Inno Setup 的 `iscc.exe` 不保证在 runner 镜像里，zip 解压即用即可。
