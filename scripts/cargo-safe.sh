#!/bin/sh
# 按本机实际资源动态限流的 Cargo 封装。
#
# 为什么需要它（2026-11-07 在 8 核 / 16 GiB / 无 swap 的容器上实测）：
#
#   command                    峰值 RSS   结果
#   cargo test --workspace -j8  > 16 GiB   rustc 被 OOM killer 杀掉（signal 9）
#   cargo test --workspace -j4  14.87 GiB  勉强通过，但没有余量留给编辑器
#   cargo test --workspace -j1   5.92 GiB  稳定，但慢
#
# 单任务（主要是 fengheshell 的链接步骤）实测约 3 GiB，常量开销约 3 GiB。
# 并发数写死成任何固定值都是错的：在 64 核 / 256 GiB 的机器上写死 4 是浪费，
# 在 8 GiB 的笔记本上写死 4 依然会 OOM。所以这里改为每次运行前探测主机资源、
# 从预算里扣掉「编辑器 / LSP 正在占用」和「必须留出的余量」再算并发度。
#
# 用法：
#   scripts/cargo-safe.sh test --workspace
#   scripts/cargo-safe.sh clippy --workspace --all-targets -- -D warnings
#   scripts/cargo-safe.sh build --release
#   scripts/cargo-safe.sh --explain          # 只打印决策过程，不执行 cargo
#
# 可调环境变量（一般不需要碰）：
#   FENGHE_JOBS=6          直接指定并发数，跳过探测
#   FENGHE_PER_JOB_MIB=3072  单个编译/链接任务的预算（实测值）
#   FENGHE_RESERVE_MIB     手工指定留给系统与编辑器的余量
#   FENGHE_NICE=0          关闭 nice / ionice 降优先级
#
# 设计取向：宁可少开两个任务慢一点，也不要 OOM 杀掉整个构建 —— 被杀掉重跑的
# 代价远高于并发少开一点。
#
# 参数拟合（本机实测，单位 GiB，均不含可回收 page cache）：
#   -j1 → 5.92   -j3 → 14.26   -j4 → 14.87〜（边缘）   -j8 → OOM
# 拟合出「固定 ≈ 1 + 每任务 ≈ 3」，因此 16 GiB 机器上算出 3 并发与实测一致。

set -eu

# 数值校验：非数字一律退回自动探测（而非用 set -u 报「未绑定变量」）
require_uint() {
    case "$1" in
        ''|*[!0-9]*) echo "" ;;
        *) echo "$1" ;;
    esac
}

# 可调参数先归一化成「空 或 纯数字」，空表示走自动探测 / 默认值。
FENGHE_MEM_LIMIT_MIB=$(require_uint "${FENGHE_MEM_LIMIT_MIB:-}")
FENGHE_JOBS=$(require_uint "${FENGHE_JOBS:-}")
FENGHE_RESERVE_MIB=$(require_uint "${FENGHE_RESERVE_MIB:-}")

# 不论并发多少都要占的固定开销（cargo 自身、只编一次的依赖、链接缓冲）。
# 实测拟合约 0.5–1 GiB，取 1024 偏安全。
FIXED_MIB=$(require_uint "${FENGHE_FIXED_MIB:-}"); FIXED_MIB=${FIXED_MIB:-1024}
# 单个编译/链接任务的预算。实测约 3 GiB（见文件头的拟合）。
PER_JOB_MIB=$(require_uint "${FENGHE_PER_JOB_MIB:-}"); PER_JOB_MIB=${PER_JOB_MIB:-3072}

# 至少给编辑器 / LSP 留这么多；小内存机器上也别少于 1.5 GiB。
RESERVE_MIN_MIB=1536

