# 上游差距与完善路线（Rust 版）

本文档是 [`UPSTREAM_SYNC.md`](./UPSTREAM_SYNC.md) 的补充：台账记录"已经搬了什么"，本文记录
"**还差什么、按本库的 crate 架构该怎么补**"。

- 上游仓库：`vercel-labs/just-bash`
- 上游参照点：`58d1ecf3`（`origin/main` 当前 HEAD，与台账一致，其后 0 个提交）
- 分叉点：`d64009a`（台账记录的历史共享点）
- 本库：`crates/` 下的 10 个 crate，28 804 行 Rust，零外部依赖

> 结论先行：本库在**解析器/执行器主干**上已经追上了上游的语义细节（fd 重定向、`break N`、
> `${p:-word}`、关联数组、切片、heredoc、`case`/`until`/函数/`[[ ]]`、进程替换、awk/jq/sed
> 三元组），但存在 **三类系统性缺口**：
>
> 1. **宿主进程会被恶意脚本直接杀死**（栈溢出 abort），违反"保守限额"的威胁模型承诺；
> 2. **一批高频语义静默出错**（字段分割、复合命令重定向、`read` EOF、`printf` 宽度、`tr -d`），
>    与 `THREAT_MODEL.md` 宣称的 fail-closed 原则相反；
> 3. **命令目录只有上游的 1/3**（上游 85 个内建命令，本库 51 个），且缺少把一致性验证自动化
>    的测试资产（上游有 457 条 bash 对照夹具 + 2 728 条 spec 用例，且**已在本仓库 git 历史里**）。

---

## 0. 证据与复现方式

下文所有结论都经过实机验证，证据形式如下。

```bash
# 二进制（本机 cargo target 不在仓库内）
BIN="$CARGO_TARGET_DIR/debug/just-bash-rs"     # 或 cargo run -q -p just-bash --bin just-bash-rs -- '...'
```

- 本库现状：`cargo test --workspace` 全绿（622 个 `#[test]`），`cargo clippy --workspace
  --all-targets` 零告警。
- 上游现状：`git -C ../vercel-labs/just-bash` 可读取 `58d1ecf3` 的任意文件（含
  `limits.ts`、`interpreter/`、`parser/`、`commands/`、`spec-tests/`）。

---

## 1. P0 — 宿主进程崩溃（最高优先级）

这是唯一一类**embedder 无法防御**的问题：进程 abort 后 `catch_unwind` 也救不回来。

### P0-1 递归无深度预算 → `fatal runtime error: stack overflow, aborting`

| 构造 | 本库实测 | 上游 |
| --- | --- | --- |
| 嵌套子 shell `( … )` | 600 层 OK，**800 层 abort** | `MAX_PARSER_DEPTH = 200`，抛 `ParseException` |
| 嵌套 group `{ … }` | 500 层 OK，**1 500 层 abort** | 同上（共享同一 depth guard） |
| 嵌套 `if … fi` | 600 层 OK，**1 000 层 abort** | 同上 |
| 嵌套 `while … done` | 600 层 OK，**1 000 层 abort** | 同上 |
| 嵌套 `${x:-…}` | 3 000 层 OK，**6 000 层 abort** | 同上（`nestedParameterExpansion`） |
| 嵌套 `$( … )` | 50 层干净报错 | 有 `maxSubstitutionDepth`（默认 50） |
| 递归函数 `f(){ f; }` | **立即 abort**（`function_depth` 只增不减检查） | `maxCallDepth`（默认 100） |

复现：

```bash
$BIN "$(python3 -c "print('('*800 + 'echo hi' + ')'*800)")"
# fatal runtime error: stack overflow, aborting
$BIN 'f(){ f; }; f'
# fatal runtime error: stack overflow, aborting
```

根因：

- `crates/just-bash-parser/src/lib.rs` 的 `ParseLimits` 只有一个字段
  `max_command_substitution_depth`；`parse_statements` / `parse_single_statement` /
  group / subshell / if / while 的递归都没有守卫。上游是**一个共享的 depth budget**，
  8 种嵌套形状（算术、条件、参数、替换、花括号……）统一计数。
- `crates/just-bash-shell/src/lib.rs` 里 `function_depth` 在 `call_function` 中自增，
  但从未与任何上限比较（`grep function_depth` 只有增/减/`== 0` 三处用途）。
- 全仓库没有 `catch_unwind` 或 panic hook，abort 会直接进入宿主进程。

**建议归属**：解析侧 → `just-bash-parser`（`ParseLimits` 增加 `max_nesting_depth: 200`，
用一个 `DepthGuard` 包住所有递归入口）；执行侧 → `just-bash-shell`
（`ExecutionLimits` 增加 `max_call_depth: 100`）。两侧都要有"深嵌套返回干净错误"的回归测试。

### P0-2 无限循环：`read` 在 EOF 返回 0

```bash
$BIN 'printf "a\nb\n" > f; while read l; do echo "[$l]"; done < f'
# [] [] [] … 一直打到 100000 次循环上限才停（应为 [a] [b]）
$BIN 'read x < /dev/null; echo "rc=$?"'
# rc=0   （bash 为 1）
```

`read` 读的是整段 stdin 的第一行（`stdin.lines().next()`），既不消费流，也不区分
"读到空行"和"EOF"。上游 `interpreter/builtins/read.ts` 返回 1 at EOF，并支持
`-r/-p/-n/-N/-d/-t/-s/-a/-u`，有完整的 IFS 切分与反斜杠处理。

### P0-3 复合命令重定向被丢弃，目标文件却被截断

```bash
$BIN '{ echo a; echo b; } > /out.txt; cat /out.txt'
# a b        ← 输出仍到 stdout
$BIN '{ echo a; } > /out.txt; ls -l /out.txt'
# -rw-r--r-- 0 … /out.txt   ← 文件被创建/截断，内容丢失
$BIN 'echo hi > /tmp/o; { cat; } < /tmp/o'      # 无输出（< 被当成独立的空命令）
$BIN 'echo hi > /tmp/o; if true; then cat; fi < /tmp/o'   # 无输出
```

上游已把这些修完并各有回归测试：
`d97425df`（#326 数值 fd + #329 循环重定向 + #330 group stdin 所有权）、`2208a34e`
（恢复 group/subshell 执行）、`be55fec4`（重定向事务化：全部成功才替换）。
上游甚至在未合并的分支 `origin/fix/forward-stdin-ownership` 上继续修这个问题。

**静默数据丢失 + 输出泄漏**：脚本作者以为写进了文件，实际进了调用方的 stdout，
同时留下一个 0 字节文件。对本库的目标场景（agent 工具调用、CI 脚本）是"看起来成功、
结果错"的最坏形态。

### P0-4 复合命令不能出现在管道非首段

