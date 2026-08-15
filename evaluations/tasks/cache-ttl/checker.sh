#!/usr/bin/env bash
# checker.sh: cache-ttl 判分脚本。
# exit 0 = 通过；1 = 失败。
set -e
cd "$(dirname "$0")/workspace"

# 1) 行为测试必须全绿（含 agent 自补的回归测试 test_regression.py）。
python -m unittest discover -s tests -v

# 2) 接口契约检查：capacity=0 构造必须抛 ValueError。
python - <<'PYEOF' || { echo "REQUIRED: Cache(capacity=0) must raise ValueError" >&2; exit 1; }
from cachestore.cache import Cache
try:
    Cache(0)
except ValueError:
    pass
else:
    raise SystemExit(1)
PYEOF

# 3) 随机化行为验证（核心防特判）：
#    固定随机种子，用假时钟驱动“workspace 实现”与“内嵌独立参考实现”执行同一随机
#    序列，逐步比对 get 返回值与 len。任何一步不一致即判失败。
export PYTHONIOENCODING=utf-8
python - <<'PYEOF' || exit 1
import random
import sys
from collections import OrderedDict

from cachestore.cache import Cache  # 被测对象（workspace 实现）


# ---- 独立参考实现：严格按 cachestore.cache 规格用 OrderedDict 实现。----
class RefClock:
    """可手动推进的假时钟；被测实现与参考实现共享同一实例。"""
    def __init__(self):
        self.t = 0.0
    def __call__(self):
        return self.t
    def advance(self, dt):
        self.t += dt


class RefCache:
    """参考实现：带 TTL 的 LRU 缓存。与被测 Cache 的公共接口一致。"""
    def __init__(self, capacity, default_ttl_seconds=None, clock=None):
        if capacity < 1:
            raise ValueError("capacity must be >= 1")
        if default_ttl_seconds is not None and default_ttl_seconds <= 0:
            raise ValueError("default_ttl_seconds must be > 0 or None")
        self._cap = capacity
        self._ttl = default_ttl_seconds
        self._now = clock
        self._d = OrderedDict()

    def _resolve(self, ttl):
        if ttl is None:
            ttl = self._ttl
            if ttl is None:
                return None
        elif ttl <= 0:
            raise ValueError("ttl_seconds must be > 0 or None")
        return self._now() + ttl

    def _expired(self, key):
        _, ex = self._d[key]
        return ex is not None and self._now() >= ex

    def _purge(self):
        # 淘汰所有过期条目（过期条目不占容量；按 TTL 判定，与 LRU 顺序无关）。
        for k in [k for k, _ in self._d.items() if self._expired(k)]:
            del self._d[k]

    def get(self, key):
        if key not in self._d:
            return None
        if self._expired(key):
            del self._d[key]
            return None
        self._d.move_to_end(key)
        v, _ = self._d[key]
        return v

    def set(self, key, value, ttl_seconds=None):
        ex = self._resolve(ttl_seconds)
        if key in self._d:
            self._d[key] = (value, ex)
            self._d.move_to_end(key)
            return
        self._purge()
        if len(self._d) >= self._cap:
            self._d.popitem(last=False)
        self._d[key] = (value, ex)

    def delete(self, key):
        if key in self._d:
            del self._d[key]
            return True
        return False

    def __len__(self):
        self._purge()
        return len(self._d)


def run(seed, rounds=200, fail_fast=True):
    del fail_fast
    rng = random.Random(seed)
    clock = RefClock()
    capacity = rng.randint(1, 5)
    default_ttl = rng.choice([None] + [1.0, 2.0, 5.0, 10.0, 30.0])
    worker = Cache(capacity=capacity, default_ttl_seconds=default_ttl, clock=clock)
    ref = RefCache(capacity=capacity, default_ttl_seconds=default_ttl, clock=clock)

    KEYS = [f"k{i}" for i in range(8)]
    for step in range(rounds):
        r = rng.random()
        if r < 0.45:
            key = rng.choice(KEYS)
            value = f"v{step}"
            choose = rng.random()
            if default_ttl is not None and choose < 0.3:
                ttl = rng.choice([1.0, 2.0, 5.0])
                worker.set(key, value, ttl_seconds=ttl)
                ref.set(key, value, ttl_seconds=ttl)
            elif choose < 0.1:
                # 显式 ttl=None -> 永不过期
                worker.set(key, value, ttl_seconds=None)
                ref.set(key, value, ttl_seconds=None)
            else:
                worker.set(key, value)
                ref.set(key, value)
        elif r < 0.75:
            key = rng.choice(KEYS)
            wgot = worker.get(key)
            rgot = ref.get(key)
            if wgot != rgot:
                print(f"MISMATCH step={step} get {key} expected={rgot!r} actual={wgot!r}")
                return False
        elif r < 0.88:
            key = rng.choice(KEYS)
            worker.delete(key)
            ref.delete(key)
        else:
            wlen = len(worker)
            rlen = len(ref)
            if wlen != rlen:
                print(f"MISMATCH step={step} len expected={rlen} actual={wlen}")
                return False
        clock.advance(rng.uniform(0.0, 5.0))
    return True


ok = True
for seed in (20260815, 20260816, 20260817, 20260818, 20260819):
    if not run(seed):
        ok = False
if not ok:
    print("RANDOM FAIL: workspace Cache diverges from reference implementation")
    sys.exit(1)
print("RANDOM OK: Cache matches reference across 5 seeded sequences")
PYEOF

# 4) CLI 冒烟：构造 ops.txt 运行 CLI 并断言输出。
cat > ops.txt <<'EOF'
capacity 3
default_ttl 100
set a 1
set b 2
get a
get missing
len
delete a
len
EOF
CLI_OUT="$(python -m cachestore.cli < ops.txt)"
rm -f ops.txt
printf '%s\n' "$CLI_OUT" | grep -Fxq "get a = 1" || { echo "FAIL: CLI 'get a' output wrong" >&2; exit 1; }
printf '%s\n' "$CLI_OUT" | grep -Fxq "get missing = None" || { echo "FAIL: CLI missing get wrong" >&2; exit 1; }
printf '%s\n' "$CLI_OUT" | grep -Fxq "len = 2" || { echo "FAIL: CLI len output wrong" >&2; exit 1; }

echo "CHECKER OK"
exit 0
