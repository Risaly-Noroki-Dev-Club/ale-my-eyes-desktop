# AME 任务闭环：运行器契约、生产接入与验收边界

任务闭环（verified tasks）已经接入桌面端的手机请求路径：Windows 上由 UI Automation 提供真实观察与执行环境，云端模型可提出带可验证后置条件的语义步骤，用户确认后由运行器执行并逐步验证；非 Windows、UIA 不可用、截图不可披露或模型没有返回 verified 工具时，自动回退到原有“计划→确认→执行”路径，并在诊断里记录 `verified_task_unavailable`。

它仍然不是“任意自然语言目标验证器”，也没有完成真实 Windows/读屏软件/真实模型的验收。CLI 的离线模拟与 24 例评测只证明运行器契约，不证明真实应用成功率。手机协议已升级为 **Misaka-Network v4**。

## 已实现的链

```text
宿主确定任务目标、窗口、授权范围和预算（goal 来自用户转写文本）
  → UIA 观察前台窗口与控件树 → 检查任务目标
  → 云端模型只返回一个 propose_verified_task（语义步骤 + 可验证后置条件）
  → 宿主校验并构建 Task → 手机预览（task_mode=verified + verification）→ 用户确认
  → 新观察 → 按确认顺序取一步 → 目标精确匹配 → 授权
  → 再次观察（变化则重新决策、重新授权）→ 平台按窗口/树路径/role/label 重校验后执行一次
  → 读取新状态 → 验证步骤 + 验证原始任务目标
  → 完成 / 继续决策 / 有限恢复 / 停止并交还用户
```

- 运行器：`ale-core/src/harness/`，不依赖 GUI、Windows、手机或模型守护进程。
- 生产接线：`ale-gui/src/remote_server.rs` 选择 verified 或 legacy 路径，`verified_task.rs`（schema/校验/Task/预览/状态映射）、`verified_planner.rs`（ScriptedPlanner）、`verified_authorizer.rs`（ConfirmedTaskAuthorizer）、`uia_environment.rs`（Environment + 可注入 `UiaBridge`，Windows 用 PowerShell UIA 桥；非 Windows 返回不可用）。
- 开关：设置页“已验证任务（实验）”对应 `AppConfig.verified_tasks.enabled`，默认 `true`，仅在平台 UIA 可用、可向云端披露屏幕且请求走云端规划时生效。
- 预览：verified 模式回给手机的 `CommandPreview` 带 `task_mode="verified"` 与 `verification`（每步可验证后置条件的人类可读列表），手机确认卡据此展示；桌面 Work 页沿用 `confirmation_text` 与 `action_steps`。所有用户可见文本在发送前脱敏。
- 离线参考实现：`harness-demo` + 确定性模拟器；这不是生产路径。

### 回退条件与诊断

verified 模式是“配置开关 × 平台能力 × 请求上下文”的组合。以下情况透明回退到原有计划执行路径，不向用户报错；除配置关闭外均记录 `verified_task_unavailable` 诊断（reason 为稳定数字，不包含模型文本）：

| reason | 含义 |
| --- | --- |
| `2` | 平台没有 UI Automation（非 Windows，或运行时不可用） |
| `3` | 本次请求未授权向云端披露屏幕（截图能力不可用） |
| `9` | 没有可用的云端规划调度器（`ale-modeld` 未运行） |
| `4` | UIA 首次观察失败 |
| `5` | 云端模型没有返回 `propose_verified_task` 工具调用 |
| `6` | 工具参数解析/校验失败（越权操作、非法 property、长度/步数超限等） |
| `7` | 宿主构建 `Task` 失败 |
| `8` | 云端返回多个 `propose_verified_task` 调用（必须唯一） |

设置页关闭该功能、或请求本身走本地推理路径时，直接使用 legacy 路径，不记录 unavailable 诊断。回退只影响是否逐步验证，不改变原有问答/确认/执行能力。

### 任务与成功条件

`Task` 由可信宿主在用户确认目标后创建，包含固定窗口身份、操作种类上限、云端披露授权、成功条件和预算。模型收到这些条件，但只能返回一个 `Decision`，不能修改目标或授权。生产构建里 `Task.goal` 使用用户转写文本而不是模型输出的 goal；`success` 取最后一步的后置条件。

成功条件是结构化属性的精确比较：唯一匹配的控件 role、label、属性和值。多个条件必须全部成立。控件缺失、名称歧义、属性缺失返回 `Unknown`；明确读到不同值返回 `Unsatisfied`。`Finish` 只是模型的声明，没有观察证据就转为 `NeedsUser`。

这是对可验证任务的首版支持，不是任意自然语言目标验证器。首版的后置条件由模型提出，但宿主强制执行操作/属性白名单、长度与步数上限、固定窗口与操作范围，并把每一步的验证条件随预览展示给用户确认；它不会把自由文本目标当成可验证的成功条件。真实目标的解释、用户澄清、视觉验证和无法通过 UIA 读取的状态，需要后续宿主适配。