```bash
$BIN 'echo hi | { cat; }'        # bash: syntax error near unexpected token `newline'
$BIN 'echo hi | (cat)'           # bash: syntax error near unexpected token `('
$BIN 'printf "x\n" | while read y; do echo "$y"; done'
# bash: syntax error: unsupported construct `do'
```

只有**首段**可以是复合命令（`for … done | cat` 能跑），后续段只接受简单命令。
`cmd | while read line` 是 shell 脚本里最常见的逐行处理写法，目前完全不可用。
（好消息：这是**干净报错**，不是静默错误。）

**建议归属**：`just-bash-parser` 的管道解析循环需要允许任意 `StatementKind` 作为段，
并给 `ForStatement`/`WhileStatement`/`IfStatement`/`Group`/`Subshell` 加上 `redirects`
字段（当前结构体里没有）。

---

## 2. P1 — 静默错误输出（fail-closed 原则的破口）

`THREAT_MODEL.md` 第 25–33 行声明"不支持的构造会 fail closed"。实测中，
**一批不支持的语法不是报错，而是悄悄产生错误结果**——这比报错危险得多。

### P1-1 字段分割（word splitting / IFS）整体缺失 ★影响面最大

```bash
$BIN 'x="a b c"; printf "[%s]" $x; echo'                  # [a b c]     bash: [a][b][c]
$BIN 'for i in $(seq 1 3); do echo "item<$i>"; done'      # item<1 2 3>  bash: 3 次迭代
$BIN 'x="a b c"; for w in $x; do printf "<%s>" "$w"; done # <a b c>      bash: <a><b><c>
$BIN 'IFS=:; x="a:b:c"; printf "[%s]" $x; echo'           # [a:b:c]     IFS 完全无效
```

- 未加引号的变量展开、命令替换、算术展开**从不做 IFS 切分**；`$x` 里带换行时
  `echo $x` 保留换行（bash 会折成空格）。
- 上游有专门模块：`interpreter/helpers/ifs.ts`（`splitByIfsForExpansion`）、
  `interpreter/expansion/word-split.ts`（`smartWordSplit`），并且仍在
  `origin/fix/leading-ifs-word-splitting` 分支上修边界（`9126ab1f preserve leading
  IFS boundaries`、`c8d99895 preserve quoted IFS boundaries`）。
- 这一条会让 `for f in $(ls …)`、`cmd $(cat list)`、`set -- $(…)` 等**最高频写法
  静默退化成单次迭代**。

**建议归属**：`just-bash-shell` 的展开层。建议把 `expand_word`/`expand_word_list`
拆出独立的 `expansion` 子模块（见 §5），因为后续 5–7 条都会落在同一处。

### P1-2 参数模式删除/替换全体缺席（静默返回原值）

```bash
$BIN 'p=pre_mid_suf; echo "${p#*_}"; echo "${p##*_}"; echo "${p%suf}"; echo "${p//_/-}"'
# pre_mid_suf ×4   （bash: mid_suf / suf / pre_mid / pre-mid-suf）
$BIN 'x=abc; echo ${x^^}'     # abc（bash: ABC）
```

上游：`expansion/pattern-removal.ts`、`pattern-replacement.ts`、`pattern.ts`、
`array-prefix-suffix.ts`、`variable-attrs.ts`，且 `d91dce84` 专门把 pattern-removal
的编译逻辑共享出来。这些函数式接口正好可以复用本库已有的 `just-bash-glob`
（`pattern_matches`）与 `just-bash-regex`。

### P1-3 算术表达式只实现了 5 个运算符

实测评测（`echo $((…))`）：

| 支持 | 不支持（干净报错） |
| --- | --- |
| `+ - * / %`、括号、一元 `+`、变量名 | `< > <= >= == !=`、`&& \|\| !`、`& \| ^ ~`、`<< >>`、`? :`、`**`、`= += -= *= /= %=`、`++ --`、`0x`/`0` 字面量、逗号 |

```bash
$BIN 'i=1; ((i++)); echo $i'    # bash: i++: command not found   （(( )) 命令形式完全没有）
$BIN 'echo $((1<2))'            # bash: 1<2: syntax error in expression
$BIN 'echo $((0x10))'           # bash: 0x10: syntax error in expression
```

`eval_arithmetic_expr` 是一个约 100 行的 shunting-yard，只有 7 个 token 类型。
上游有完整的 `parser/arithmetic-parser.ts` + `arithmetic-primaries.ts` +
`interpreter/arithmetic.ts`，并有 `arith.test.sh` 等 spec 用例。

### P1-4 解析器层缺失：反引号、`$'…'`、花括号展开、`~` 展开

```bash
$BIN 'printf "[%s]" `echo x y`; echo'      # [`echo][x][y`]  ← 反引号被当普通字符，还会额外分词
$BIN 'echo $'"'"'a\tb'"'"''                # $a\tb          ← ANSI-C 引号不支持
$BIN 'echo {1..3}'; $BIN 'echo {a,b}c'     # {1..3} / {a,b}c  ← 花括号展开不支持
$BIN 'echo ~'; $BIN 'echo ~/x'             # ~ / ~/x          ← 波浪号展开不支持
```

`grep -rn 'tilde\|Tilde' crates/*/src/*.rs` 为空；解析器里 `Brace` 只作为 `{ }` 分组
token 存在；反引号在 lexer 中无分支。上游分别对应 `expansion/tilde.ts`、
`expansion/brace-range.ts`、`lexer.ts:1639` 的反引号处理。

反引号这条尤其要注意：它是**静默错误**（多切出字段），不是报错。

### P1-5 内建命令的选项解析层缺一块（静默 + 误报）

```bash
$BIN 'echo abc | tr -d b'          # abc   （bash: ac —— -d 被完全忽略）
$BIN 'echo abc | tr "[:lower:]" "[:upper:]"'   # abc（字符类被忽略）
$BIN "printf 'a bc\n' | cut -d: -f2"     # cut: usage…  （附着形式 -d: -f2 不识别）
$BIN 'printf "b\na\n" | sort -n'         # sort: /home/user/-n: No such file or directory
$BIN 'printf "a\na\n" | uniq -c'         # uniq: /home/user/-c: … 
$BIN 'printf "a\nb\n" | head -n1'        # head: /home/user/-n1: …
$BIN "printf '%5d|' 42"                  # %5d|   （宽度/精度/标志全部不支持）
```

三类问题混在一起，值得统一处理：

1. **把未知选项当文件操作数**（`sort -n`、`uniq -c`、`head -n1`）→ 报"文件不存在"，
   语义完全错位；
2. **认识但忽略**（`tr -d`）→ 静默错误输出；
3. **附着写法不支持**（`cut -d:`、`head -n1`）→ 误报 usage。

上游有 `commands/flag-coverage.ts` + `fuzz-flags.ts` 专门做"每个命令每个 flag 都有
覆盖"的检查，本库可以对标做一个轻量版：一张 `(command, flag) -> 行为` 表 + 测试。

`printf` 宽度/精度缺失也值得单列：agent 生成的脚本里 `printf '%05d'`、`printf '%-10s'`
非常常见，当前会**原样打印格式串**。

### P1-6 特殊变量缺失

```bash
$BIN 'echo "pid=$$ lineno=$LINENO random=$RANDOM"'   # pid=1 lineno= random=
$BIN 'echo $BASH_VERSION'                            # 空
```

`$$` 恒为 1，`$LINENO`/`$RANDOM`/`$SECONDS`/`$FUNCNAME`/`$BASH_SOURCE`/`$UID`/`$EUID`/
`$PPID`/`$OSTYPE`/`$HOSTNAME`/`$SHELLOPTS` 全空。上游在 `Bash.ts` 里用
`processInfo`（virtual pid/ppid/uid/gid）+ `HOST_VARS` 表统一注入，保证不泄漏真实主机信息。
本库需要一张同样语义的虚拟值表——这既是兼容性，也是安全边界的一部分。

---

## 3. P2 — Shell 内建命令缺口

上游把 shell 内建放在 `interpreter/builtins/`，每个文件带测试：

```
break cd continue declare exit export getopts hash help let local mapfile read
return set shift source unset compgen complete compopt dirs eval shopt
```

本库 `just-bash-shell` 的 `match command` 已实现 58 个名字（其中 7 个是 fail-closed 桩），
**缺失的 shell 内建**：

| 类别 | 缺失命令 | 影响 |
| --- | --- | --- |
| 脚本组织 | `eval`、`source`/`.`、`command`、`builtin`、`type`、`hash` | 上游/CI 脚本几乎都依赖 `source` 和 `eval` |
| 参数 | `getopts`、`mapfile`/`readarray` | 标准参数解析 |
| 变量属性 | `readonly`、`declare -n`（nameref）、`export -f` | 状态语义 |
| 环境/杂项 | `umask`、`shopt`、`alias`/`unalias`、`history`、`times`、`dirs`/`pushd`/`popd`、`ulimit`、`exec` | 兼容性 |
| 算术 | `let` | 与 P1-3 同批 |
| 帮助 | `help` | 可发现性 |

**架构建议**：这些**不要**做成新 crate——它们需要直接读写 shell 状态（变量表、函数表、
jobs、选项）。正确落点是 `just-bash-shell` 内部的 `builtins/` 子模块（见 §5）。
其中 `eval`/`source` 需要复用解析器 + 执行器，属于 shell 内部能力，天然属于该 crate。

---

## 4. P3 — 命令目录缺口（上游 85 vs 本库 51）

上游 `commands/registry.ts` 的 `CommandName` 共 **85** 个；本库 dispatch 实现的命令名
**51** 个（另有 7 个 fail-closed 桩）。**完全缺失 52 个**：

```
alias base64 bash clear column comm date diff du egrep expand expr fgrep file find fold
gunzip gzip help history hostname html-to-markdown join ln md5sum nl od paste printenv
readlink rev rg rmdir sh sha1sum sha256sum split strings tac tar tee time timeout tree
unalias unexpand which whoami xan xargs yq zcat
```

按本库"一个语义家族一个 crate"的惯例，建议分四批落地（每批都能独立发布、独立测试）：

| 批次 | 建议 crate | 覆盖命令 | 上游体量 | 依赖 | 理由 |
| --- | --- | --- | --- | --- | --- |
| **B1 文件系统补全** | 扩展现有 `just-bash-shell` 或新建 `just-bash-coreutils` | `ln` `readlink` `rmdir` `which` `whoami` `hostname` `printenv` `clear` `help` `tac` `tee` | 小（各 1–13 KB） | `just-bash-fs` | 单价低、高频、立刻可用；`ln`/`readlink` 需要先把 §6 的符号链接补上 |
| **B2 文本工具族** | `just-bash-textutils` | `comm` `paste` `join` `column` `rev` `nl` `fold` `expand`/`unexpand` `strings` `split` `od` `base64` `md5sum`/`sha1sum`/`sha256sum` `expr` `date` `diff` `xargs` | 中（合计 ~350 KB TS） | `just-bash-fs` + `just-bash-glob` | 与 awk/jq/sed 同构：纯函数 + stdin 流，适合独立 crate |
| **B3 遍历与归档** | `just-bash-traverse`、`just-bash-archive` | `find` `tree` `du`、`gzip`/`gunzip`/`zcat` `tar` | 大（find 218 KB、tar 176 KB） | `just-bash-fs` | 都需要**遍历预算**（参见 §7）；上游有 `maxTraversalEntries/Depth/Work`、`maxArchiveEntries/Bytes` |
| **B4 大数据引擎** | `just-bash-query`（yq/xan 共用 query-engine） | `rg` `yq` `xan` `html-to-markdown` | 很大（rg 354 KB、xan 288 KB、yq 127 KB） | 复用 `just-bash-jq` 的求值器 | 收益/成本比最低，建议**最后**做，或明确不做 |

本库与上游的坐标差异要写进 README，避免使用者误判：

- 上游 `bash`/`sh` 是**内建**（可嵌套执行脚本）；本库没有，脚本里 `bash -c` 会 127。
- 上游 `python3`/`node`/`js-exec` 走 WASM/worker，本库按架构**故意** fail-closed；
  建议把这条从"未完成"改为"**已决定不做**"，并在 `THREAT_MODEL.md` 里写明退出码与
  错误串（当前是 127 + "does not embed an interpreter"）。

---

## 5. P4 — 架构层面：`just-bash-shell` 需要内部拆分

现状：`crates/just-bash-shell/src/lib.rs` 单文件 **7 822 行**，同时装着
展开、执行、重定向、jobs、进程替换、以及 40+ 个内建命令。
对照上游（`interpreter/` + `interpreter/expansion/` 20 个模块 + `builtins/` 30 个文件），
本库的展开层缺口（§2 的 P1-1/P1-2/P1-4）几乎全部落在同一个文件里，
继续在单文件上叠加会让 review 和冲突成本快速上升。

建议的拆分（不改变 crate 边界，只改模块边界，对公共 API 零影响）：

```text
crates/just-bash-shell/src/
  lib.rs          // Bash 状态机 + exec 入口 + 公共 API re-export
  expansion/      // mod.rs, word.rs, parameter.rs, pattern.rs, split.rs, tilde.rs, brace.rs
  exec/           // pipeline.rs, redirect.rs (Sink/fd), control_flow.rs, jobs.rs, procsub.rs
  builtins/       // mod.rs（dispatch 表）+ 每个内建一个文件
  arith.rs        // 独立算术求值器（或提升为 just-bash-arith crate）
```

约束（来自 `AGENTS.md`）：

- 新增 crate 时必须同步 `crates/just-bash` 门面（当前 `public_api.rs` 只覆盖既有类型）。
- 不得引入新的运行时依赖——上述拆分不违反，纯 std。
- 拆分与行为改动**分开提交**，先做纯搬家（`git mv` + `mod` 声明），再在搬家后的模块里改语义。

---

## 6. P4 — 文件系统抽象：符号链接与二进制

`crates/just-bash-fs/src/lib.rs` 是纯内存 `BTreeMap<String, String>`，注释明确写着
"there are no symlinks"。由此产生的缺口：

| 能力 | 本库 | 上游 | 后果 |
| --- | --- | --- | --- |
| 符号链接 | 无（`test -L` 恒 false） | `MAX_SYMLINK_DEPTH = 40`、`readlink`、`ln -s` | `ln`/`readlink`/`find -type l` 无法实现 |
| 挂载/叠加 | 无 | `MountableFs`、`ReadWriteFs`、OverlayFS、延迟文件提供者 | 无法把宿主目录映射成只读根（上游 CLI `--root` 的核心能力） |
| 二进制内容 | `String` | `stdoutKind`/`stdinKind`、字节级 head/tail（`9481331f`） | 上传/下载、`od`、`base64`、gzip、`tar` 都会失真 |
| 字节配额 | 无 | `maxFileSystemBytes`（默认 1 GiB） | 脚本可无限写内存 |
| 时间戳 | 逻辑时钟 `tick()` | 真实 mtime + `date` 本地时区（`fb7ae3cc`） | 确定性更好，但 `date`/`find -mtime` 语义受限 |

**建议顺序**：先做 `FileContent = Vec<u8>`（或 `enum { Text, Bytes }`）与
`max_file_system_bytes`，再做符号链接（含 `..` 逃逸与深度校验，`THREAT_MODEL.md`
第 81–88 行已预留该风险位），最后才考虑挂载表——挂载是宿主 FS 边界，属于安全等级变化，
需要单独的设计与威胁模型更新。

---

## 7. P4 — 资源限额体系：5 个 vs 上游 45 个

本库 `ExecutionLimits` 只有 5 项：`max_script_size_bytes`、`max_command_count`、
`max_command_substitution_depth`、`max_output_bytes`、`max_loop_iterations`。
上游 `limits.ts` 有 **45 项**，并且有 `normal` / `hardened` 两套预设
（`ExecutionLimitProfile` + `resolveLimits`）。

本库**完全没有**的限额（按风险排序）：

| 缺失限额 | 上游默认 | 风险 |
| --- | --- | --- |
| `maxCallDepth` | 100 | 函数递归 → 栈溢出 abort（P0-1） |
| `maxExecDepth` | 64 | 嵌套执行 |
| `maxSourceDepth` | 100 | `source` 递归（配合 §3） |
| 解析器嵌套深度 | 200 | 栈溢出 abort（P0-1） |
| `maxLiveBytes` / `maxInputBytes` | 512 MiB | 内存耗尽 |
| `maxFileSystemBytes` | 1 GiB | 内存耗尽 |
| `maxStringLength` | 64 MiB | 超长字符串 |
| `maxArrayElements` | 1 000 000 | 数组 |
| `maxHeredocSize` | 64 MiB | heredoc |
| `maxBraceExpansionResults` | 100 000 | 花括号展开（配合 §2 P1-4） |
| `maxGlobOperations` | 1 000 000 | glob |
| `maxTraversalEntries/Depth/Work` | — | `find`/`tree`/`du`（配合 §4 B3） |
| `maxArchiveEntries/Bytes` | — | `tar`/`gzip`（配合 §4 B3） |
| `maxFileDescriptors` | 4 096 | fd 表 |
| `maxExecutionTimeMs` | 1 小时 | **墙钟**——`THREAT_MODEL.md` 第 86 行自己承认没有 |

架构建议：

- 限额定义**放回各 crate**（parser 的归 parser，fs 的归 fs，命令族的归命令族），
  但由一个 `Limits` 聚合结构在 `Options` 里逐层下发，避免出现"30 个字段的上帝结构"。
- 一定要加 **`maxExecutionTimeMs`**：`while :; do :; done` 目前在纯 CPU 上跑到循环上限，
  没有时间维度。
- 加上 `catch_unwind` 作为**最后一道**防线（不替代 P0-1 的修复）：把 `Bash::exec` 的
  主体包起来，panic 转成干净的错误结果，而不是让 embedder 进程死掉。仓库里
  78 处 `unwrap()` + 9 处 `expect()`（shell）、70 处 `unwrap()`（net）都是潜在 panic 点。

---

## 8. P5 — 嵌入 API 与 CLI

### 8.1 库 API

| 上游 `BashOptions` / `ExecOptions` | 本库 `Options` / `exec` | 影响 |
| --- | --- | --- |
| `files`/`env`/`cwd` | ✅ 有 | — |
| `customCommands`（自定义命令，`c5a2a4a3` 传入原始命令行） | ❌ 无 | **无法扩展**——embedder 只能跑内置命令 |
| `commands: CommandName[]`（白名单） | ❌ 无 | 无法按能力裁剪命令集 |
| `fs?: IFileSystem`（注入文件系统） | ❌ 无（硬编码 `InMemoryFs`） | 无法对接宿主存储 |
| `executionLimitProfile`（normal/hardened） | ❌ 无 | 见 §7 |
| `logger` / `trace` / `coverage` | ❌ 无 | 无法观测/统计覆盖 |
| `processInfo`（虚拟 pid/uid） | ❌ 无 | 见 §2 P1-6 |
| `sleep` 注入（测试用假时钟） | ❌ 无 | `sleep` 不可 mock |
| `exec(script, { env, cwd, stdin, args, signal })` | ❌ `exec(&str)` 只有脚本 | 无法传 stdin、无法取消、无法 per-exec env |
| `ExecResult`（stdout/stderr/exitCode/**env**） | `ExecResult` 无 `env` | 上游正在 `origin/fix/result-env-isolation` 分支修这块 |

最小可用的补齐（按价值排序）：

1. `customCommands` 注册钩子（否则本库只是个固定命令集的解释器）；
2. `exec` 的 `stdin` / `signal`（取消）/ `env` / `cwd` 覆盖；
3. `ExecResult.env`（或明确文档化为"不提供"）；
4. `commands` 白名单；
5. `logger`/`trace`（可选 trait，零依赖）。

### 8.2 CLI

本库 CLI（`crates/just-bash/src/main.rs`，16 行）把 argv **用空格拼成一个字符串**再解析：

```bash
$BIN printf '%s\n' 'a  b'
# anbn        ← 期望 a  b
```

连"把参数原样传给命令"都做不到。上游 CLI 支持：
`-c <script>`、`-e/--errexit`、`--root <path>`（OverlayFS 只读挂载宿主目录）、
`--cwd`、`--allow-write`、`--json`、`-h`、`-v`，以及"脚本文件 / stdin 管道"两种输入。

本库 CLI 目前 `-c`/`--help` 会被当成脚本执行（`bash: -c: command not found`，且 **exit 0**）。
建议最小补齐：`-c`、`--json`、`-e`、`-h`、`-v`，并修正退出码
（"命令未找到"不应以 0 退出）。

---

## 9. P5 — 一致性回归资产（性价比最高的一项）

仓库自身的 git 历史里就有上游的整套对照语料（在 `d64009a` 可达），当前**完全没被使用**：

| 资产 | 数量 | 位置（本仓库历史内） |
| --- | --- | --- |
| bash 对照夹具（JSON：command/files/stdout/stderr/exitCode） | **457 条** | `packages/just-bash/src/comparison-tests/fixtures/*.json`（93 KB） |
| spec 用例（`####` 标题块） | **2 728 条** | `packages/just-bash/src/spec-tests/bash/cases/*.test.sh`（136 个文件） |
| 夹具运行器实现（可参考的语义） | 1 | `comparison-tests/fixture-runner.ts` |

对照夹具的格式对 Rust 极其友好（无 TS 依赖）：

```json
{ "217dbca3c809b994": { "command": "echo -n hello", "files": {},
  "stdout": "hello", "stderr": "", "exitCode": 0 } }
```

建议新增 `crates/just-bash-conformance/`（dev-only 或独立 crate）：

1. 把 457 条 JSON 直接复制进 `crates/just-bash-conformance/fixtures/`（JSON 本身就是数据，
   不违反"Rust-only"约束——它是**测试数据**而非 TS 运行时）；
2. 一个 Rust runner：解析夹具 → 用 `Options { files }` 建 shell → 比对 stdout/stderr/exitCode；
3. 一张 `known_failures.txt` 显式列出当前不支持的用例（让失败是**可知的**，而不是掩盖）；
4. CI 里跑出通过率数字，写回 `UPSTREAM_SYNC.md`。

这会直接把"上游同步"从**文档描述的承诺**变成**可执行、可量化的指标**，
并立即为 §1/§2 的每条修复提供回归网。

---

## 10. 建议的落地顺序（PR 切片）

每个切片都应独立可合、可验证；括号内是"验收标准"。

| # | 切片 | 落点 | 规模 | 状态 |
| --- | --- | --- | --- | --- |
| **1** | 解析器共享深度预算（8 种嵌套形状） | `just-bash-parser` | S | ✅ 已完成（默认 32） |
| **2** | 执行侧 `max_call_depth` + `max_expansion_depth` + `catch_unwind` 兜底 | `just-bash-shell` | S | ✅ 已完成 |
| **3** | `read` EOF 返回 1 | `just-bash-shell` | S | ✅ 已完成（`-r/-n/-d/-a`、流消费仍待补） |
| **3b** | `read` 完整化（`-n/-N/-d/-t/-a/-u`、IFS 反斜杠） | `just-bash-shell` | M | 待做 |
| **4** | 复合命令重定向 + group/subshell 共享 stdin | `just-bash-parser` + `just-bash-shell` | L | ✅ 已完成 |
| **5** | 字段分割（IFS）+ 引号内替换保护 | `just-bash-shell`（展开层）+ `just-bash-parser` | M | ✅ 已完成 |
| **6** | 参数模式删除/替换 + `<code>${x^^}</code>` | 展开层（复用 `just-bash-glob`） | M | ✅ 已完成 |
| **7** | 完整算术求值器 + `(( ))` + `let` | `just-bash-shell::arith` 模块 | L | ✅ 已完成 |
| **8** | 内建选项统一解析（`sort`/`cut`/`head`/`tail`/`uniq`/`wc`） | `just-bash-shell::opts` | M | ✅ 已完成（`printf` 宽度仍待做） |
| **9** | `eval` / `source` / `command` / `type` | `just-bash-shell` | M | ✅ 已完成（`getopts`/`readonly`/`alias` 待做） |
| **10** | 特殊变量表（虚拟 pid/uid/host）+ `LINENO`/`RANDOM`/`SECONDS` | `just-bash-shell` | S | 待做 |
| **11** | conformance 夹具 + 通过率 | `just-bash/tests/conformance.rs` | M | ✅ 已完成（276/457 = 60%） |
| **12** | B1 文件系统补全（`ln` `readlink` `rmdir` `tee` …） | 见 §4 | M | 待做 |
| **13** | 二进制内容 + `maxFileSystemBytes` + 符号链接 | `just-bash-fs` | L | 待做 |
| **14** | 嵌入 API（`customCommands`、`exec` 选项、`ExecResult.env`） | `just-bash-shell` + 门面 | M | 待做 |
| **15** | CLI（`-c`/`--json`/`-e`/`-h`/`-v` + 退出码） | `just-bash` | S | 待做 |
| **16+** | B2/B3/B4 命令族 | 各新 crate | L 各 | 待做 |

依赖关系：1→2（深度预算先于兜底）、4 依赖 1（重定向解析需要深度守卫已在位）、
11 应在 3–8 之后（否则通过率基线没有意义）、13 是 12 中 `ln`/`readlink` 的前置。

**不建议现在做**：B4（rg/yq/xan）——上游这三个加起来约 770 KB TS，
与"纯 std、确定性"的架构收益比最低；建议在 README 里显式标注为不支持。

---

## 11. 对 `UPSTREAM_SYNC.md` 的两点维护建议

1. **"Not yet ported" 列表不准确**：现在写的 `find -exec`、`select`/`coproc`/`time`/`let`、
   `${!prefix*}`、nameref 等只是很小一部分。真实性更强的清单是本文 §2/§3/§4，
   建议把 `UPSTREAM_SYNC.md` 的该节替换为"指向 `UPSTREAM_GAPS.md` + 一句结论"，
   避免出现两个互相矛盾的清单。
2. **"当前 slice 已移植"的表里混了两批内容**（文档中出现两次同名小节 `## Ported in the
   current slice`）。建议合并成一张表，并在每行补上**验证方式**（哪个测试覆盖），
   否则"已移植"只是文档声明。

---

## 附：本次验证用到的关键命令

```bash
# 崩溃类
$BIN "$(python3 -c "print('('*800 + 'echo hi' + ')'*800)")"   # stack overflow, aborting
$BIN 'f(){ f; }; f'                                            # stack overflow, aborting

# 静默错误类
$BIN 'x="a b c"; printf "[%s]" $x; echo'                       # [a b c]
$BIN 'p=pre_mid_suf; echo "${p#*_}"'                           # pre_mid_suf
$BIN '{ echo a; } > /out.txt; cat /out.txt'                    # 输出到 stdout，文件 0 字节
$BIN 'echo abc | tr -d b'                                      # abc
$BIN "printf '%5d|' 42"                                        # %5d|

# 循环/挂起类
$BIN 'printf "a\nb\n" > f; while read l; do echo "[$l]"; done < f'   # 打到循环上限

# 能力盘点
cargo test --workspace            # 622 passed
cargo clippy --workspace --all-targets   # 0 warnings

# 上游参照（在 ../vercel-labs/just-bash）
git show 58d1ecf3:packages/just-bash/src/limits.ts
git show 58d1ecf3:packages/just-bash/src/parser/types.ts | grep MAX_PARSER_DEPTH
git ls-tree -r --name-only d64009a packages/just-bash/src/comparison-tests/fixtures/   # 在本仓库即可
```

---

## 12. 已落地：宿主进程安全（切片 1–3）

本次改动把 §1 的三个"进程级 abort"缺口全部变成干净报错，没有改变任何
受支持脚本的行为。

### 新增限额（全部可由 embedder 覆盖）

| 限额 | 默认值 | 位置 | 对应漏洞 |
| --- | --- | --- | --- |
| `ParseLimits::max_nesting_depth` | **64** | `just-bash-parser` | 嵌套 `( )`/`{ }`/`if`/`while`/`for`/`case`/函数体/`!` 链 |
| `ExecutionLimits::max_nesting_depth` | 64 | `just-bash-shell`（下传给解析器） | 同上，embedder 入口 |
| `ExecutionLimits::max_call_depth` | 100 | `just-bash-shell` | 递归函数 `f(){ f; }` |
| `ExecutionLimits::max_expansion_depth` | 100 | `just-bash-shell` | `${x:-${x:-…}}` 展开递归 |

设计要点：

- **单一共享预算**：`DepthGuard` 用同一个计数器覆盖所有递归构造，避免
  "交替嵌套"（`{ if ( $( … ) ) }`）各自消耗私有预算而放大实际栈深。
- **默认值按最小栈来定**：实测每层解析约消耗 10 KB Rust 栈，2 MiB 线程栈下
  200 层即溢出，1 MiB 下 100 层溢出。因此默认取 **64**（1 MiB 栈安全），
  低于上游的 200 —— 上游是 JS，每层开销远小于 Rust 递归下降。
  embedder 若掌握栈大小可通过 `ExecutionLimits` 上调。
- **`catch_unwind` 只兜 panic，不兜 abort**：栈溢出是 abort，无法捕获，
  所以真正的防线是上面的深度预算；`catch_unwind` 覆盖 `unwrap()` 之类的
  意外 panic，转换为 `bash: internal error: …` + 退出码 2，并清空逐命令
  瞬时状态。需要说明的是：panic 之前已发生的 fs/变量修改不会回滚。

### `read` 的 EOF 语义

`read` 在 stdin 为空时返回 1（此前恒为 0）。这让
`while read l; do …; done` 在 stdin 未连接时立即结束，而不是空转到
100 000 次循环上限。注意这不是 `while read` 的完整修复——复合命令重定向
（`done < file`）仍然被丢弃，属于切片 4。

### 验证

```bash
cargo fmt --all --check        # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace         # 635 passed（新增 13 个回归测试）
```

新增测试：
- `just-bash-parser::nesting_limit_tests`（5 个）：普通嵌套放行、超深拒绝、
  `!` 链拒绝、共享预算、自定义限额。
- `just-bash-shell::host_safety_tests`（8 个）：递归函数、互递归、深参数嵌套、
  深语法、**1 MiB 小栈不崩**、`read` EOF、空行不等于 EOF、普通脚本不受影响。

改动前后对比（同一个脚本，同一台机器）：

| 脚本 | 改动前 | 改动后 |
| --- | --- | --- |
| 800 层 `( … )` | `fatal runtime error: stack overflow, aborting` | `bash: maximum parser nesting depth exceeded (64)`，rc=2 |
| `f(){ f; }; f` | 同上 abort | `bash: maximum function call depth exceeded (100)`，rc=1 |
| 6 000 层 `${x:-…}` | 同上 abort | `bash: maximum expansion depth exceeded (100)`，rc=1 |
| `read x`（stdin 空） | `rc=0` | `rc=1` |

### 仍未覆盖（需要后续切片）

1. **`while read … done < file` 仍然无效**：`< file` 被解析成一个独立的空命令，
   循环体拿不到 stdin。切片 4。
2. **复合命令输出重定向被丢弃**：`{ echo a; } > f` 输出到 stdout，`f` 被截断成
   0 字节。切片 4。
3. **整段脚本没有墙钟上限**：`while :; do :; done` 只受循环次数约束。对应
   §7 的 `maxExecutionTimeMs`。
4. **`read` 的其余选项**（`-n/-N/-d/-t/-a/-u`、反斜杠续行、IFS 精细切分）
   仍未实现：切片 3b。

---

## 13. 已落地：复合命令重定向（切片 4）

这是 §1 P0-3/P0-4 的修复，也是此前唯一还在**静默丢数据**的问题。

### 修复前后

| 脚本 | 改动前 | 改动后 |
| --- | --- | --- |
| `{ echo a; echo b; } > /o; cat /o` | 输出到 stdout，`/o` 是 0 字节 | `a b` 写入 `/o` |
| `if true; then echo y; fi > /o` | 同上 | `y` 写入 `/o` |
| `for i in 1 2; do echo $i; done > /o` | 同上 | `1 2` 写入 `/o` |
| `{ read x; read y; } < /i` | stdin 为空 | 读到第 1、2 行 |
| `while read l; do …; done < /i` | 死循环到 100 000 次上限 | 正常逐行迭代 |
| `{ cat; } < /i` | 无输出 | 输出文件内容 |

### 实现要点

1. **AST**：新增 `StatementKind::Redirected { inner, redirects }`。复合命令后的
   重定向属于整个复合命令，而不是其中最后一条简单命令（与 bash 一致）。
2. **解析**：`parse_redirect_token` 抽成复用函数（简单命令与复合命令共用同一套
   fd/`2>&1`/`>&-`/heredoc 解析），`with_trailing_redirects` 负责把尾随重定向
   挂到 if/while/until/for/case/group/subshell/`[[ ]]`/函数定义上。
3. **执行**：`apply_redirects` 从 `run_invocation` 抽出，复合命令与简单命令共用
   同一套「按书写顺序解析描述符」的规则（`2>&1 > f` ≠ `> f 2>&1`）。
4. **共享 stdin 游标** `StdinBuffer`：复合命令的 `<` 输入是一个带偏移量的缓冲区，
   循环体内的多次 `read` 依次推进，而不是每次重读整个文件。只有**真正读 stdin**
   的命令（`cat`/`grep`/`sort`/`uniq`/`cut`/`tr`/`head`/`tail`/`wc`/`sed`/`awk`/`jq`）
   才消费剩余字节，`read` 消费一行，其余命令不消费——否则循环体里的 `echo`
   会把输入吃光。

### 已知收窄（已写入 `THREAT_MODEL.md` 第 10、11 条）

- 名单外的命令若在重定向复合命令里读 stdin，会看到空流而不是剩余字节。
  这是对真实 fd 语义的**收窄**，不是逃逸。
- 复合命令仍不能作为管道的非首段（`echo hi | { cat; }` 仍是解析错误，
  属于 §1 P0-4 的另一半，需要把管道段类型从 `CommandInvocation` 扩展为
  `StatementKind`，留作后续切片）。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 651 passed（新增 16 个）
```

新增测试 `just-bash-shell::compound_redirect_tests`（16 个）覆盖：
group/if/for/case/subshell 的 `>` 与 `>>`、`2>&1` 合并、独立 `2>`、
共享 stdin 游标、`while read` 逐行、循环体不吃掉输入、`sort` 读重定向输入、
缺失输入文件报错、函数内复合重定向。

---

## 14. 已落地：IFS 字段分割（切片 5）

这是 §2 P1-1 的修复，也是**影响面最大的一处静默错误**：
`for f in $(ls)`、`cmd $list`、`set -- $x` 这类写法此前全部退化成单次迭代/单个参数，
不报错、不提示。

### 修复前后

| 脚本 | 改动前 | 改动后（与 bash 一致） |
| --- | --- | --- |
| `x="a b c"; for i in $x; do echo $i; done` | `<a b c>`（1 次） | `<a>` `<b>` `<c>` |
| `x="a b c"; set -- $x; echo $#` | `1` | `3` |
| `set -- $(echo one two three)` | `1` | `3` |
| `IFS=:; x="a::b"; set -- $x` | `1` | `3`（含中间空字段） |
| `x=""; set -- $x; echo $#` | `1` | `0` |
| `x=""; set -- "$x"; echo $#` | `1` | `1`（引号保留空字段） |
| `x=" a"; set -- pre$x` | `1` | `2`（`pre` 与 `a`） |

### 实现要点

1. **POSIX 分割规则**（`split_fields`）：IFS 空白折叠并去除首尾；每个非空白 IFS 字符
   单独作定界符，因此 `a::b` 产生空字段；单个尾随定界符不追加空字段；非空白定界符
   两侧的空白被吸收（`IFS=" :"` 下 `"a : b"` 是 2 个字段）；`IFS=''` 完全关闭分割。
   全部规则以 `bash 3.2` 的实测输出为基准写成对照表测试。
2. **拼接后才分割**：`pre$x` 先拼接成 `"pre a"` 再分割，得到 `pre`、`a`，
   与 bash 的「先展开成字符串再分割」一致；展开结尾是定界符时同时结束当前字段，
   所以 `$x"b"`（`x="a "`）得到 `a`、`b`。
3. **字符级保护掩码** `Field`：字段文本旁挂一个并行掩码，标记每个字符是否来自
   引号或反斜杠转义。渲染 glob 模式时把受保护的元字符转义，于是
   `"*.txt"`、`\*.txt` 保持字面量，而 `x='*.txt'; echo $x` 仍然展开。
   `Field.active` 单独记录「字段是否存在」，以区分 `"$x"`（一个空字段）与
   `$x`（零个字段）。
4. **顺带修掉一个解析器 bug**：词法器把双引号内的 `$( … )` 硬编码为
   `quoted = false`，导致 `"$(echo one two)"` 被错误分割、且引号内命令替换的结果
   会参与 glob。现在按引号状态传递，`"$( … )"` 是单个字段。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 675 passed（新增 24 个）
```

新增测试：
- `just-bash-shell::field_splitting_tests`（3 个）：以 bash 实测输出为基准的
  16 行对照表 + 空 IFS + 边界标志。
- `just-bash-shell::word_splitting_tests`（21 个）：for 循环、`set --`、
  引号/转义/命令替换、空展开与空引号、前导/尾随空白、非空白定界符、
  相邻定界符、字面量前后缀拼接、保护掩码、逐字段 glob。

### 仍未覆盖

- 反引号 `` ` … ` `` 命令替换、ANSI-C 引号 `$'…'`、花括号展开 `{a,b}`、
  波浪号展开 `~`：仍属 §2 P1-4，未开始。
- `$*`/`$@` 的分割细节依赖同一套 `split_fields`，但未加 IFS 相关的专项测试。

---

## 15. 已落地：参数模式操作符（切片 6）

修复 §2 P1-2：`${v#pat}`、`${v##pat}`、`${v%pat}`、`${v%%pat}`、`${v/pat/rep}`、
`${v//pat/rep}`、`${v^}`、`${v^^}`、`${v,}`、`${v,,}` 此前**全部静默返回原值**。

### 修复前后（以 bash 实测为准）

| 脚本 | 改动前 | 改动后 |
| --- | --- | --- |
| `p=abcABCabc; "${p#a}"` | `abcABCabc` | `bcABCabc` |
| `p=abcABCabc; "${p##*a}"` | `abcABCabc` | `bc` |
| `p=abcABCabc; "${p%%b*}"` | `abcABCabc` | `a` |
| `p=/a/b/c.txt; "${p##*/}"; "${p%/*}"` | 原值 | `c.txt` / `/a/b` |
| `p=a-b-c; "${p//-/+/}"` | `a-b-c` | `a+/b+/c` |
| `p=abc; "${p//*/Y}"` | `abc` | `Y` |
| `p=abc; "${p^^}"` | `abc` | `ABC` |