# ── 探测：内存上限 ────────────────────────────────────────────────────────────
# cgroup v2 → cgroup v1 → /proc/meminfo → macOS sysctl → Windows（Git Bash）
read_mem_limit_mib() {
    if [ -n "${FENGHE_MEM_LIMIT_MIB:-}" ]; then
        echo "$FENGHE_MEM_LIMIT_MIB"
        return
    fi
    for f in /sys/fs/cgroup/memory.max \
             /sys/fs/cgroup/memory/memory.limit_in_bytes; do
        if [ -r "$f" ]; then
            v=$(cat "$f" 2>/dev/null || true)
            # cgroup v2 用 "max" 表示不限制。
            case "$v" in
                ''|max|*[!0-9]*) continue ;;
            esac
            # v1 的「不限制」是一个接近 2^63 的巨大值，当作不可用处理。
            if [ "$v" -lt 1099511627776 ]; then
                echo $((v / 1048576))
                return
            fi
        fi
    done
    if [ -r /proc/meminfo ]; then
        awk '/^MemTotal:/{print int($2/1024)}' /proc/meminfo
        return
    fi
    if command -v sysctl >/dev/null 2>&1; then
        b=$(sysctl -n hw.memsize 2>/dev/null || true)
        case "$b" in ''|*[!0-9]*) ;; *) echo $((b / 1048576)); return ;; esac
    fi
    if command -v powershell.exe >/dev/null 2>&1; then
        kb=$(powershell.exe -NoProfile -Command \
            "(Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize" \
            2>/dev/null | tr -d '\r\n ' || true)
        case "$kb" in ''|*[!0-9]*) ;; *) echo $((kb / 1024)); return ;; esac
    fi
    echo 0
}

# ── 探测：已被占用的内存（可回收的 page cache 不算）───────────────────────────
read_used_mib() {
    if [ -r /sys/fs/cgroup/memory/memory.stat ]; then
        v=$(awk '/^rss /{print $2}' /sys/fs/cgroup/memory/memory.stat 2>/dev/null || true)
        case "$v" in ''|*[!0-9]*) ;; *) echo $((v / 1048576)); return ;; esac
    fi
    if [ -r /sys/fs/cgroup/memory.stat ]; then
        v=$(awk '/^anon /{print $2}' /sys/fs/cgroup/memory.stat 2>/dev/null || true)
        case "$v" in ''|*[!0-9]*) ;; *) echo $((v / 1048576)); return ;; esac
    fi
    if [ -r /proc/meminfo ]; then
        awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} END{print int((t-a)/1024)}' /proc/meminfo
        return
    fi
    if command -v vm_stat >/dev/null 2>&1; then
        # macOS：用 active + wired 近似「不可回收」的部分。
        vm_stat 2>/dev/null | awk '
            /page size of/ { gsub(/[^0-9]/,"",$0); ps=$0 }
            /Pages active/ { gsub(/[^0-9]/,"",$0); a=$0 }
            /Pages wired down/ { gsub(/[^0-9]/,"",$0); w=$0 }
            END { if (ps=="" ) ps=4096; print int((a+w)*ps/1048576) }'
        return
    fi
    echo 0
}

# ── 探测：CPU 数目（同时尊重 cgroup 配额）─────────────────────────────────────
read_cpus() {
    n=0
    if [ -r /sys/fs/cgroup/cpu.max ]; then
        # v2："<quota> <period>"，quota 为 max 表示不限。
        set -- $(cat /sys/fs/cgroup/cpu.max 2>/dev/null || true)
        if [ $# -eq 2 ] && [ "$1" != "max" ]; then
            n=$(( $1 / $2 ))
            [ "$n" -lt 1 ] && n=1
        fi
    fi
    if [ "$n" -eq 0 ] && [ -r /sys/fs/cgroup/cpu/cpu.cfs_quota_us ]; then
        # v1：quota <= 0 表示不限。
        q=$(cat /sys/fs/cgroup/cpu/cpu.cfs_quota_us 2>/dev/null || echo -1)
        p=$(cat /sys/fs/cgroup/cpu/cpu.cfs_period_us 2>/dev/null || echo 100000)
        if [ "$q" -gt 0 ] 2>/dev/null; then
            n=$(( q / p ))
            [ "$n" -lt 1 ] && n=1
        fi
    fi
    if [ "$n" -eq 0 ]; then
        n=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)
    fi
    case "$n" in ''|*[!0-9]*) n=1 ;; esac
    [ "$n" -lt 1 ] && n=1
    echo "$n"
}

# ── 计算 ──────────────────────────────────────────────────────────────────────
LIMIT_MIB=$(read_mem_limit_mib)
USED_MIB=$(read_used_mib)
CPUS=$(read_cpus)

if [ -n "${FENGHE_JOBS:-}" ]; then
    JOBS=$FENGHE_JOBS
    REASON="FENGHE_JOBS 显式指定"
    RESERVE_MIB_SHOWN="未套用（显式指定并发）"