### 观察、动作与授权

- `Environment::observe` 提供新采集的状态，序号必须递增。窗口使用进程 ID 和窗口 ID（Windows 为 HWND 字符串），不能只用标题。控件 ID 在同一观察内唯一（UIA 实现用树路径）；适配器不能仅依赖可能为空或重复的 UIA AutomationId。
- `Planner` 接收当前状态、原始任务与此前每次尝试的回执和验证结果。生产 verified 路径使用 `ScriptedPlanner` 按用户确认过的步骤顺序取下一步：目标 0 个或多个匹配、步骤偏离确认内容时返回 `NeedHelp`；离线/云端适配器（`CloudPlanner`）强制一个符合 schema 的 `next_task_step` 工具调用。不接受任意命令、多动作或模型授权。
- 云端只接收结构化状态（role/label/property 等），不自动发送截图；生产 verified 路径要求本次请求已获得截图/云端披露授权（`allow_full_screenshot`）。宿主负责去除密码、令牌等不应披露的信息。运行器会在调用云端规划器前检查授权；内置云端适配器也独立检查。
- 工具只有 `Invoke` 和精确替换的 `SetValue`，没有追加输入、坐标点击、脚本或文件工具。`SetValue` 的可重试性依赖适配器真正实现“设置为该值”，不能映射成不清空内容的键盘输入。
- `Authorizer` 是必须提供的宿主接口。生产 `ConfirmedTaskAuthorizer` 只在窗口、操作、目标与已确认步骤完全匹配时 `Allow`，偏离则 `NeedsUser`；没有默认同意实现。任务中的操作种类只是上限；宿主仍需检查目标、内容、后果，以及当前用户授权是否覆盖该动作。模拟器的 `SettingsGrant` 仅用于离线演示，禁止用于真实桌面。
- 授权之后再次采集。节点发生变化时，旧批准不用于新动作，重新决策和授权。随后 `Environment::execute` 还必须紧贴实际输入检查窗口、目标身份、可用性和状态，覆盖观察与执行之间的竞争。

### 回执、恢复与停止

#### 不预知下一页的观察与重规划

宿主可以在 `Task` 中显式启用 `allow_observation_steps`（缺省为 `false`）。这是一项执行策略授权，不是应用的页面转换说明，也不是模型可以修改的权限。启用后，`CloudPlanner` 对未知下一页的 `Invoke` 可以返回 `expected: []`，避免臆造尚未看到的控件名称；`SetValue` 仍必须提供精确读回条件。已有固定步骤的生产 `ScriptedPlanner` / `ConfirmedTaskAuthorizer` 保持严格模式，不自动启用此选项。

这种步骤仍须逐次通过宿主授权、固定窗口检查和执行前重新观察。仅当回执为 `Dispatched`，且新观察的语义状态确实变化，运行器才继续决策；下一次决策接收实际的新页面及尝试历史。比较忽略采集序号、节点临时 ID 和枚举顺序，避免把重新采集本身算成进展。`Attempt.observation_changed` / `ObservationChecked` 记录界面变化，步骤 `verdict` 仍为 `Unknown`，不会伪造已验证的后置条件。界面变化不证明特定动作效果，更不能代替 `Task.success` 的最终目标核验。

点击后始终没有变化、执行回执不确定、输入后读不到界面、窗口变化时仍停止。已发出的观察型点击，如果再次回到相同语义状态并选择相同 role/label 的目标，会交还用户；改写节点 ID 或补写一个预测条件不能绕过这个检查。明确 `NotApplied` 的动作仍可在预算内重新决策。动作、决策、期限与连续无进展上限继续生效；动态界面变化不提供任意脚本或额外操作权限。

此能力不等于任意应用的自主任务验收：宿主仍需定义目标与动作授权范围。Windows GUI 的固定步骤流程没有切换为自由探索，真实 Windows/Android 验收仍需另做。server 下游仅能从已提交上游版本同步；未提交改动的服务器验证应使用隔离测试副本，不改写下游固定版本的 vendored 树。

| 情况 | 行为 |
| --- | --- |
| 输入已发出 `Dispatched` | 重新观察；不直接宣布完成 |
| 明确没有发出输入 `NotApplied` | 有限次重新观察和决策 |
| 输入可能发生 `Uncertain` | 观察原始目标；目标证据充分可完成，否则 `OutcomeUnknown`，不重试 |
| 非幂等 Invoke 发出但未验证 | `OutcomeUnknown`，不重复点击 |
| 精确 SetValue 没有达到预期 | 在预算内重新决策；新的动作仍需授权 |
| 页面尚未变化 | 最多三次状态读取，中间等待；不额外发出动作 |
| 模型声称完成但目标未满足 | `NeedsUser` |
| 用户拒绝 / 输入前取消 | `Cancelled` |
| 执行期间取消、超时，或输入后无法读取状态 | `OutcomeUnknown`，保留输入可能已发生的信息 |
| 前台窗口变更 | 输入前交还用户；输入后按结果不确定处理 |