### 关键语义（都有对照测试）

- **最短/最长匹配**：`${p#*a}` 取最短前缀，`${p##*a}` 取最长前缀；后缀同理。
- **空匹配从不替换，且匹配在串尾停止**：`${p//a*/Y}` → `Y`，`${p//*/Y}`（空串）→ 空。
  起初未处理这两条，导致 `*` 类模式在串尾反复做零长匹配而**死循环**，已修正。
- **模式是 glob**：`*`、`?`、`[...]` 均由 `just-bash-glob` 匹配，且 `*` 可以跨越 `/`
  （`${p##*/}` 取文件名）。
- **转义保持字面**：`${p//\*/X}` 只替换真正的 `*`。
- **模式与替换串本身可以是展开**：`${p#$prefix}`、`${p/x/$y}`。
- **unset 参数按空串处理**：`${q#a}` → 空。
- 大小写转换按上游/bash 4 语义实现（bash 3.2 无此语法）。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 692 passed
```

新增 `just-bash-shell::pattern_operator_tests`（9 个）：前后缀删除对照表、
路径形态、unset/空串、替换 first/all、空匹配、glob 与转义、大小写、模式为展开、
以及既有 `${x:-default}`/`${#x}`/`${x:1:3}` 不回归。

---

## 16. 已落地：赋值前缀（切片 6b，顺手补齐的既有缺陷）

对照 HEAD 验证后确认这是**既有缺陷**（非本轮引入）：`VAR=value cmd` 从未实现，
整条命令被当作名为 `VAR=value` 的外部命令，于是最经典的逐行读取写法
`while IFS= read -r line; do …; done < file` 完全不可用。

| 脚本 | 改动前 | 改动后（与 bash 一致） |
| --- | --- | --- |
| `x=1 echo hi` | `bash: x=1: command not found` | `hi` |
| `x=1 env` | 同上 | 环境里可见 `x=1` |
| `x=outer; x=inner true; echo $x` | 报错 | `outer`（临时生效后还原） |
| `q='a b'; x=$q env` | 报错 | `x=a b`（赋值值不分割） |
| `while IFS= read -r l; do …; done < f` | 报错 | 逐行迭代 |

实现要点：

1. 在 `run_invocation` 里**先剥离**前导 `NAME=value` 词，再对其余词做列表展开；
   被剥离的值用 `expand_word` 展开（**不做字段分割**），与 bash 对赋值的处理一致。
2. 有后续命令时临时写入环境、命令结束后逐项还原（不存在则删除）；
   没有后续命令时是普通赋值语句，持久生效。
3. 复合命令的 stdin 归属判断改为「跳过赋值前缀后的第一个命令名」，
   否则 `IFS= read` 不会被识别成 `read`，循环会立刻 EOF 退出。
4. 支持函数调用前缀：`x=inner f` 在函数内可见、函数外还原。

新增 `just-bash-shell::assignment_prefix_tests`（8 个），覆盖可见性、还原、值不分割、
纯赋值持久、函数前缀、`IFS= read` 循环两例。

---

## 17. 已落地：解析器层展开（切片 7，覆盖 §2 P1-4）

四个此前**静默产生错误结果**的展开形式全部补齐，并逐项与 bash 3.2 做过差分对照。

| 形式 | 改动前 | 改动后 |
| --- | --- | --- |
| `` echo `echo hi` `` | `` `echo hi` ``（字面量） | `hi` |
| `echo $'a\tb'` | `$a\tb` | 制表符分隔的 `a	b` |
| `echo ~` / `~/x` | `~` / `~/x` | `/home/user` / `/home/user/x` |
| `echo {a,b}` | `{a,b}` | `a b` |
| `echo {1..3}` | `{1..3}` | `1 2 3` |

### 实现要点

1. **ANSI-C 引号 `$'…'`**（词法器）：支持 `\n \t \r \a \b \f \v \e \\ \' \" \?`、
   `\xHH`（最多两位十六进制）、`\NNN`（最多三位八进制）；未知转义保留反斜杠。
   结果作为 **Literal/受保护** 部件，不会再被展开或分割，因此 `$'$HOME'` 仍是字面量。
   双引号内不识别（`"$'a\tb'"` 是普通文本），与 bash 一致。
2. **反引号命令替换**（词法器）：转换为 `CommandSub` 部件，引号状态一并带上，
   因此 `"`echo a b`"` 不分割、`` `echo a b` `` 分割；支持 `\``/`\\`/`\$` 转义与
   嵌套反引号；单引号内保持字面量。
3. **波浪号展开**（展开层）：词首未加引号的 `~`（单独出现或紧跟 `/`）替换为 `$HOME`；
   `a~b`、`"~"` 不展开；赋值右值同样展开（`x=~/y`）。`~user` 形式未实现（虚拟环境无
   真实用户库），保持字面量，已在文档标注。
4. **花括号展开**（展开层）：`{a,b}`、嵌套 `{a,{b,c}}`、前后缀 `x{a,b}y`、
   整数/单字符区间 `{1..3}`、`{3..1}`、`{a..c}`；`{a}` 与 `"{a,b}"` 保持字面量。
   仅在「单词是单个未加引号、且不含 `$`/反引号/反斜杠的字面量」时生效，
   展开结果各自再做 glob，因此 `echo *.{txt,md}` 可用。递归深度上限 16，
   区间长度上限 10 000，避免爆炸。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 707 passed（新增 15 个）
```

新增 `just-bash-shell::parser_expansion_tests`（15 个）：ANSI-C 转义/字面量/双引号内不识别、
反引号（引号内外、嵌套、单引号内字面量）、波浪号（词首、逐词、引号内、词中）、
花括号（列表/嵌套/前后缀/区间/字面量情形/结果逐项 glob）。

### 已知收窄

- 花括号内含展开（`{$x,y}`）不展开，走普通路径保持字面量。
- `~user`、`{01..03}` 补零区间、`{1..10..3}` 步长区间未实现。

---

## 18. 已落地：完整算术求值器 + `(( ))` + `let`（切片 8，覆盖 §2 P1-3）

此前的算术只支持 `+ - * / %` 和括号，其余运算符一律报
`syntax error in expression`；`(( … ))` 与 `let` 根本不存在
（`((i++))` 被当成子 shell，报 `i++: command not found`）。

### 修复前后（以 bash 3.2 实测为准）

| 脚本 | 改动前 | 改动后 |
| --- | --- | --- |
| `echo $((2**10))` | 语法错误 | `1024` |
| `echo $((5&3))` / `$((5\|3))` / `$((5^3))` / `$((~5))` | 语法错误 | `1` / `7` / `6` / `-6` |
| `echo $((1?2:3))` | 语法错误 | `2` |
| `echo $((0x1f))` / `$((010))` | 语法错误 | `31` / `8` |
| `i=5; echo $((i+=3))` | 语法错误 | `8`（并写回） |
| `((i++))` | `i++: command not found` | 递增并设置退出码 |
| `while ((i<3)); do …; done` | 不可用 | 正常循环 |
| `let x=3+4` | `let: command not found` | `x=7` |

### 实现要点

1. **独立模块** `crates/just-bash-shell/src/arith.rs`（递归下降 + 完整优先级表）：
   逗号 → 赋值（右结合：`= += -= *= /= %= <<= >>= &= |= ^=`）→ 三元 → `||` → `&&`
   → `|` → `^` → `&` → 相等 → 关系 → 移位 → 加减 → 乘除模 → 一元/幂 → 后缀 `++/--` → 原子。
2. **优先级细节**按 bash 3.2 实测：`**` 右结合（`2**3**2` = 512），且**比一元负号松**
   （`-2**2` = 4）；`-7/2` = -3（向零截断）；`&&`/`||` 短路；`2**-1` 报
   `exponent less than 0`；除零报 `division by 0`。
3. **变量与参数**：未定义变量为 0；变量值本身可以是表达式（`x=y; y=5` → `x+1` = 6，
   带深度上限防 `x=x`）；支持 `$1`、`${n}` 前缀写法（正则里的 `${n}` 与 `$1`）。
4. **读/写分离**：`Vars` trait + `LiveVars`（可赋值）/ `FrozenVars`（`&self` 场景，
   赋值计算正确但不落盘）。`${a[i]}`、切片边界等只读路径走 frozen。
5. **`(( … ))` 语句**：词法器在命令位置遇到 `((` 时按算术命令处理（与 bash 一致；
   `( ( cmd ) )` 带空格仍按子 shell 解析），新增 `StatementKind::Arithmetic`，
   退出码「值非 0 → 0，值为 0 → 1，语法错误 → 2」。
6. **`let`**：逐个参数求值，退出码取最后一个值（0 → 1）。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 719 passed（新增 12 个）
```

新增 `just-bash-shell::arithmetic_tests`（12 个）：28 行运算符对照表、变量与复合赋值、
自增自减、`$1`/`${n}`、变量值为表达式、除零、`(( ))` 的副作用与退出码、循环/条件、
空格子 shell 不受影响、`let` 取值与错误。

### 过程中修掉的两个自身缺陷

- `**` 起初实现成「指数平方」但漏了底数平方，`2**10` 得 4；改为标准快速幂。
- 支持 `$` 前缀参数时引入了**死循环**（`index` 被同名变量遮蔽未回写）；已修复，
  并补了 `$1`/`${n}` 的回归测试。

### 仍未覆盖

- `$(( … ))` 内嵌数组下标（`a[i]`）与 `++`/`--` 之外的赋值形式（如 `a=1 i=2`）未测。
- 八进制/十六进制只支持字面量前缀，未支持 `64#zzz` 这类任意进制。

---

## 19. 已落地：`eval` / `source` / `type` / `command`（切片 9）

§3 里最影响脚本组织能力的四个内建命令。此前它们全部报
`command not found`，导致「拆分的库文件 + 主脚本」这种最基本的结构无法工作。

| 脚本 | 改动前 | 改动后 |
| --- | --- | --- |
| `x=1; eval "x=2"; echo $x` | `eval: command not found` | `2` |
| `. /lib.sh`（文件里定义函数） | `. : command not found` | 函数可用 |
| `type echo` | `type: command not found` | `echo is a shell builtin` |
| `command echo hi` | `command: command not found` | `hi` |
| `command -v echo` | 同上 | `echo` |

### 实现要点

1. **`eval`**：参数以空格拼接后重新解析，在**当前 shell 状态**中执行（`exec_script_text`），
   因此变量赋值、函数定义都会留下；`f(){ eval …; }` 里调用也能正确看到函数作用域。
2. **`source` / `.`**：从虚拟文件系统读取并在当前状态执行；额外参数临时作为
   `$1..$n`（执行后还原）；文件内 `return N` 结束 source 并把 N 作为退出码；
   文件不存在时报 `bash: source: …: No such file or directory`（退出码 1）；
   缺文件名报用法错误（退出码 2）。
3. **`return` 的适用范围修正**：原来是 `function_depth == 0` 就报错，现在
   **函数内或 source 内**都允许，与 bash 一致（此前 source 里的 `return` 会直接报错）。
4. **`type`**：区分 `shell builtin` / `function` / `not found`（退出码 1）。
5. **`command`**：绕过 shell 函数直接执行（`command echo` 不受 `echo(){ … }` 覆盖影响），
   `-v`/`-V` 打印名字是否可解析。
6. **新增限额** `ExecutionLimits::max_source_depth`（默认 100）：`eval`/`source` 互相递归
   有明确上限并报 `nesting exceeds limit`，不会栈溢出。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 731 passed（新增 12 个）
```

新增 `just-bash-shell::shell_builtin_tests`（12 个）：eval 的当前状态/函数可见性/无参、
source 的库文件/位置参数/`return`/缺文件/缺参数、type 三类、command 执行与 `-v`、
以及 `command` 绕过函数覆盖。

### 已知收窄

- `type` 只打印「是内建/是函数」，不打印函数体（bash 会打印）。
- 本运行时没有外部命令，所以 `type ls` 报 builtin 而 `type nosuchcmd` 报 not found。
- `getopts`、`readonly`、`alias`/`unalias`、`hash`、`shopt`、`pushd`/`popd` 仍未实现。

---

## 20. 已落地：一致性测试基建（切片 11）

### 做了什么

把**本仓库 git 历史里**的上游对照夹具（`d64009a` 可达的
`packages/just-bash/src/comparison-tests/fixtures/*.json`，27 个文件 / **457 条用例**）
导出为 `crates/just-bash/tests/conformance/fixtures/`，并写了
`crates/just-bash/tests/conformance.rs`：

- 自带一个约 200 行的 **std-only JSON 解析器**（仓库不允许外部依赖）；
- 每条用例建一个 `Bash`，把 `files` 写进虚拟文件系统，执行 `command`，
  逐字节比对 `stdout` / `stderr` / `exitCode`；
- 输出**每个 suite 的通过率**，并把尚未匹配的用例记在
  `known_failures.txt`（一行一个 `suite/id`）；
- 关键设计：**失效条目也算失败**——某条用例修好后如果还留在 known_failures 里，
  测试会报错提醒删除。这样「已知缺口」只会变少，不会悄悄腐烂。

### 首次运行得到的真实基线

```
conformance: 225/457 match bash（49%）  ← 第一次运行
```

各 suite 通过率暴露出的缺口非常集中：`find` 0/22、`sort` 2/23、`paste` 0/18、
`cut` 0/17、`wc` 0/13、`column-join` 0/14 —— 绝大多数不是「没实现功能」，
而是**同一批内建缺少统一的选项解析**。

---

## 21. 已落地：共享选项解析器（切片 8，修复上述最大根因）

`sort -n`、`cut -d: -f2`、`head -c 5`、`uniq -c`、`wc -l` 此前都会被当成
「文件名以 `-` 开头」，报 `No such file or directory`；`cut -d: -f2` 还会因为不认
附着写法直接打 usage。根因是每个内建各写一套手写循环。

### 实现

新增 `crates/just-bash-shell/src/opts.rs`：一张 `(短选项, 长选项, 是否带值)` 表，
统一支持三种写法与簇：

```
-d X     -dX     --delimiter=X     --delimiter X     -rn（多标志簇）
```

未知选项报 GNU 形状的 `sort: invalid option -- 'z'`（退出码 2），
`--` 终止选项解析，单独的 `-` 仍是操作数（表示 stdin）。

改写的内建：

| 内建 | 新增能力 |
| --- | --- |
| `sort` | `-r` `-n` `-u` `-f` `-h` `-c` `-b` `-t` `-k`（含 `-t: -k2 -rn` 组合） |
| `uniq` | `-c` `-d` `-u` `-i`，计数右对齐 7 列 |
| `wc` | `-l` `-w` `-c` `-m` `-L`，多文件逐行 + `total` 汇总 |
| `head`/`tail` | `-n N` `-n +N`（tail）`-c N`、遗留 `-5` 写法、多文件 `==> file <==` 标题 |
| `cut` | `-d` `-f` `-c`，支持列表与区间 `1,3` / `2-4` / `5-` / `-3` |

### 结果

```
225/457（49%）  →  276/457（60%）      +51 条用例
sort      2/23  →  17/23
cut       0/17  →  16/17
head-tail 10/22 →  18/22
uniq      5/16  →  14/16
wc        0/13  →  13/13
pipes-redirections 21/23 → 22/23
```

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 746 passed（新增 8 个 + 6 个解析器单测）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 276/457 match bash（181 条记在 known_failures.txt）
```

新增测试：`opts::tests`（6 个，覆盖簇、附着值、长选项两种写法、`--`、`-`、未知选项）、
`text_tool_option_tests`（8 个，覆盖 `sort`/`uniq`/`cut`/`head`/`tail`/`wc` 的真实用法，
含 GNU 的「行数按换行字节计」与多文件标题顺序）。

过程中修正了 `wc -l` 的语义（我最初按逻辑行数计，GNU 数的是换行字节，
`printf 'a' | wc -l` 是 0 而不是 1），以及 `head`/`tail` 多文件时标题与内容的输出顺序。

### 仍缺（已知 181 条，按价值排序）

1. `grep`：`-r`、`-A/-B/-C`、`-o`、`--include`，以及**默认应按 BRE 解释正则**
   （现在 `^hello`、`[ch]at` 被当字面量）——  12 条；
2. `tr`：字符类与 `-d`/`-s`（9 条）；
3. `sed`：地址区间、`y`、`a/i/c`、`h/H/g/G`、`N/P/D`（6 条）；
4. 未实现的命令族：`find`(22)、`paste`(18)、`column/join`(14)、`strings/split`(11)、
   `tar`(10)、`alias`(3)、`basename-dirname` 部分(7)、`text-processing`(30)；
5. `echo -e`、`ls -R`、`jq range()` 等零散缺口。

---

## 22. 已落地：`grep` 正则语义与上下文/递归 + `tr` 完整化（切片 12）

一致率 **276/457（60%）→ 297/457（65%）**，两个 suite 直接打满。

### `grep`：默认按正则解释（最大语义偏差）

此前 `grep` 默认做**字面子串**匹配，这在文档里写着「deliberate deviation」，
但它让 shell 里最常见的模式全部失效：

| 脚本 | 改动前 | 改动后（= GNU grep） |
| --- | --- | --- |
| `grep "^hello" f` | 无匹配 | `hello world` |
| `grep "world$" f` | 无匹配 | `hello world` |
| `grep "c.t" f` | 无匹配 | `cat` `cut` `cot` |
| `grep "ca*t" f` | 无匹配 | `ct` `cat` `caat` |
| `grep "[ch]at" f` | 无匹配 | `cat` `hat` |
| `grep -F "c.t" f` | — | 仍只匹配字面 `c.t` |

实现上把 `GrepOptions::regex` 的默认值改为 `true`（手写 `Default`），
`-F` 仍切回字面量。**注意**：GNU 默认是 BRE，本引擎是 ERE，所以
`\+`、`\(` 这类 BRE 专有写法按 ERE 解释——已在代码注释与文档标注。

### `grep` 新增选项

- `-o`：只输出匹配部分（用正则的 `find` 逐段推进，零宽匹配不死循环）
- `-A/-B/-C N`：上下文行，非相邻分组之间输出 `--`，上下文行用 `-` 分隔符
- `-r`/`-R` + `--include=GLOB`：递归遍历目录，路径相对当前工作目录输出
  （`grep -r pat dir` → `dir/file:...`）

### `tr`：`-d` / `-s` / `-c` 与字符类

此前只支持 `SET1 SET2` 映射，`-d`/`-s` 被完全忽略（`tr -d b` 原样输出）。

| 脚本 | 改动前 | 改动后（= GNU tr） |
| --- | --- | --- |
| `tr -d 'lo'` | 原样 | 删除 l、o |
| `tr -s ' '` | 原样 | 压缩连续空格 |
| `tr -s 'ab'` | 原样 | `aabbcc` → `abcc` |
| `tr '[:lower:]' '[:upper:]'` | 原样 | 转大写 |
| `tr -cd '[:digit:]'` | 原样 | 只留数字 |

支持 `[:lower:]`/`[:upper:]`/`[:digit:]`/`[:alpha:]`/`[:alnum:]`/`[:space:]`/
`[:blank:]`/`[:punct:]`/`[:cntrl:]`/`[:print:]`/`[:graph:]`/`[:xdigit:]`。
`-s` 单独使用时压缩 SET1，与 SET2 同时给出时压缩 SET2，与 `-d` 共用时压缩 SET1。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 757 passed（新增 11 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 297/457 match bash（160 条记在 known_failures.txt）
```

新增 `grep_tr_tests`（11 个）：默认正则的 `^ $ . * []`、`-F` 切回字面量、
`-o`、`-A/-B/-C`、递归与 `--include`、`tr` 的删除/压缩/字符类/补集。

过程中还修了 `grep -r` 的路径显示（原先多出一层 `home/user/`），
并把一处记录旧「字面量默认」行为的断言更新为 GNU 语义。

### 仍缺（160 条，按价值排序）

1. 未实现命令族：`find`(22)、`paste`(18)、`column/join`(14)、`strings/split`(11)、`tar`(10)；
2. `sed` 地址区间 / `y` / `a i c` / `h H g G` / `N P D`（6 条）；
3. `basename-dirname` 的选项形态（7 条）、`text-processing`（30 条，主要是上述命令族）；
4. 零散：`echo -e`、`ls -R`、`jq range()`、`alias`（3 条）。

---

## 23. 已落地：零散内建补齐（切片 13）

一致率 **297/457（65%）→ 317/457（69%）**，`echo`、`basename-dirname`、`ls`、`alias`
四个 suite 全部打满。

| 命令 | 改动前 | 改动后 |
| --- | --- | --- |
| `echo -e 'a\tb'` | 原样输出 `-e a\tb` | 制表符 |
| `basename -s .txt p/file.txt` | 不支持 | `file` |
| `basename -a a.txt b.txt` | 只处理第一个 | 每行一个 |
| `dirname ./p/f.txt` | `/p`（被规范化） | `./p`（文本式） |
| `dirname a b` | 只处理第一个 | 每行一个 |
| `ls -R` | `invalid option -- 'R'` | 递归列表，`./dir:` 标题 |
| `alias x='…'; x` | command not found | 展开执行 |
| `unlias`-不存在的名字 | command not found | 报 `not found` 并返回 1 |

### 实现要点

1. **`echo`**：支持 `-n`/`-e`/`-E` 及簇写法（`-ne`）；`-e` 解释
   `\n \t \r \a \b \f \v \e \\ \0NNN` 与 `\c`。
2. **`basename`**：`-a`（多操作数）、`-s SUFFIX`、以及「末尾操作数即后缀」的传统写法；
   三种后缀写法归一。**`dirname`**：文本式（保留 `./`，不做路径规范化），
   多操作数逐行输出——这也是它此前与 bash 不一致的原因。
3. **`ls -R`**：把目录打印逻辑抽成 `print_ls_directory` 并递归，子目录标题相对
   操作数（`./dir/sub`），`-a` 的 `.`/`..` 不进递归。
4. **`alias`/`unalias`**：新增别名表与定义/查询/删除；并在命令分派前做**首词展开**
   （自引用别名不展开，避免死循环）。注意 bash 在非交互 `-c` 模式下默认不展开别名，
   这里选择展开——夹具不依赖该行为，属于更强的能力。

### 顺带修好的一处比对缺陷

`alias` 三条用例此前因 stderr 前缀不同而失败：夹具记录的是 `/bin/bash: line 1: …`，
本运行时输出 `bash: …`。这是**记录器路径与行号**差异，不是行为差异。harness 现在会
把 `PATH: line N: ` 与 `bash: ` 两种前缀都归一化后再比对，避免把"录制环境"
当成"语义不一致"。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 769 passed（新增 12 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 317/457 match bash（140 条记在 known_failures.txt）
```

新增 `misc_builtin_tests`（12 个）：`echo` 的转义/默认关闭/簇标志、`basename` 的
三种后缀写法与 `-a`、`dirname` 的文本式与多操作数、`alias` 的定义/列出/缺失报错、
`unalias`、`ls -R` 的嵌套输出。

### 仍缺（140 条）

1. 未实现命令族：`find`(22)、`paste`(18)、`column/join`(14)、`strings/split`(11)、`tar`(10)；
2. `sed` 地址区间 / `y` / `a i c` / `h H g G` / `N P D`（6 条）；
3. `text-processing`(30，主要是上述命令族)；
4. 零散：`jq range()`、`test` 部分选项（4 条）等。

---

## 24. 已落地：文本工具族（切片 14）

一致率 **317/457（69%）→ 373/457（82%）**，一次补齐 8 个此前完全不存在的命令。

| 命令 | 说明 |
| --- | --- |
| `rev` | 逐行反转字符 |
| `fold -w N [-s]` | 按宽度折行；`-s` 在空白处断行并保留该空白 |
| `expand [-t N]` | 制表符按 tab stop 展开为空格 |
| `unexpand [-a] [-t N]` | 空格回转为制表符；默认只处理行首，`-a` 处理全部 |
| `strings [-n N]` | 提取连续可打印字节串 |
| `paste [-d LIST] [-s] FILE…` | 按行并排拼接；`-d` 指定分隔符环，`-s` 按文件串行 |
| `column -t [-s SEP]` | 按列对齐，列宽取该列最宽单元格 + 2 空格 |
| `split [-l N\|-b N] [FILE [PREFIX]]` | 按行/字节切分并写回虚拟文件系统（`xaa`、`xab`…） |

### 实现要点

1. **独立模块** `crates/just-bash-shell/src/textutils.rs`：8 个纯函数 + 9 个单元测试，
   与执行器解耦（输入解析仍由 `lib.rs` 的内建分派负责）。
2. **`paste` 的共享 stdin**：多个 `-` 表示*同一个* stdin 被按行轮流分配
   （`printf '1\n2\n3\n4\n' | paste - -` → `1\t2` / `3\t4`），而不是每个 `-` 读整段。
3. **`unexpand` 的两种模式**：默认只转换行首空白，`-a` 转换行内空白，
   按 tab stop 精确换算并保留尾随空格。
4. **`split` 的参数形态**：`split [OPTION]… [FILE [PREFIX]]` —— 第一个操作数是输入
   文件、第二个是输出前缀（默认 `x`），写出 `xaa`/`xab`…
5. **`printf` 转义补齐**：`strings`/`split` 的夹具依赖 `printf '\x00'`，而此前
   `printf` 只支持 `\n \t \r \\`，`\xHH` 被当成字面量。现补齐 `\xHH`、`\NNN`、
   `\0NNN`、`\a \b \f \v \e \" \'`（顺带让 3 条 `strings-split` 用例通过）。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 786 passed（新增 17 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 373/457 match bash（84 条记在 known_failures.txt）
```

新增测试：`textutils::tests`（9 个单元测试）+ `textutils_builtin_tests`（8 个端到端）。

### 仍缺（84 条）

1. `join`（8 条）——需要按键合并两个已排序文件；
2. `find`（22 条）——目录遍历 + 表达式；
3. `tar`（10 条）、`sed` 扩展命令（6 条）、`test` 少数选项；
4. 零散：`cat -`（stdin 操作数）、`jq range()` 等。

---

## 25. 已落地：一致性收尾（切片 15）

一致率 **373/457（82%）→ 387/457（85%）**，并且修掉了一个从切片 5 起就存在的真实缺陷。

### 修掉的最重缺陷：带引号的空字符串被丢弃

```
set -- "" ""; echo $#     # bash: 2   本库(修复前): 0
test -n ""                # bash: 假  本库(修复前): 真
```

根因在解析器与展开层的交界：`""` 被解析成 **parts 为空的 Word**，而
`expand_word_list_splitting` 遍历空 parts 后什么都没产生，字段就被丢了。
修复：空 parts 的 Word 视为「一个空字段」。这解释了 `test -n ""`、
`printf '[%s]' ""` 等一系列偏差。

### 其余修复

| 项 | 修复内容 |
| --- | --- |
| `wc` | 计数右对齐 7 列；始终打印文件名（含单文件）；`total` 行同样对齐 |
| `test` / `[` | 支持 `-a`/`-o` 逻辑连接符（跳过首个操作数位） |
| `cat` | `-` 操作数在出现位置读取 stdin |
| `cut` | 无 `-d` 时默认分隔符改为 **TAB**（此前是逗号） |
| `sort -n` | 按**前导数字**比较（`2 oranges` < `10 apples`） |
| `sort -b` | 忽略前导空白 |
| `sort -V` | 新增版本号排序 |
| `sort -c` | 报 `sort: FILE:N: disorder: LINE` 并返回 1 |
| `rm -f` | 文件不存在时不再报错 |
| `grep -r` | 递归模式总是带文件前缀（即使只匹配一个文件） |
| `jq` | 新增 `range(n)` / `range(from;to)` / `range(from;to;step)`，从「不支持」名单移除 |

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 797 passed（新增 11 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 387/457 match bash（70 条记在 known_failures.txt）
```

新增 `conformance_fix_tests`（11 个）。同时更新了 9 处因 `wc` 对齐与
`grep -r` 前缀变化而失效的旧断言，以及 jq 的 `range` 编译错误用例。

### 仍缺（70 条）

1. `find`（22 条）——目录遍历与表达式；
2. `tar`（10 条）；
3. `sed` 地址（`$`、`/re/`、`2,3`）与部分命令（5 条）；
4. `substitution-desync`（4 条）、`text-processing` 余项（9 条）；
5. 零散：`join`（8 条）等。

---

## 26. 已落地：`wc` 输出对齐（切片 16）

一致率 **387/457（85%）→ 401/457（88%）**，`wc` 从 10/13 提升到 12/13。

### 真正的对齐规则（此前两次都没猜对）

`wc` 的输出布局分三种情形，**不是** GNU 的「统一 7 列」：

| 情形 | 规则 | 例子 |
| --- | --- | --- |
| 单列 + 单输入 | **不填充** | `wc -l t.txt` → `3 t.txt` |
| 有文件操作数 + 多列 | 每列宽度 = `max(本列最宽值, 2)` | `wc t.txt` → ` 0  2 10 t.txt` |
| 有文件操作数 + 单列 + 多文件 | 每列宽度 = `max(本列最宽值, 2)` | `wc -l a b` → ` 2 a.txt` / ` 5 total` |
| 读标准输入 + 多列 | 固定 7 列 | `echo "hello world" \| wc` → `      1       2      12` |

前两次尝试（统一 7 列、统一 2 空格分隔）都因为把不同情形混为一谈而失败。
这次的依据是**逐条对照夹具**：先推导出每一行期望的空格数，再反推列宽公式，
并用一个独立小脚本验证了 6 组样例后才改代码。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 801 passed（新增 4 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 401/457 match bash（56 条记在 known_failures.txt）
```

新增 `wc_alignment_tests`（4 个）分别覆盖上表四种情形；
同时更新了 12 处因对齐规则变化而失效的旧断言（都是预期的行为修正）。

### 仍缺（56 条）

1. `find`（22 条）——目录遍历与表达式；
2. `tar`（10 条）；
3. `text-processing`（9 条）、`column-join` 的 `join`（8 条）；
4. `sed` 地址（4 条）、`substitution-desync` 余项。

---

## 27. 已落地：`nl`/`join`/`sed` 地址/`find`/`tar`（切片 17）

一致率 **401/457（88%）→ 456/457（99.8%）**，只剩 1 条已知缺口。

| 子系统 | 内容 |
| --- | --- |
| `nl` | 行编号：`-b a`、`-n ln/rn/rz`、`-w`、`-s`、`-v`、`-i`；空行仍占位但不编号 |
| `join` | `-t`、`-1/-2`、`-a 1/2`、`-v 1/2`，按键合并两个有序文件 |
| `sed` | 地址从「行号/`$`」扩展为 **`/regex/` + `start,end` 范围**；范围状态跨行保持 |
| `find` | `-maxdepth`/`-mindepth`/`-name`/`-type`/`-o`，目录遍历与表达式求值 |
| `tar` | 新 crate `just-bash-tar`：ustar 打包/解包 + **gzip 容器**（stored deflate） |

### 顺带修掉两个真实缺陷

**1. 单引号没有阻止 glob 展开（切片 7 起就存在）**

```
echo '*.txt'     # bash: *.txt     本库(修复前): a.txt b.txt
find . -name '*.txt'   # 只匹配到第一个文件
```

根因在 `push_char`：只有双引号被标记为 `quoted`，单引号文本落进了
`Literal/quoted: false`，于是被 glob 当成模式展开。修正为 `quote.is_some()`。
这个 bug 也解释了 `find` 的一类诡异失败（第二个文件名被当作 root 操作数）。

**2. 二进制归档在字符串文件系统里被破坏**

内存文件系统存 `String`，gzip 字节经 `from_utf8_lossy` 后不可逆。
新增 `just_bash_tar::encode_bytes`/`decode_bytes`（Latin-1 逐字节映射），
`tar -z` 的写入与读取都走这条无损通道。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 816 passed
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash（1 条记在 known_failures.txt）
```

新增测试：`just-bash-tar` 6 个（ustar 往返、长名 prefix、gzip 往返、
空输入、非法输入、字节编解码无损）+ `find_tar_tests` 9 个。

### 剩余 1 条

`substitution-desync/e7accfa7d60b71b5`：`$((echo AA; false || echo BB)2>/dev/null)`。
bash 在 `$((` 后先尝试算术，失败则回退为 `$( ( … ) )` 子 shell；本库只走算术路径，
因此对含命令的 `$((` 报解析错误。这是刻意的**窄化**（fail-closed 而非误执行），
已在 `THREAT_MODEL.md` 记录。

---

## 28. 收尾：`$((…))` 的消歧结论（切片 18）

上一节把最后 1 条记录为「刻意窄化」。本轮深入验证后，**这个判断需要修正**：
不是我们窄化，而是 bash 本来就不回退。

### 实测结论

| 输入 | bash 实际行为 |
| --- | --- |
| `echo $((1+2))` | `3` |
| `echo $((echo AA))` | `syntax error in expression (error token is "AA")` |
| `echo $((echo AA; echo BB))` | `syntax error: invalid arithmetic operator` |
| `echo $(( (echo LEFT) ))` | `` missing `)' `` |
| `echo $( (echo AA; echo BB) )` | `AA BB` —— 这是命令替换，不是 `$((` |

即：bash 对 `$((` **从不**把命令形状的内容重读成子 shell；只有显式带空格的
`$( ( … ) )` 才是命令替换，而那种写法在本库里早已正常工作。

### 一次真实的安全回归（已撤销）

我曾按「bash 会回退」的假设加入回退逻辑，结果一致率从 456 掉到 455 ——
`substitution-desync/01e22b0a5a658000` 从 `MARKER_ABSENT` 变成 `MARKER_PRESENT`，
意味着恶意构造的 `$(( (echo LEFT; echo DESYNC > marker) || (echo RIGHT) ))`
**真的被执行了**。这正说明为什么该 fixture 属于安全测试套件：解析消歧一旦
被输入影响，就变成了执行面。

已完全撤销该回退，并新增 `arithmetic_disambiguation_tests`（4 个）把边界固定下来：

- `$((echo AA))` 不执行命令体，只报算术语法错误；
- `$( ( … ) )` 作为命令替换正常运行；
- desync 构造不产生副作用（`MARKER_ABSENT`）；
- 普通算术不受影响。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 820 passed
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

### 最后 1 条的定位

`substitution-desync/e7accfa7d60b71b5` 期望 `AA BB`（bash 在该行确实执行了
子 shell 形状的内容），但本库对 `$((` 内出现命令形状内容一律 fail-closed。
两者在这条输入上确有分歧；本库选择**不执行**，与同套件其余 4 条的安全要求一致。
若将来要完全对齐，需要让词法层把 `$((…))` 的动态边界与算术文法绑定，
成本高而收益仅为这 1 条，故维持现状并在此记录理由。

---

## 29. 已落地：复合命令接入管道（切片 19）

一致率仍是 456/457，但本轮修掉了一个** fixture 完全覆盖不到的严重缺陷**——
这正是「只跑一致性套件不足以证明可用」的例证。

### 缺陷：复合命令在管道中静默失效

```
{ echo a; echo b; } | wc -l     # bash: 2        本库(修复前): a b 0
(echo a; echo b) | wc -l        # bash: 2        本库(修复前): a b 0
for i in 1 2; do echo $i; done | wc -l   # 同样输出 1 2 0
printf 'a\nb\n' | while read -r l; do echo "[$l]"; done   # 报 unsupported construct `do'
```

根因在解析器：`{ … }`、`( … )`、`if`、`while`、`for`、`case` 由
`parse_single_statement` 解析后**直接 return**，其后的 `| 下游` 从未被接续，
于是被解析成两条独立语句。结果是复合命令的输出**直接打到终端**、下游收到空输入、
并且**不报任何错误** —— 静默给出错误结果，比语法错误危险得多。

### 修复

1. `CommandInvocation` 增加 `compound: Option<Box<StatementKind>>` 字段，
   使管道的单个阶段可以承载复合命令（全仓库仅 1 处构造点，改动面很小）。
2. 新增 `continue_pipeline_with`：复合命令解析完后若紧跟 `|`，
   将其包装为首阶段并循环收集后续阶段。
3. `parse_pipeline_statement` 在**阶段起始位置**识别复合命令
   （`is_compound_start`），使 `printf … | { cat; }` 这类中段/末段复合也成立。
4. 执行器在阶段循环里新增复合分支：用 `child_shell()` + 共享 `StdinBuffer`
   执行，使 `… | while read l` 每轮消费新的一行。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 825 passed（新增 5 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `compound_pipeline_tests`（5 个）：`{}` 阶段、`()` 阶段、循环/条件阶段、
`printf | while read` 全量迭代、简单阶段后接复合阶段。

### 当时的边界（已在 §35 修复）

`printf … | { cat; } | wc -l` 这类三阶段组合当时中段拿到空输入。
该问题已在切片 25 定位并修掉 —— 结论保留在此以记录演进过程。

---

## 30. 已落地：第二轮 fixture 外审计（切片 20）

一致率仍是 456/457，但本轮又修掉两个 fixture 覆盖不到的真实缺陷，
进一步印证「一致性套件通过 ≠ 可用性完备」。

### 缺陷 1：位置参数切片的偏移差一

```
set -- a b c d; echo ${@:2}       # bash: b c d   本库(修复前): c d
set -- a b c d; echo ${@:1}       # bash: a b c d 本库(修复前): b c d
set -- a b c d; echo ${@:2:2}     # bash: b c     本库(修复前): c d
```

`${@:off:len}` / `${*:off:len}` 的位置参数是 **1-based**（`${@:1}` 就是第一个参数），
而 `${var:off:len}` 是 0-based。此前两者共用了 `slice_bounds`，导致位置参数整体偏移一格。
新增 `positional_slice_bounds` 对非负偏移先减一；负偏移（`${@: -2}`）本来就对，保持不变。

### 缺陷 2：`printf` 完全没有格式支持

```
printf "%05.2f\n" 3.14159    # bash: 03.14    本库(修复前): %05.2f
printf "%-5s|\n" ab          # bash: ab   |   本库(修复前): %-5s|
printf "%x %X %o\n" 255 255 8  # bash: ff FF 10  本库(修复前): 原样输出
```

原实现只识别 `%s`/`%b`/`%d`/`%i`，任何带标志/宽度/精度的格式都原样吐出。
现按 C 语义实现：标志 `- + 空格 0 #`、宽度、`.精度`，以及
`%s %b %c %d %i %o %u %x %X %f %F %e %g %G %%`。

其中 `%e` 的指数按 C 输出为**带符号、至少两位**（`1.234500e+03`）——
Rust 的 `{:e}` 输出 `e3`，需要补零与符号。

### 关于调试过程的一点记录

这次 `printf` 的解析改了四轮才对，原因是 `%` 之后的字符可能同时是
标志、宽度或转换符，用「peek + 有条件 next」的写法极易漏字符。
最终改为「把已消费的字符放进 `progress` 槽位，所有读取都从该槽位或
迭代器统一取」，逻辑一次理顺。中途我也曾误判为「改动没生效」，
实际是单测用错了过滤条件导致结果串台——先用**直接单测**定位，
比反复跑 CLI 更有效。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 831 passed（新增 7 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `printf_format_tests`（7 个）：宽度/对齐/补零、精度、整数与字符转换、
浮点与 `%%`、多参数重复、多说明符按序取参。同时更新了 `positional_slices`
里记录旧错误行为的两条断言。

---

## 31. 已落地：第三轮 fixture 外审计（切片 21）

一致率仍是 456/457，但本轮补齐了 **7 个完全缺失的常用命令**和 2 个语义缺陷。
前两轮各发现 2 个缺陷，这一轮发现了 9 个 —— 说明「fixture 之外」仍是富矿。

### 完全缺失的命令（此前一律 `command not found`）

| 命令 | 说明 |
| --- | --- |
| `tee [-a]` | 管道分流——**此前连最基本的 `echo x \| tee f \| cat` 都输出空** |
| `base64 [-d]` | 编码/解码 |
| `trap ACTION EXIT` | 退出清理钩子（只支持 EXIT，其余信号安全忽略） |
| `getopts SPEC NAME` | 选项解析，支持簇（`-ab`）、带参（`f:`）、非法选项返回 `?` |
| `shopt -s nullglob` | 未匹配的通配展开为空（默认关闭，保持既有行为） |
| `uname` / `whoami` / `id` | 虚拟平台身份 |
| `readlink` / `realpath` | 路径规范化 |

### 语义缺陷

**1. 算术缺少 `BASE#DIGITS`**
```
echo $((16#ff)) $((2#1010))    # bash: 255 10    此前: 空（语法错误）
```

**2. 参数操作符的字面词未剥离引号**
```
set -- a; echo ${1+"set"}      # bash: set       此前: "set"
```
`${p+word}` / `${p-word}` 里的 `word` 是 shell 源码，引号属于语法。
新增 `strip_shell_quotes` 处理。

### 仍未实现（已记录，按价值排序）

`xxd`、`md5sum`、`sha256sum`、`chown`、`du`、`df`、`hostname`、`date`、`cal`。
其中 `date` 与运行时「确定性时钟」的设计冲突（`THREAT_MODEL.md` 已说明
`now`/`gmtime` 之类不提供），其余属于低频工具。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 842 passed（新增 11 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `shell_builtin_coverage_tests`（11 个）：trap 触发与移除、getopts 三种形态、
nullglob 开关、tee 写盘与追加、base64 往返、算术基数、参数词去引号。

---

## 32. 已落地：低层语义审计（切片 22）

一致率仍是 456/457，本轮转向 `set -o` 选项与变量属性，发现 **3 个真实缺陷**。

### 缺陷：`set` 的大部分选项不被接受

```
set -f;  echo *.txt      # bash: *.txt      此前: a.txt b.txt 并报 set: -f: invalid option
set -n;  echo never      # bash: 空         此前: never       并报 invalid option
set -x;  echo hi         # bash: stderr 有 + echo hi   此前: 无跟踪
set -o noglob            # 此前 invalid option name
```

`set` 此前只认 `-e`/`-u`/`-o pipefail`，其余一律 `invalid option`。
本轮补齐并接上行为：

| 选项 | 行为 |
| --- | --- |
| `-f` / `noglob` | 关闭路径名展开，通配符保持字面 |
| `-n` / `noexec` | 读完不执行；在脚本中途生效，只影响其后的语句 |
| `-x` / `xtrace` | 把展开后的命令以 `+ cmd` 写到 stderr，**裸赋值也跟踪** |

`xtrace` 的实现位置有个坑：跟踪必须写进 `expansion_stderr`，而它在
`run_invocation` 中途就被 `take()` 走了。第一次加在了 take 之后，
输出悄无声息地丢了 —— 用一条临时探针打印 `xtrace/temporary/n_assign`
才定位到。

### 缺陷：`readonly` 缺失，`declare -r` 只是摆设

```
x=1; readonly x; x=2     # bash: bash: x: readonly variable（值保持 1）
readonly y=5             # 此前 command not found
declare -r z=7; z=8      # declare -r 此前被静默忽略，z 会被改成 8
```

新增只读名集合，`readonly` 与 `declare -r` 都写入，赋值路径检查后
报 `bash: NAME: readonly variable` 并返回 1。

### 已知未做

`exec` 的持久 fd 重定向当时记录为「需要 fd 表、风险高于收益」。
该问题已在 §36 实现 —— 结论保留在此以记录演进过程。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 847 passed（新增 5 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `shell_option_tests`（5 个）：noglob 开关、noexec 跳过、xtrace 命令与赋值跟踪、
readonly 记录与拒写。

---

## 33. 已落地：第五轮审计与收尾（切片 23）

一致率仍是 456/457。本轮审计 `awk`/`jq` 的 fixture 外用法（**全部通过**），
但在 `sed` 与 CLI 层发现 3 个问题。

### `awk` / `jq`：无需改动

20 个典型写法（字段、条件、累加、`END`、`-F`、`BEGIN`、`printf`、
`jq -r`/`-c`/`.[]`/`sort`/`length`/对象合并）全部与 bash 一致。

### 缺陷 1：`sed` 缺 `y` 与 `N`

```
printf 'hello\n' | sed 'y/el/ip/'      # bash: hippo   此前: 报错
printf 'a\nb\n'  | sed 'N;s/\n/ /'     # bash: a b     此前: 报错
```

- `y<d>from<d>to<d>`：逐字符转换，两侧长度必须相等。
- `N`：把下一行并入模式空间。为此把 `run` 的输入循环从 `for` 迭代器改成游标，
  并让 `SedOutcome` 带 `advanced` 字段告知主循环跳过已消费的行。
  **边界**：`N` 遇到输入末尾时 bash 会丢弃模式空间且不输出，
  这一点单独修正过（最初会多打印一行）。

### 缺陷 2：`just-bash-rs … | head` 会 panic

```
just-bash-rs 'seq 1 200000' | head -2   # 此前: rc=101，stderr 有 panicked
```

`main.rs` 用 `print!` 写出结果，下游关闭管道后 Rust 的 `print!` 会 panic。
改为显式 `write_all` 并忽略写入错误 —— shell 在这种情形下本就应当安静退出。
现在 `rc=0` 且无 panic。`Bash::new` 与 stdin 读取也从 `expect` 改成了可读的错误消息。

### 一次「差一点就错了」的教训：`uniq -c` 的对齐

我按**真机 GNU 的行为**把宽度改成 4 列（`   2 a`），一致性套件立刻从 456 掉到 451 ——
5 条 `uniq` 用例全红。原因是**上游 fixture 记录的是 7 列**（`      2 a`）。
本项目的判据是上游夹具而非本机 bash，于是改回 7 列并保留「计数更长时按需加宽」。
教训：在这个仓库里，**夹具优先于本机 bash**。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 849 passed（新增 2 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

同时更新了 `unsupported_commands_fail_closed` / `sed_fails_closed_on_unsupported_scripts`
两处把 `y`/`N` 当作「未支持」的旧断言。

---

## 34. 收尾核查（切片 24）

本轮不再新增功能，只做最后三件事：清理工作区、跨层面抽查、确认可收尾。

### 工作区清理

差分测试留下了 5 个垃圾文件（`a.txt`、`b.txt`、`f`、`f1`、`f2`），已删除。
剩余未跟踪文件全部是本次工作的产物：

```
UPSTREAM_GAPS.md                     1693 行
crates/just-bash-shell/src/arith.rs   609 行
crates/just-bash-shell/src/opts.rs    222 行
crates/just-bash-shell/src/textutils.rs 709 行
crates/just-bash-tar/src/lib.rs       337 行
crates/just-bash/tests/conformance.rs 428 行
crates/just-bash/tests/conformance/   （harness + 27 个 fixture + 名单）
```

相对 HEAD：13 个文件修改，约 +11800 / −4900 行。

### 跨层面抽查

用 8 个真实脚本片段验收（批量文件处理、数据流水线、配置解析、错误处理），
7 个完全一致。唯一差异是 `find` 的**输出顺序**：bash 依赖底层目录实现
（实测为倒序），本库按字符串序返回。这是更确定的行为，且 22 条 `find` 夹具
全部一致，故不改。

### 最终状态

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 849 passed, 0 failures
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash（1 条已记录）
```

**结论：从上游同步点出发的对齐工作已完成。** 剩余差距只有一处刻意的
fail-closed 取舍（`$((…))` 不回退为子 shell），以及若干已记录的低频项
（`exec` 持久 fd、`xxd`/`md5sum`、`date` 与确定性时钟的冲突）。

---

## 35. 已落地：修复中段复合命令的静默错误输出（切片 25）

上一轮把 `printf … | { cat; } | cat` 记为「三阶段以上、中段复合读 stdin」的已知边界，
理由是「修复需要重构 stdin 生命周期，风险大于收益」。**这个判断是错的** ——
它是一条**静默给出错误结果**的路径，属于最该修的一类。

### 缺陷

```
printf 'a\nb\n' | { cat; } | cat      # bash: a b      此前: 空
printf 'a\nb\n' | (cat)   | cat       # bash: a b      此前: 空
printf '1\n2\n3\n' | { grep 2; } | cat  # bash: 2      此前: 空
```

两种情况都正确：末段复合（`printf | { cat; }`）与首段复合（`{ … } | wc`）。
只有**中段**复合会吞掉数据，因此很不容易在常规测试里撞到。

### 定位过程

临时探针（`JB_P` 环境变量控制）打印每个阶段的 `stdin` 与解析后的值，输出很直观：

```
[pipe] stage 1 compound=true stdin="a\nb\n"      ← 输入是对的
[pipe] compound_stdin present=true
[pipe] resolved stdin="" first=None               ← 解出来却是空
[pipe] stage 0 compound=false stdin=""            ← 内层 cat 拿到空
```

根因：`exec_pipeline` 用 `first_command_name(commands)` 判断「这条管道是否读 stdin」，
再决定从共享游标里取多少。而复合阶段的 `words` 为空，`first_command_name` 返回 `None`，
落进了 `_ => String::new()` 分支 —— **游标从未被消费，内层命令自然读到空**。
这个判断原本是为单阶段复合（`{ …; } < f`）写的，用在多阶段管道上就失效了。

### 修复

在 `None` 分支上补一条：若首阶段是复合命令（`command.compound.is_some()`），
就把游标剩余内容交给它，由内层命令自己决定怎么读。

```rust
None if commands.first().is_some_and(|c| c.compound.is_some()) => {
    buffer.take_remaining()
}
```

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 852 passed（新增 3 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `middle_compound_stage_tests`（3 个）：中段复合透传、中段复合体内命令读到输入、
其余复合位置（末段/首段/`while read`）不回归。

### 顺带清理

差分测试在仓库根留下了 5 个垃圾文件（`a.txt`、`b.txt`、`f`、`f1`、`f2`），已删除。

---

## 36. 已落地：`exec` 与持久 fd 重定向（切片 26）

上一轮把 `exec` 记为「需要一张 fd 表，风险高于收益」。实际上改动比预想小得多：
`Sink` 已有 `Clone + PartialEq`，`apply_redirects` 只差从一张表里取初值。

### 缺陷

```
exec echo hi                                  # 此前: command not found
echo before; exec echo mid; echo after        # 此前: before after（缺 mid）
exec 3>/tmp/f3; echo hi >&3; cat /tmp/f3      # 此前: command not found
exec 2>/tmp/e; echo x >&2; cat /tmp/e          # 此前: x 出现在终端而非文件
exec 4>/f; echo a >&4; echo b >&4; cat /f      # 此前: 只写出 b
```

### 实现

1. 新增 `persistent_fds: BTreeMap<u32, Sink>`。`apply_redirects` 用它初始化 fd 1/2
   （而不是硬编码 `Sink::Stdout`/`Stderr`），于是 `exec 2>f` 对后续命令持续生效。
2. `N>&M` 扩展为任意描述符：既能绑定额外 fd，也能在目标是 1/2 时成为本命令的输出。
3. `exec` 在 `run_invocation` 里提前拦截：
   - 无命令词 → 把当前 fd1/fd2 写入持久表；
   - 有命令词 → 执行它并置 `exec_replaced`，让语句循环停止（bash 的「替换进程」语义）。
4. `N>&-` 关闭描述符时一并从持久表移除。

### 一个隐蔽的坑：截断与追加

`exec 4>f` 打开时截断一次（正确），但**之后每次 `>&4` 写入又截断一次**，
于是 `echo a >&4; echo b >&4` 只剩 `b`。
修正：`Sink::File` 表示的是**已打开的描述符**，`append` 字段固定为 `true`，
截断只在打开那一刻发生。普通 `>` 重定向在每次打开时重新截断，语义不受影响
（`echo a > f; echo b > f` 仍只有 `b`）。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 856 passed（新增 4 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `exec_redirection_tests`（4 个）：`exec cmd` 替换、额外 fd 持久化、
stderr 持久重定向、普通 `>` 与 `>>` 的截断/追加不回归。

---

## 37. 已落地：校验和命令（切片 27）

此前 `md5sum` / `sha1sum` / `sha256sum` 一律 `command not found`。
新增 crate `just-bash-checksum`（纯 std，三个算法各约 60 行），
并经 shell 内建接入。

### 覆盖范围

```
printf 'abc' | md5sum        # 900150983cd24fb0d6963f7d28e17f72  -
printf 'abc' | sha1sum       # a9993e364706816aba3e25717850c26c9cd0d89d  -
printf 'abc' | sha256sum     # ba7816bf…f20015ad  -
sha256sum f1 f2              # 每个文件一行，带文件名
sha256sum -c listing.txt     # `f: OK` / `f: FAILED`，失败时退出码 1
```

无操作数时读标准输入并以 `-` 标注文件名（GNU 行为）。

### 验证方式

- **参考向量**：RFC 1321（MD5）、FIPS 180（SHA-1/SHA-256）的空串与 `"abc"` 向量，
  外加一条 1000 字节的多块消息，覆盖填充路径。
- **真实 bash 差分**：14 个用例（stdin、文件操作数、多文件、`-c` 通过/失败、
  与 `cut`/`awk`/`head` 的组合）全部一致。

### 一处已知差异

`head -c 200 /dev/zero | md5sum` 在本运行时得到空输入的摘要，因为
`/dev/zero` 这个虚拟设备不存在（`/dev/null` 有专门处理）。这属于既有的
设备文件缺口，与校验和实现无关，已在此记录。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 865 passed（新增 9 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增测试：`just-bash-checksum` 4 个（三算法参考向量 + 多块）、
`checksum_tests` 5 个（shell 层：参考向量、空输入、文件操作数、`-c` 通过/篡改检测）。

---

## 38. 已落地：虚拟设备文件（切片 28）

上一轮实测发现 `head -c 200 /dev/zero | md5sum` 得到空输入摘要 —— `/dev/zero`
根本不存在。此前 `/dev/null` 也只在**重定向**时被特殊处理，作为**操作数**读取时
（`cat /dev/null`、`grep x /dev/null`）会报 "No such file or directory"。

### 修复前后

| 命令 | 修复前 | 修复后 |
| --- | --- | --- |
| `cat /dev/null; echo rc=$?` | `rc=1` + 报错 | `rc=0` |
| `grep x /dev/null; echo rc=$?` | `rc=2` + 报错 | `rc=1`（无匹配） |
| `md5sum /dev/null` | 报错 | `d41d8cd9…  /dev/null` |
| `head -c 5 /dev/zero \| wc -c` | `0` + 报错 | `5` |
| `cat /dev/zero \| head -c 10 \| wc -c` | `0` + 报错 | `10` |

### 实现要点

1. `/dev/zero` 是**无限流**，不能直接materialise。生成量由 `line_budget`
   （下游 `head`/`grep -m` 算出的预算）与输出上限共同封顶，最多 1 MiB ——
   与 `yes` 用的是同一套背压机制。
2. 新增 `read_operand` 作为统一入口：先判设备、再落到文件系统。
   `cat`、`head`/`tail`、`grep`、`md5sum` 等所有读操作数的地方都改走它，
   避免逐个命令打补丁。
3. 写方向的 `/dev/null` 行为不变（丢弃、不创建文件）。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 870 passed（新增 5 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `device_file_tests`（5 个）：`/dev/null` 空读、`/dev/zero` 受下游限流、
设备摘要值、`grep` 无匹配而非报错、写 `/dev/null` 丢弃。

### 仍未实现

`/dev/full`（写入必失败）与 `/dev/random`（随机源会破坏确定性）。
`ls /dev` 也仍不可用：本运行时没有 `/dev` 目录，设备是读取时识别的虚拟路径。

---

## 39. 已落地：`xxd`（切片 29）

`xxd` 此前不存在。实现了默认十六进制转储与 `-p` / `-r` 三种形态：

```
printf 'hello\n' | xxd      # 00000000: 6865 6c6c 6f0a                           hello.
printf 'hello\n' | xxd -p   # 68656c6c6f0a
printf '68656c6c6f' | xxd -r -p   # hello
```

默认布局与真实 `xxd` 逐字节一致（每行 16 字节、`OFFSET: ` 前缀、
8 组两字节十六进制、双空格、ASCII 列以 `.` 替换不可打印字符）。
这一处「双空格」一开始少打了一个，是用 `od -c` 逐字节比对后才定位的。

### 顺带发现：`printf \xHH` 只对 ASCII 范围正确

调试 `xxd` 时用 `printf '\xff'` 做输入，发现本库输出的是 UTF-8 双字节
（`c3 bf`）而不是单字节 `ff`。追查后确认这是**架构层面的限制**：

- 整个运行时的输出用 `String` 传递，`String` 只能存合法 UTF-8；
- `printf '\xff'` 想产生单字节 `0xff`，而 `0xff` 不是合法 UTF-8 序列；
- 因此高字节必然被扩展成双字节，`printf '\xff' | wc -c` 得到 2 而不是 1。

影响范围经实测确认：**ASCII 与 `\x00`–`\x7f` 完全正确**，只有 `\x80` 及以上
受影响。要根治需要把整条输出链从 `String` 改为字节缓冲，属于大范围
结构性改动，不在本切片范围内，故如实记录而不做局部假修复。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 880 passed（新增 10 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增测试：`xxd_tests` 6 个（默认布局、16 字节换行、空输入、`-p`、双向往返、
不可打印字符）+ `xxd_builtin_tests` 4 个（shell 层 dump/plain/reverse/低字节）。

---

## 40. 已落地：`du`（切片 30）

`du` 与 `df` 此前都不存在。本轮只做 `du`：`df` 报告的是真实磁盘信息，
在虚拟文件系统里没有意义（真实机器上的 `df` 输出因平台而异，macOS 与 Linux
列都不同），做一个假的反而会误导。

### 覆盖形态

```
du d            # 每个目录一行，含递归总计
du -a d         # 连同文件一起列出
du -s d         # 每个操作数只给一行总计
du -h d         # 人类可读单位（512B / 1.0K / 1.0M）
du d/a.txt      # 文件操作数
```

### 调试中修正的两个问题

1. **目录没有汇总子项**（`d` 报 1 而不是 2）。原因是我在遍历时把**自身大小**
   累加给父目录，且 `totals` 已被本轮循环改写，造成重复覆盖。
   正确做法是累加**该路径的子树总计**。这个错误用一个 10 行的 Python 模型
   复现并验证了修正，比直接改 Rust 再跑更快。
2. **文件操作数完全不输出**。过滤条件写成「非目录则跳过」，
   把 `du f.txt` 这种最普通的用法也一起滤掉了。改为：操作数本身永远输出，
   `-a` 才额外列出其内部文件。

### 与真实 bash 的差异（有意保留）

块大小不同：本运行时按 512 字节块计算、空文件也计 1 块；
真实 bash 的 `du` 按文件系统的 4096 字节块计。由于虚拟文件系统没有块概念，
内部保持自洽即可，文档在此说明。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 886 passed（新增 6 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `du_tests`（6 个）：目录递归汇总、文件操作数、`-s`、`-a`、`-h`、缺失路径报错。

### `df` 与「输出改字节缓冲」的评估结论

- `df`：**不做**（见上，虚拟磁盘信息无意义）。
- 高字节保真（`printf '\xff' > f` 落盘为 `c3bf`）：本轮先做了勘察 ——
  `CommandOutput` / `ExecResult` / `FsEntry::File.contents` 全部用 `String`，
  共 86 处输出路径受影响，且 `ExecResult` 是公开 API 的一部分。
  这是**独立的架构级改动**，不应塞进本轮的缺口收敛任务里，建议单独立项。

---

## 41. 已落地：`pushd`/`popd`/`dirs` 与内建注册表修复（切片 31）

收尾前做了一次**完整性核查** —— 不依赖自己的叙述，而是把所有「仍未实现」
的声明逐条拿 CLI 实测。结果发现两类问题。

### 缺陷 1：`type` 的内建名单长期失同步

```bash
type xxd        # 修复前: xxd: not found   修复后: xxd is a shell builtin
type md5sum     # 同样误报
type du         # 同样误报
type alias      # 同样误报
```

`is_shell_builtin` 的名单停留在早期版本，之后每轮新增的内建（`tee`、`base64`、
`trap`、`getopts`、`shopt`、`exec`、`xxd`、`md5sum`、`du` 等 15 个）都没登记。
这会让用户和脚本误判命令可用性。

修复后新增 `builtin_registry_tests`：逐个 `type <name>` 断言退出码为 0 且输出含
`builtin`，同时直接断言 `is_shell_builtin` 覆盖这些名字 —— 以后新增内建若忘了登记，
测试会失败。

### 缺陷 2：目录栈三个命令缺失

`pushd` / `popd` / `dirs` 此前全部 `command not found`。

```
mkdir -p a b; pushd a; dirs        # /home/user/a /home/user
pushd ../b; dirs                   # /home/user/b /home/user/a /home/user
popd; pwd                          # /home/user/a
dirs -v                            #  0  …  1  …
```

栈的第一个元素始终是当前目录，`pushd` 会把旧目录压入、切换并回显栈
（重定向时可静默）；`popd` 在栈空时报错并返回 1。与真实 bash 在相同初始目录下
逐条比对一致。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 893 passed（新增 7 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `dir_stack_tests`（5 个）与 `builtin_registry_tests`（2 个）。

### 核查后仍确认未实现（按价值排序，已明确取舍）

| 命令 | 判断 |
| --- | --- |
| `hash` | 无外部命令，哈希表无意义；建议保持缺失 |
| `date` | 与运行时「确定性时钟」设计冲突（`THREAT_MODEL.md` 已列 `now`/`gmtime` 不提供） |
| `hostname` | 与 `uname -n` 重复，可用后者 |
| `cal` | 依赖日历与本地时区，低价值 |
| `chown` | 无多用户模型，纯属空操作 |
| `df` | 虚拟磁盘信息无意义（§40 已说明） |

以上六项属于**有意不实现**，而非遗漏。

---

## 42. 已落地：`xxd -r` 的字节保真修复（切片 32）

上一轮把 `printf '\xff'` 的高字节问题定性为「架构级改动」，并**顺带略过**了
一处可以单独修的缺陷。本轮先做勘察再下结论，发现确实存在一个低成本的切口。

### 缺陷

```
printf 'ff' | xxd -r -p | wc -c   # bash: 1   修复前: 3
printf 'ff' | xxd -r -p | xxd -p  # bash: ff  修复前: efbfbd
```

`xxd -r` 解出字节 `0xff` 后，用 `String::from_utf8_lossy` 转成文本 ——
`0xff` 不是合法 UTF-8，被替换成 U+FFFD（`efbfbd`，3 字节）。
**数据被静默破坏**，比单纯的长度不符更严重。

### 修复

改为「每个字节映射到同码位字符」的可逆编码（与 `just-bash-tar` 里的
`encode_bytes` 同一思路）：

```rust
pub fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| *byte as char).collect()
}
```

效果：`0xff` 从 3 字节降为 **1 个字符**（`\u{ff}`），可打印数据完全往返
（`414243` → `ABC` → `414243`）。

### 为什么仍差最后一步

写进文件时 `\u{ff}` 作为 `String` 会编码成 UTF-8 的 `c3bf`（2 字节），
所以 `printf 'ff' | xxd -r -p > f; wc -c < f` 得到 2 而不是 1。
这是 `String` 存储模型本身的下限，与我上一轮勘察的结论一致：
`CommandOutput` / `ExecResult` / `FsEntry::File.contents` 全部使用 `String`，
共 86 处输出路径，根治需改字节缓冲。

**本轮的价值在于把「3 字节的静默数据破坏」变成了「2 字节的可逆编码」** ——
后者至少能通过 `xxd` 正确还原（`ff` → `\u{ff}` → `c3bf` 是可预期的 UTF-8 表示，
而 U+FFFD 的替换则永久丢失了原始值）。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 895 passed（新增 2 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `xxd_tests` 两条：`0xff` 解码后是单个 `\u{ff}` 字符（而非替换字符）、
可打印字节完全往返。

---

## 43. 已落地：`tac`（切片 33）

本轮先复查了高字节问题的改造切口：`read_file` 返回 `&str`（借用），改成字节存储
后这个签名无法保留，会波及 shell 层 50 处调用点 —— 确认属于架构改造，
不在局部修补范围内。于是转向抽查常见脚本惯用法，发现 `tac` 缺失。

### 覆盖形态

```
printf 'a\nb\nc\n' | tac      # c b a
printf 'a\nb'      | tac      # 保留“无尾换行”
printf 'a,b,c'     | tac -s , # 自定义分隔符
tac file                      # 文件操作数
```

### 与真实 bash 的刻意差异

`tac` 在**末尾无分隔符**时，bash 会把最后一条与倒数第二条粘在一起：

```
printf 'a\nb' | tac     # bash: ba⏎       本库: b⏎a
printf 'a,b,c' | tac -s ,  # bash: cb,a,   本库: c,b,a
```

这是 bash 的已知怪癖（它按分隔符反向扫描并处理尾部特殊），并非直觉语义。
本库按「记录反向、分隔符保留」实现，测试中把这一差异写成显式断言而非隐藏。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 901 passed（新增 6 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `tac_tests`（6 个）：行序反转、缺失尾换行、空输入、自定义分隔符、
与 `sort`/`head` 组合、文件操作数。

### 顺带核实的两个「疑似缺陷」

抽查中有两条一度看起来像 bug，核实后是**测试写法问题**，记录以免重复排查：

- `echo "a,b" | IFS=, read -r p q` 两侧输出不同 —— 因为 bash 的管道右侧在子 shell
  里执行，变量不会回流；用 `IFS=, read -r p q <<< "a,b"` 或重定向写法时两边一致。
- `printf 'p\nq\n' > g; tac f g` 报 `f` 不存在 —— 前一条用例把 `f` 建在了另一个
  临时目录里，属于测试残留而非实现问题。

---

## 44. 已落地：`${#x:-5}` 应报 bad substitution（切片 34）

命令清单已基本补齐（抽查只剩 `hostname`，而它已判定为与 `uname -n` 重复），
于是本轮转向**语义层审计**：对退出码传播、变量作用域、算术边界、展开顺序
逐项做差分。绝大多数一致，发现一处会静默给出错误结果的问题。

### 缺陷

```
x=abc; echo ${#x:-0}    # bash: ${#x:-0}: bad substitution   修复前: 0
x=;    echo ${#x:-5}    # bash: 同上                          修复前: 0
```

`${#name}` 是长度，而 `${name:-default}` 是默认值 —— 两者不能组合。
本库此前把 `#` 之后的内容拆开，长度分支因 `inner_rest` 非空而被跳过，
随后又各自按「长度」和「默认值」处理，最终**静默输出 `0`**。
一个写错的脚本会拿到看似正常的结果，比报错更糟。

### 修复

`#` 之后若出现非名字字符（即存在操作符），置 `param_error` 报
`bad substitution` 并返回空，与 bash 一致。

### 验证

```bash
cargo fmt --all --check                  # ok
cargo clippy --workspace --all-targets   # 0 warnings
cargo test --workspace                   # 903 passed（新增 2 个）
cargo test -p just-bash --test conformance -- --nocapture
#   conformance: 456/457 match bash
```

新增 `length_substitution_tests`（2 个）：四种正常长度形式（含 `${#}`、`${#a[@]}`、
未设置变量），以及三种畸形写法必须报 `bad substitution`。

### 本轮审计中确认无问题的部分

退出码传播（`$?`、函数返回值、`! `取反、`&&`/`||`、`pipefail`）、
变量作用域（全局修改、`local`、函数位置参数）、算术（负数整除取模、
`x++`、`&&`/`||`）、展开顺序（`$x` 分割 vs `"$x"` 不分割）—— 全部与 bash 一致。
