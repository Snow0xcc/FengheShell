#!/bin/sh
# 遥测门禁（对应 docs/AI_PANEL_REQUIREMENTS.md §6.1 的 P0-N1）。
#
# FengheShell 不做遥测：没有使用统计、埋点、崩溃上报或行为采集。这条约束只写在文档里
# 迟早会被绕过，所以这里给它一个可机械校验的形式 —— 扫描参与构建的源码与 UI，命中强
# 特征词即失败。
#
# 扫描范围只有 src / crates / tests / ui：vendor 与 third_party 是上游快照，不参与构建、
# 也不由我们控制（来源与许可证见 THIRD_PARTY_NOTICES.md），因此不在范围内。
#
# 注意：这里只查「强特征词」。metrics / tracking 这类词在本仓库有正当用途
# （系统信息窗口的本地展示 SysMetricRow、终端鼠标 tracking、窗口尺寸 tracking），
# 混入规则只会制造噪音，故意不查。

set -eu

PATTERN='telemetry|analytics|sentry|posthog|matomo|mixpanel|amplitude|google-analytics|track_event|track_event_batch|usage_report|crash_report|crash-report|埋点|遥测|行为上报|使用统计'

hits=$(grep -rniE "$PATTERN" \
    --include='*.rs' --include='*.slint' --include='*.toml' \
    src crates tests ui 2>/dev/null || true)

if [ -n "$hits" ]; then
    echo "遥测门禁失败：命中禁用关键字（FengheShell 不做遥测、埋点、使用统计与崩溃上报）"
    echo "Telemetry gate failed: forbidden keyword found."
    echo "$hits"
    echo
    echo "若为误报（例如变量名里含 analytics 但与遥测无关），请改词或在规则里给出理由，"
    echo "不要直接跳过这条门禁。理由记录在 docs/AI_PANEL_REQUIREMENTS.md §5 的隐私约束。"
    exit 1
fi

echo "遥测门禁通过：src / crates / tests / ui 内无遥测、埋点或统计上报特征词。"
echo "Telemetry gate passed: no telemetry / tracking / analytics keyword found."