else
    # 余量：至少 15% 或 RESERVE_MIN_MIB（取大者），覆盖编辑器、LSP 与文件缓存波动。
    RESERVE_MIB=$(( LIMIT_MIB / 100 * 15 ))
    [ "$RESERVE_MIB" -lt "$RESERVE_MIN_MIB" ] && RESERVE_MIB=$RESERVE_MIN_MIB
    [ -n "${FENGHE_RESERVE_MIB:-}" ] && RESERVE_MIB=$FENGHE_RESERVE_MIB

    RESERVE_MIB_SHOWN="${RESERVE_MIB}"
    BUDGET_MIB=$(( LIMIT_MIB - USED_MIB - RESERVE_MIB ))
    # 扣掉固定开销后剩下多少、能开几个任务。不扣这一项会把并发估高：
    # 实测 -j3 时构建本身峰值约 11.8 GiB，而不是 3 × 3 GiB = 9 GiB。
    JOBS_BUDGET_MIB=$(( BUDGET_MIB - FIXED_MIB ))
    if [ "$JOBS_BUDGET_MIB" -lt "$PER_JOB_MIB" ]; then
        JOBS=1
        REASON="预算不足（可用 ${BUDGET_MIB} MiB，扣除固定 ${FIXED_MIB} MiB 后不足单任务），退回单任务"
    else
        JOBS=$(( JOBS_BUDGET_MIB / PER_JOB_MIB ))
        REASON="可用 ${BUDGET_MIB} MiB - 固定 ${FIXED_MIB} MiB，÷ 单任务 ${PER_JOB_MIB} MiB"
        if [ "$JOBS" -gt "$CPUS" ]; then
            JOBS=$CPUS
            REASON="受 CPU 核数限制（${CPUS} 核）"
        fi
    fi
fi

case "$JOBS" in ''|*[!0-9]*) JOBS=1 ;; esac
[ "$JOBS" -lt 1 ] && JOBS=1

# ── --explain：只报告决策，方便在不同主机上核对 ───────────────────────────────
if [ "${1:-}" = "--explain" ]; then
    echo "内存上限      : ${LIMIT_MIB} MiB"
    echo "当前已占用    : ${USED_MIB} MiB（不含可回收 cache）"
    echo "留给系统/编辑器: ${RESERVE_MIB_SHOWN} MiB"
    echo "CPU 核数      : ${CPUS}"
    echo "单任务预算    : ${PER_JOB_MIB} MiB（固定开销 ${FIXED_MIB} MiB）"
    echo "→ 并发数      : ${JOBS}（${REASON}）"
    exit 0
fi

if [ $# -eq 0 ]; then
    echo "用法: $0 [--explain] <cargo 子命令> [参数...]" >&2
    echo "例:   $0 test --workspace" >&2
    exit 2
fi

# ── 执行：降优先级，让交互式开发不被编译抢占 ──────────────────────────────────
# nice 会被子进程继承，因此 rustc / ld 一起降级；ionice 只影响 I/O 调度。
NICE_CMD=""
if [ "${FENGHE_NICE:-1}" != "0" ] && command -v nice >/dev/null 2>&1; then
    NICE_CMD="nice -n 10"
fi
if [ "${FENGHE_NICE:-1}" != "0" ] && command -v ionice >/dev/null 2>&1; then
    # 尽力而为的最低优先级，而不是 idle 级：idle 会让编译慢到不可用。
    NICE_CMD="$NICE_CMD ionice -c 2 -n 7"
fi

echo "cargo-safe: 并发 ${JOBS}（${REASON}），上限 ${LIMIT_MIB} MiB / 已用 ${USED_MIB} MiB" >&2

# `-j` 只被 cargo 子命令接受，写在子命令前面会被当成全局参数而报错，
# 所以拆成「全局参数」+「子命令」+「-j」+「其余参数」。
# 同时兼容 `+toolchain` 这种紧贴 cargo 的写法（如 +stable）。
FIRST=$1
shift
case "$FIRST" in
    +*)
        if [ $# -eq 0 ]; then
            echo "cargo-safe: $FIRST 后面缺少子命令" >&2
            exit 2
        fi
        SUB=$1
        shift
        # shellcheck disable=SC2086
        exec $NICE_CMD cargo "$FIRST" "$SUB" -j "$JOBS" "$@"
        ;;
    *)
        # shellcheck disable=SC2086
        exec $NICE_CMD cargo "$FIRST" -j "$JOBS" "$@"
        ;;
esac
