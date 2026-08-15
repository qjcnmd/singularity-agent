# 任务：修复带 TTL 的 LRU 缓存库

`cachestore` 是一个带 TTL（Time-To-Live，生存时间）的 LRU 缓存库，外加一个命令行列程。项目自带单元测试（`tests/`），当前**部分测试失败**：基础读写（`get`/`set`）能通过，但 TTL 过期、容量边界与 LRU 淘汰相关的行为与规格不符。

## 背景

你的目标是让 `cachestore.cache.Cache` 完全符合下述规格，并让项目全部测试通过。

### Cache 类规格（`cachestore/cache.py`）

- 构造 `Cache(capacity: int, default_ttl_seconds: float | None = None, clock=time.monotonic)`：
  - `capacity` 必须 ≥ 1，为 0 或负数应抛 `ValueError`；
  - `default_ttl_seconds`：`None` 表示永不过期；`> 0` 为默认生存时间；`≤ 0` 应抛 `ValueError`；
  - `clock` 为返回当前时刻（秒）的可调用对象，内部统一用它做过期判定；测试会注入假时钟以便确定性验证，请务必通过此参数取时间，不要直接调用 `time.monotonic`。
- `set(key: str, value: str, ttl_seconds: float | None = None)`：
  - `ttl_seconds=None` 时使用构造时的默认 TTL；
  - 显式传入 `ttl_seconds ≤ 0` 应抛 `ValueError`；
  - **更新已存在的键**：刷新其过期时间并**移到最近使用位置**，不重复占用容量。
- `get(key: str) -> str | None`：
  - 命中且未过期：返回 value，并把该键移到最近使用位置；
  - **已过期：返回 `None` 并立即删除该条**；
  - 未命中：返回 `None`。
- `__len__()`：返回当前**未过期**的条目数（已过期条目不计入）。
- **LRU 淘汰**：`set` 插入一个**新键**且容量已满时，需淘汰条目以腾出空间。淘汰策略：先淘汰所有已过期条目（过期条目不占容量），若仍满，再淘汰**最久未使用**的条目。
- `delete(key) -> bool`：存在则删除并返回 `True`，否则返回 `False`。

### CLI 规格（`cachestore/cli.py`）

`python -m cachestore.cli < ops.txt`：从 stdin 逐行读取操作（空格分隔）：

- `set <key> <value> [ttl]`
- `get <key>` → 输出 `get <key> = <value>` 或 `get <key> = None`
- `delete <key>`
- `len` → 输出 `len = <n>`

前若干行是配置：必须含 `capacity <n>`，可含可选的 `default_ttl <secs>`（两者顺序任意，且须出现在任何操作行之前）。非法行忽略。遇到 `ValueError`（如非法 TTL）输出 `ERROR: <message>` 并**继续**处理后续行，进程不崩溃。退出码为 0。

## 现象

- 某个键超过了 TTL，`get` 仍返回旧的缓存值（应该返回 `None`）。
- 容量满时新增键行为异常：`len` 可能超过 `capacity`，或淘汰掉的不是最久未使用的条目。
- `len` 的结果与预期不符（把已过期条目也算进来了）。

## 要求

1. 先运行测试复现失败：`python -m unittest discover -s tests -v`。
2. 阅读 `cachestore/cache.py` 与 `cachestore/cli.py`，定位并修复全部缺陷，使行为完全符合上述规格。
3. **不得改变 `Cache` 类的公共接口**：类名、方法签名、构造参数（含 `clock` 注入）都必须保持不变。
4. 为**至少两处所修复的缺陷**各补一个回归测试，放在 `tests/test_regression.py`（使用 unittest），确保这些缺陷不再复发。请在 `tests/` 目录下已有的测试保持全绿。
5. 全部测试通过（`python -m unittest discover -s tests -v`，0 failures / 0 errors）。

## 验收标准

- `python -m unittest discover -s tests` 全部通过。
- `python -m cachestore.cli` 行为符合上述 CLI 规格，且输出格式不变。
- 修复只针对根因，不破坏公共接口，不做无关改动。

## 操作提示

在项目目录内工作时请使用**相对路径**操作文件（如 `cachestore/cache.py`），不要使用 `pwd` 输出的绝对路径拼接文件操作参数。

**注意**：判分会使用注入假时钟的随机化行为验证（与另一份独立参考实现比对），请确保缓存的时间来源与淘汰/过期/容量语义严格符合上文规格。