默认最多 12 次工具派发、20 次模型决策、2 次连续无进展、每步 3 次验证读取；单次调用 10 秒，总任务 120 秒。调用期限覆盖观察、模型、授权、执行和等待。预算是运行器规则，模型无法扩大。生产 verified 任务由手机确认后启动，独立于旧的确认/执行路径；从启动到终态有 28 秒期限（`VERIFIED_TERMINAL_DEADLINE`），超时按 `outcome_unknown` 返回。

取消使用 `watch` 通道。接收到取消（手机取消/断连）或宿主丢弃发送端会停止任务；执行可能已发生时不会宣称已撤销。平台适配器必须取消后停止后续输入，不得丢下会继续输入的后台线程；Windows 桥通过 `ExecutionControl` 协作取消，遗留的阻塞桥接不能只靠丢弃 future。

运行器状态映射到 Misaka-Network v4 的 `ExecutionState`：`Completed→completed`、`Failed/BudgetExceeded→failed`、`Cancelled→cancelled`、`OutcomeUnknown→outcome_unknown`、`NeedsUser→needs_user`。进度通过 `ProgressUpdate.stage` 的 `verified_step`/`verified_verify`/`verified_recover`（以及既有 stage）发送。

## 可复现的验证

在桌面仓库运行：

```sh
cargo test -p ale-core harness --locked
cargo run -p ale-cli --locked -- harness-demo --task zoom --scenario normal
cargo run -p ale-cli --locked -- harness-demo --task theme --scenario recover
cargo run -p ale-cli --locked -- harness-demo --task mute --scenario lost-receipt
cargo build -p ale-cli --locked
python3 scripts/evaluate-harness.py
bash scripts/verify-pc-issues.sh
# Windows 代码静态检查（不替代真机）：
cargo check -p ale-gui --target x86_64-pc-windows-gnu
```

CLI 输出 JSON，包含任务契约、观察、决策、授权、工具回执、步骤/目标验证、耗时和终止原因。完成退出码为 0，其他任务结果为 2，参数/文件错误为 1。

评测运行三个任务 × 八种环境，共 24 个案例；保存每次轨迹及 `target/harness-evaluation/summary.json`。报告区分“符合预期的停止”与“任务完成”，不能把 24 个预期结果都当作 24 个用户任务成功。模拟任务分别为保存缩放、主题和静音设置，均经过菜单、设置页、修改草稿、保存及已生效值读回。CI 的 `harness-evaluation` job 会构建 `ale-cli` 并运行同一脚本；`target/` 下的轨迹与日志是生成产物，不提交。

额外 Rust 测试覆盖目标不允许被模型改写、无授权不执行、状态过期、观察歧义、审批期间状态改变、决策和动作预算、模型/执行超时、取消及观察失败；verified 路径另用 mock `UiaBridge`/`Environment` 覆盖 schema 拒绝、Task 构建、planner、authorizer、事件映射和四种终态。云端适配器通过本机 HTTP 服务执行完整四步任务，HTTP 响应由规则模拟器生成，验证协议连接与控制流，不代表真实模型的决策质量。

报告的 `actions` 是工具派发尝试数，包括平台明确拒绝的请求，不等同于实际完成动作数。事件中的 `Dispatched` 也不等同于任务成功。

运行中的进度保存在内存；CLI 可用 `--output` 显式保存最终报告。报告可能含界面文字和输入值，生产宿主必须设计脱敏、访问限制与留存期限。首版不自动恢复进程崩溃/断线后的任务，也不自动回放轨迹中的输入。崩溃后执行结果必须视为未知并重新观察。

## 真实接入与验收边界

已实现但**未验收**的部分：Windows UIA 观察/执行桥（前台窗口身份、树路径控件、Invoke/Value、输入前重校验、协作取消）与手机端 v4 预览/进度/终态展示。真实 Windows 桌面、读屏软件、焦点变化与真实云端模型组合尚未验收。

真机验收至少包括：真实窗口移动/切换、重名控件、UIA 属性可读性、输入焦点变化、动作延迟、网络超时、授权后状态改变、操作中取消，以及真实目标用户能否听懂并接手。再使用同一任务集评估真实云端模型的完成率、误报成功、人工接手、耗时和成本。离线通过只证明运行逻辑的契约，不能替代这些验收。

非 Windows 平台没有 UIA 适配器，verified 模式自动回退旧路径；回退不影响原有问答/计划执行，但不会获得逐步验证保证。协议不支持旧客户端，v4 之前的客户端会被拒绝（`PROTOCOL_INCOMPATIBLE`），不做降级。
