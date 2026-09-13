import { useMemo, useState, useCallback, useEffect } from 'react';
import {
  LineChart,
  Line,
  BarChart,
  Bar,
  LabelList,
  XAxis,
  YAxis,
  ResponsiveContainer,
  Tooltip,
  CartesianGrid,
} from 'recharts';
import { Coins, RefreshCw } from 'lucide-react';
import PageHeader from '../components/PageHeader';
import { StatCard, EmptyState } from '../components/ui';
import ExpiryCalendar, { type ExpiryItem } from '../components/ExpiryCalendar';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';
import { useIsDark } from '../lib/useIsDark';
import { fmtCredits, normZero } from '../lib/format';
import type { UsageHistoryResult } from '../types';

function localDate(d: Date): string {
  const y = d.getFullYear();
  const m = `${d.getMonth() + 1}`.padStart(2, '0');
  const day = `${d.getDate()}`.padStart(2, '0');
  return `${y}-${m}-${day}`;
}

// ---- 趋势时间区间 ----
type RangeKey = 'today' | '7d' | '30d' | 'month' | 'year';
const RANGES: { key: RangeKey; label: string }[] = [
  { key: 'today', label: '今日' },
  { key: '7d', label: '近7天' },
  { key: '30d', label: '近30天' },
  { key: 'month', label: '本月' },
  { key: 'year', label: '近一年' },
];

/** 区间 → 本地自然日序列（升序） */
function rangeDates(range: RangeKey): string[] {
  const today = new Date();
  const dates: string[] = [];
  const push = (d: Date) => dates.push(localDate(d));
  switch (range) {
    case 'today':
      push(today);
      break;
    case '7d':
      for (let i = 6; i >= 0; i--) push(new Date(Date.now() - i * 86400000));
      break;
    case '30d':
      for (let i = 29; i >= 0; i--) push(new Date(Date.now() - i * 86400000));
      break;
    case 'month': {
      for (
        let d = new Date(today.getFullYear(), today.getMonth(), 1);
        d <= today;
        d = new Date(d.getTime() + 86400000)
      ) {
        push(new Date(d));
      }
      break;
    }
    case 'year':
      for (let i = 364; i >= 0; i--) push(new Date(Date.now() - i * 86400000));
      break;
  }
  return dates;
}

// ---- 年度活动热力图（GitHub 贡献图风格：周一为每周首列） ----
/** 绿色梯度：GitHub 明/暗两套色板 */
const HEAT_COLORS = {
  light: { empty: '#ebedf0', levels: ['#9be9a8', '#40c463', '#30a14e', '#216e39'] },
  dark: { empty: '#27272a', levels: ['#0e4429', '#006d32', '#26a641', '#39d353'] },
};

type HeatCell = { date: Date; key: string; credits: number } | null;

/** 近 365 天按周列组织（周一开头），并生成每月首列的月份标签 */
function buildHeatmap(usageMap: Map<string, number>) {
  const days: HeatCell[] = [];
  for (let i = 364; i >= 0; i--) {
    const date = new Date(Date.now() - i * 86400000);
    const key = localDate(date);
    days.push({ date, key, credits: usageMap.get(key.replace(/-/g, '')) ?? 0 });
  }
  // 周一为一周之首：(getDay()=0 周日) → 6
  const lead = days.length ? (days[0]!.date.getDay() + 6) % 7 : 0;
  const flat: HeatCell[] = [...Array<HeatCell>(lead).fill(null), ...days];
  const weeks: HeatCell[][] = [];
  for (let i = 0; i < flat.length; i += 7) {
    const week = flat.slice(i, i + 7);
    while (week.length < 7) week.push(null);
    weeks.push(week);
  }
  // 月份标签：每周列取其首个有效日的月份，月份切换处标注「N月」
  const monthLabels: (string | null)[] = [];
  let prevMonth = -1;
  for (const week of weeks) {
    const first = week.find((c) => c != null)!;
    const m = first!.date.getMonth();
    monthLabels.push(m !== prevMonth ? `${m + 1}月` : null);
    prevMonth = m;
  }
  return { weeks, monthLabels };
}

export default function Credits() {
  const accounts = useAppStore((s) => s.accounts);
  const creditsHistory = useAppStore((s) => s.creditsHistory);

  const creditsDaily = useAppStore((s) => s.creditsDaily);
  const isDark = useIsDark();
  const refreshRemainingCredits = useAppStore((s) => s.refreshRemainingCredits);
  const refreshAccounts = useAppStore((s) => s.refreshAccounts);
  const refreshCreditsDaily = useAppStore((s) => s.refreshCreditsDaily);
  const refreshCreditsHistory = useAppStore((s) => s.refreshCreditsHistory);
  const pushToast = useAppStore((s) => s.pushToast);
  const [refreshing, setRefreshing] = useState(false);

  // ---- 消耗明细（Trae Work 接口，按本地日聚合落盘；fresh=true 增量拉取） ----
  const [usage, setUsage] = useState<UsageHistoryResult | null>(null);
  const [usageLoading, setUsageLoading] = useState(false);
  const [range, setRange] = useState<RangeKey>('7d');
  // 模型筛选：'' = 全部模型（作用于消耗线与消耗总积分）
  const [modelFilter, setModelFilter] = useState('');

  const loadUsage = useCallback(
    async (fresh: boolean) => {
      setUsageLoading(true);
      try {
        setUsage(await api.accounts.usageHistory(fresh));
      } catch (err) {
        pushToast('error', `消耗明细查询失败：${String(err)}`);
      } finally {
        setUsageLoading(false);
      }
    },
    [pushToast],
  );

  useEffect(() => {
    void loadUsage(false);
  }, [loadUsage]);

  const handleRefresh = useCallback(async () => {
    setRefreshing(true);
    try {
      // 1. 刷新所有账号剩余积分（后端会更新 credits_daily.json 快照）
      await refreshRemainingCredits();
      // 2. 重新加载账号列表（remaining_credits 字段）
      await refreshAccounts();
      // 3. 重新加载每日积分快照
      await refreshCreditsDaily();
      // 4. 重新加载签到历史
      await refreshCreditsHistory();
      pushToast('success', '积分数据已刷新');
    } catch (err) {
      pushToast('error', `刷新失败：${String(err)}`);
    } finally {
      setRefreshing(false);
    }
  }, [refreshRemainingCredits, refreshAccounts, refreshCreditsDaily, refreshCreditsHistory, pushToast]);

  const rows = useMemo(
    () =>
      [...accounts].sort((a, b) => {
        const va = a.remaining_credits;
        const vb = b.remaining_credits;
        // null 排到最后
        if (va == null && vb == null) return 0;
        if (va == null) return 1;
        if (vb == null) return -1;
        return vb - va; // 降序
      }),
    [accounts],
  );
  const total = rows.reduce((s, a) => s + (a.remaining_credits ?? 0), 0);
  const avg = rows.length === 0 ? 0 : Math.round(total / rows.length);
  const generalTotal = rows.reduce((s, a) => s + (a.general_credits ?? 0), 0);
  const workTotal = rows.reduce((s, a) => s + (a.work_credits ?? 0), 0);
  const totalHint = accounts.some((a) => a.general_credits != null || a.work_credits != null)
    ? `通用 ${fmtCredits(generalTotal)} 积分 · Work ${fmtCredits(workTotal)} 积分`
    : '总剩余可用积分';

  const today = localDate(new Date());

  // 今日新增积分：优先使用 daily snapshot 的 earned 字段（含签到+购买）
  // 回退：仅签到 history delta
  const todayNew = useMemo(() => {
    // 1. 优先从每日快照获取 earned（包含签到 + 非签到获得）
    const snap = creditsDaily.find((s) => s.date === today);
    if (snap && snap.earned > 0) return Math.round(snap.earned);
    // 2. 回退到签到 history delta
    const histVal = creditsHistory
      .filter((r) => r.date === today)
      .reduce((s, r) => s + (r.delta || 0), 0);
    if (histVal > 0) return histVal;
    // 3. 无任何数据时不显示
    return 0;
  }, [creditsDaily, creditsHistory, today]);

  // 消耗明细按日合计（跨账号）；键统一为紧凑日期（YYYYMMDD），与区间日期格式无关
  const usageMap = useMemo(() => {
    const m = new Map<string, number>();
    for (const a of usage?.accounts ?? []) {
      for (const d of a.daily) {
        const key = d.date.slice(0, 10).replace(/-/g, '');
        m.set(key, (m.get(key) ?? 0) + d.credits);
      }
    }
    return m;
  }, [usage]);

  // 模型 → 日期 → 消耗（模型筛选作用域）
  const usageModelDay = useMemo(() => {
    const m = new Map<string, Map<string, number>>();
    for (const a of usage?.accounts ?? []) {
      for (const d of a.daily) {
        const key = d.date.slice(0, 10).replace(/-/g, '');
        for (const [model, credits] of Object.entries(d.models)) {
          let mm = m.get(model);
          if (!mm) {
            mm = new Map();
            m.set(model, mm);
          }
          mm.set(key, (mm.get(key) ?? 0) + credits);
        }
      }
    }
    return m;
  }, [usage]);

  // 全量模型清单（历史出现过即列入筛选项，按累计消耗降序）
  const allModels = useMemo(() => {
    const total = new Map<string, number>();
    for (const [model, mm] of usageModelDay) {
      let s = 0;
      for (const v of mm.values()) s += v;
      total.set(model, s);
    }
    return [...total.entries()].sort((x, y) => y[1] - x[1]).map(([model]) => model);
  }, [usageModelDay]);

  // 今日消耗积分：优先接口明细（credits_float 实际口径），回退余额差值快照
  const todayConsumed = useMemo(() => {
    const usageVal = usage?.accounts.reduce(
      (s, a) => s + (a.daily.find((d) => d.date === today)?.credits ?? 0),
      0,
    );
    if (usage != null && usageVal != null && usageVal > 0) return usageVal;
    const snap = creditsDaily.find((s) => s.date === today);
    return snap ? snap.consumed : 0;
  }, [usage, creditsDaily, today]);

  // ---- 统计卡（积分总数 / 获得总积分 / 消耗总积分） ----
  const allTimeConsumed = useMemo(() => [...usageMap.values()].reduce((s, v) => s + v, 0), [usageMap]);
  // 区间日期集合（紧凑键）与区间聚合
  const rangeAgg = useMemo(() => {
    const dates = new Set(rangeDates(range).map((d) => d.replace(/-/g, '')));
    let consumed = 0;
    let sessions = 0;
    for (const a of usage?.accounts ?? []) {
      for (const d of a.daily) {
        const key = d.date.slice(0, 10).replace(/-/g, '');
        if (!dates.has(key)) continue;
        if (!modelFilter) consumed += d.credits;
        sessions += d.sessions;
      }
    }
    if (modelFilter) {
      const mm = usageModelDay.get(modelFilter);
      if (mm) for (const key of dates) consumed += mm.get(key) ?? 0;
    }
    // 获得总积分：余额快照 earned 按区间求和
    let earned = 0;
    for (const date of rangeDates(range)) {
      const snap = creditsDaily.find((s) => s.date === date);
      if (snap) earned += snap.earned;
    }
    return { consumed, sessions, earned };
  }, [range, usage, usageModelDay, modelFilter, creditsDaily]);

  // 各模型消耗（按所选区间过滤，跨账号合计，降序）
  const modelChart = useMemo(() => {
    const dates = new Set(rangeDates(range));
    const m = new Map<string, number>();
    for (const a of usage?.accounts ?? []) {
      for (const d of a.daily) {
        if (!dates.has(d.date)) continue;
        for (const [model, credits] of Object.entries(d.models)) {
          m.set(model, (m.get(model) ?? 0) + credits);
        }
      }
    }
    return [...m.entries()].sort((x, y) => y[1] - x[1]);
  }, [usage, range]);
  const usageErrors = useMemo(
    () => (usage?.accounts ?? []).filter((a) => a.error),
    [usage],
  );

  // 趋势数据：消耗线取接口明细（credits_float 实际口径，受模型筛选）；
  // 总数/获得线取余额快照（快照缺失的日期为 null，recharts 跳点不画，避免误导性 0 值）
  const trend = useMemo(() => {
    const snapMap = new Map(creditsDaily.map((s) => [s.date, s]));
    const mm = modelFilter ? usageModelDay.get(modelFilter) : null;
    return rangeDates(range).map((date) => {
      const snap = snapMap.get(date);
      const compact = date.replace(/-/g, '');
      return {
        label:
          range === 'year'
            ? `${date.slice(0, 4)}/${+date.slice(5, 7)}/${+date.slice(8, 10)}`
            : `${+date.slice(5, 7)}/${+date.slice(8, 10)}`,
        total: snap?.total ?? null,
        earned: snap?.earned ?? null,
        consumed: mm ? (mm.get(compact) ?? null) : (usageMap.get(compact) ?? null),
      };
    });
  }, [range, creditsDaily, usageMap, usageModelDay, modelFilter]);

  // 年度热力图（全量历史，不受区间/模型筛选影响）
  const heat = useMemo(() => buildHeatmap(usageMap), [usageMap]);
  const heatMax = useMemo(
    () => Math.max(0, ...[...usageMap.values()]),
    [usageMap],
  );
  const heatColors = isDark ? HEAT_COLORS.dark : HEAT_COLORS.light;
  const heatColor = (v: number): string => {
    if (v <= 0 || heatMax <= 0) return heatColors.empty;
    const r = v / heatMax;
    if (r <= 0.25) return heatColors.levels[0];
    if (r <= 0.5) return heatColors.levels[1];
    if (r <= 0.75) return heatColors.levels[2];
    return heatColors.levels[3];
  };
  const hasHeat = [...usageMap.values()].some((v) => v > 0);

  const hasTrend = trend.some(
    (d) => d.total != null || d.earned != null || d.consumed != null,
  );
  const hasUsage = (usage?.accounts ?? []).some((a) => a.daily.length > 0);
  const showDots = range === 'today' || range === '7d';
  // 会话总数（全量历史）
  const totalSessions = (usage?.accounts ?? []).reduce(
    (s, a) => s + a.daily.reduce((x, d) => x + d.sessions, 0),
    0,
  );

  // 到期日历（F-13 批次 2 补挂 Trae 侧）：token（JWT）+ 积分包 + 会员三类，均 Unix 秒
  const expiryItems = useMemo<ExpiryItem[]>(
    () =>
      accounts.flatMap((a) => {
        const items: ExpiryItem[] = [];
        if (a.jwt_exp_timestamp != null) {
          items.push({ key: `${a.user_id}-jwt`, label: a.name, kind: 'token', expire_ts: a.jwt_exp_timestamp });
        }
        if (a.credits_expire_at != null) {
          items.push({ key: `${a.user_id}-credits`, label: a.name, kind: '积分包', expire_ts: a.credits_expire_at, note: `剩余 ${fmtCredits(a.remaining_credits ?? 0)} 积分` });
        }
        if (a.membership_expire != null) {
          items.push({
            key: `${a.user_id}-membership`,
            label: a.name,
            kind: '会员',
            expire_ts: a.membership_expire,
            note: a.pay_identity ? `套餐 ${a.pay_identity}` : null,
          });
        }
        return items;
      }),
    [accounts],
  );

  return (
    <div className="animate-fade-in">
      <div className="flex items-center justify-between">
        <PageHeader
          title="Trae · 积分看板"
          desc="查看每个账号的积分余额与趋势"
        />
        <button
          className="btn-ghost flex items-center gap-1.5 text-sm"
          onClick={handleRefresh}
          disabled={refreshing}
          title={refreshing ? '刷新中…' : '刷新数据'}
        >
          <RefreshCw size={15} className={refreshing ? 'animate-spin' : ''} />
          {refreshing ? '刷新中' : '刷新'}
        </button>
      </div>

      <div className="mb-5 grid grid-cols-2 gap-3 md:grid-cols-5">
        <StatCard label="可用总积分" value={fmtCredits(total)} hint={totalHint} tone="violet" />
        <StatCard label="账号数" value={rows.length} tone="brand" />
        <StatCard label="平均可用积分" value={normZero(avg).toLocaleString()} tone="blue" />
        <StatCard label="今日新增积分" value={normZero(todayNew).toLocaleString()} tone="green" hint={today} />
        <StatCard
          label="今日消耗积分"
          value={normZero(todayConsumed).toLocaleString('zh-CN', { maximumFractionDigits: 2 })}
          tone="amber"
          hint={today}
        />
      </div>

      <div className="card p-5">
        {/* 头部：标题 + 会话总数 | 模型筛选 | 区间 | 刷新数据 */}
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="font-medium">积分统计</h3>
            <span className="text-xs text-slate-400">会话总数：{totalSessions.toLocaleString()}</span>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <select
              className="rounded-lg border border-slate-200 bg-transparent px-2 py-1 text-xs text-slate-600 outline-none dark:border-zinc-700 dark:text-zinc-300"
              value={modelFilter}
              onChange={(e) => setModelFilter(e.target.value)}
              title="筛选消耗线与消耗总积分的模型"
            >
              <option value="">全部模型</option>
              {allModels.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </select>
            <div className="flex items-center gap-1">
              {RANGES.map((r) => (
                <button
                  key={r.key}
                  onClick={() => setRange(r.key)}
                  className={`chip border ${
                    range === r.key
                      ? 'border-brand-500 text-brand-600 dark:text-brand-400'
                      : 'border-slate-200 text-slate-500 dark:border-zinc-700 dark:text-zinc-400'
                  }`}
                >
                  {r.label}
                </button>
              ))}
            </div>
            {usage && (
              <span
                className="hidden text-xs text-slate-400 lg:inline"
                title="消耗明细最近更新时间（接口口径）"
              >
                明细更新于 {new Date(usage.fetched_at * 1000).toLocaleString('zh-CN', { hour12: false })}
              </span>
            )}
            <button
              className="btn-ghost flex items-center gap-1.5 text-sm"
              onClick={() => void loadUsage(true)}
              disabled={usageLoading}
              title="从 Trae Work 接口增量拉取消耗明细（历史已拉取部分不重复拉取）"
            >
              <RefreshCw size={14} className={usageLoading ? 'animate-spin' : ''} />
              {usageLoading ? '拉取中' : '刷新数据'}
            </button>
          </div>
        </div>

        {/* 统计卡：积分总数（消耗+目前可用）/ 获得总积分 / 消耗总积分 */}
        <div className="mb-5 grid grid-cols-1 gap-3 md:grid-cols-3">
          <StatCard
            label="积分总数（消耗+目前可用）"
            value={normZero(allTimeConsumed + total).toLocaleString()}
            hint={`消耗 ${fmtCredits(allTimeConsumed)} + 目前可用 ${fmtCredits(total)}`}
            tone="brand"
          />
          <StatCard
            label="获得总积分"
            value={normZero(Math.round(rangeAgg.earned)).toLocaleString()}
            hint="所选区间内获得（余额快照口径）"
            tone="green"
          />
          <StatCard
            label="消耗总积分"
            value={normZero(rangeAgg.consumed).toLocaleString('zh-CN', { maximumFractionDigits: 2 })}
            hint={`所选区间${modelFilter ? ` · ${modelFilter}` : ''} · 会话 ${rangeAgg.sessions.toLocaleString()}`}
            tone="amber"
          />
        </div>

        {/* 积分趋势图 */}
        <div className="mb-2 flex items-center gap-3">
          <h4 className="text-sm font-medium">积分趋势图</h4>
          <div className="flex items-center gap-3 text-xs text-slate-400">
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#6366f1' }} />
              积分总数
            </span>
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#22c55e' }} />
              获得积分
            </span>
            <span className="flex items-center gap-1">
              <span className="inline-block h-2 w-2 rounded-full" style={{ background: '#f59e0b' }} />
              消耗积分（接口{modelFilter ? ` · ${modelFilter}` : ''}）
            </span>
          </div>
        </div>
        {accounts.length === 0 ? (
          <EmptyState icon={<Coins size={28} />} title="尚无账号数据" hint="添加账号后这里会展示积分趋势。" />
        ) : !hasTrend ? (
          <EmptyState
            icon={<Coins size={28} />}
            title="暂无趋势数据"
            hint="执行签到或刷新积分后展示余额趋势；点击右上角「刷新数据」可从接口拉取历史消耗。"
          />
        ) : (
          <div className="h-56">
            <ResponsiveContainer>
              <LineChart data={trend} margin={{ top: 24, right: 16, left: 0, bottom: 4 }}>
                <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                <XAxis
                  dataKey="label"
                  tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }}
                  axisLine={{ stroke: isDark ? '#3f3f46' : '#e2e8f0' }}
                  tickLine={false}
                />
                <YAxis tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={56} />
                <Tooltip
                  cursor={{ stroke: isDark ? '#52525b' : '#cbd5e1', strokeWidth: 1, strokeDasharray: '3 3' }}
                  contentStyle={{
                    fontSize: 12,
                    borderRadius: 10,
                    border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
                    background: isDark ? '#18181b' : '#fff',
                    color: isDark ? '#e4e4e7' : '#1e293b',
                    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                    padding: '8px 12px',
                  }}
                  formatter={(v: number, name: string) => {
                    const labels: Record<string, string> = { total: '积分总数', earned: '获得积分', consumed: '消耗积分（接口）' };
                    return [normZero(v).toLocaleString('zh-CN', { maximumFractionDigits: 2 }), labels[name] ?? name];
                  }}
                />
                <Line type="monotone" dataKey="total" stroke="#6366f1" strokeWidth={2.5} dot={showDots ? { r: 3, fill: '#6366f1', strokeWidth: 0 } : false} activeDot={{ r: 5 }} connectNulls />
                <Line type="monotone" dataKey="earned" stroke="#22c55e" strokeWidth={2} dot={showDots ? { r: 3, fill: '#22c55e', strokeWidth: 0 } : false} activeDot={{ r: 5 }} connectNulls />
                <Line type="monotone" dataKey="consumed" stroke="#f59e0b" strokeWidth={3} dot={showDots ? { r: 3, fill: '#f59e0b', strokeWidth: 0 } : false} activeDot={{ r: 5 }} connectNulls />
              </LineChart>
            </ResponsiveContainer>
          </div>
        )}

        {/* 年度活动热力图（近 365 天每日消耗，GitHub 贡献图风格） */}
        <div className="mt-5 border-t border-slate-100 pt-4 dark:border-zinc-800">
          <div className="mb-2 flex items-center gap-2">
            <h4 className="text-sm font-medium">年度活动热力图</h4>
            <span className="text-xs text-slate-400">近 365 天 · 每日消耗积分</span>
          </div>
          {!hasHeat ? (
            <div className="py-4 text-xs text-slate-400">
              暂无消耗记录：点击右上角「刷新数据」从接口拉取消耗明细后展示。
            </div>
          ) : (
            <div className="overflow-x-auto pb-1">
              <div className="inline-flex min-w-max flex-col gap-[3px]">
                {/* 月份标签行 */}
                <div className="flex gap-[3px]">
                  <span className="w-5 shrink-0" />
                  {heat.weeks.map((_, i) => (
                    <span
                      key={i}
                      className="w-[11px] shrink-0 whitespace-nowrap text-[10px] leading-none text-slate-400 dark:text-zinc-500"
                    >
                      {heat.monthLabels[i] ?? ''}
                    </span>
                  ))}
                </div>
                {/* 周一/三/五 三行 + 其余四行 */}
                {heat.weeks[0]?.map((_, dayIdx) => (
                  <div key={dayIdx} className="flex items-center gap-[3px]">
                    <span className="w-5 shrink-0 text-[10px] leading-none text-slate-400 dark:text-zinc-500">
                      {dayIdx === 0 ? '一' : dayIdx === 2 ? '三' : dayIdx === 4 ? '五' : ''}
                    </span>
                    {heat.weeks.map((week, wi) => {
                      const cell = week[dayIdx];
                      if (!cell) return <span key={wi} className="h-[11px] w-[11px] shrink-0" />;
                      return (
                        <div
                          key={wi}
                          className="h-[11px] w-[11px] shrink-0 rounded-[2px]"
                          style={{ background: heatColor(cell.credits) }}
                          title={`${cell.key} · 消耗 ${fmtCredits(cell.credits)} 积分`}
                        />
                      );
                    })}
                  </div>
                ))}
                {/* 少 → 多 图例 */}
                <div className="mt-1 flex items-center justify-end gap-1 text-[10px] text-slate-400 dark:text-zinc-500">
                  少
                  {[heatColors.empty, ...heatColors.levels].map((c) => (
                    <span key={c} className="h-[10px] w-[10px] rounded-[2px]" style={{ background: c }} />
                  ))}
                  多
                </div>
              </div>
            </div>
          )}
        </div>

        {/* 模型消耗排行 */}
        {modelChart.length > 0 && (
          <div className="mt-5 border-t border-slate-100 pt-4 dark:border-zinc-800">
            <div className="mb-2 flex items-center gap-2">
              <h4 className="text-sm font-medium">模型消耗排行</h4>
              <span className="text-xs text-slate-400">所选区间 · 跨账号合计</span>
            </div>
            <div className="h-52">
              <ResponsiveContainer>
                <BarChart data={modelChart} margin={{ top: 20, right: 16, left: 0, bottom: 4 }} barCategoryGap="24%">
                  <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
                  <XAxis
                    dataKey="0"
                    tick={{ fontSize: 10, fill: isDark ? '#a1a1aa' : '#94a3b8' }}
                    axisLine={{ stroke: isDark ? '#3f3f46' : '#e2e8f0' }}
                    tickLine={false}
                    interval={0}
                    angle={-20}
                    textAnchor="end"
                    height={58}
                  />
                  <YAxis tick={{ fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' }} axisLine={false} tickLine={false} width={56} />
                  <Tooltip
                    cursor={{ fill: isDark ? 'rgba(255,255,255,0.05)' : 'rgba(0,0,0,0.03)' }}
                    contentStyle={{
                      fontSize: 12,
                      borderRadius: 10,
                      border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
                      background: isDark ? '#18181b' : '#fff',
                      color: isDark ? '#e4e4e7' : '#1e293b',
                      boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
                      padding: '8px 12px',
                    }}
                    formatter={(v: number) => [normZero(v).toLocaleString('zh-CN', { maximumFractionDigits: 2 }), '消耗积分']}
                  />
                  <Bar dataKey="1" fill="#8b5cf6" radius={[4, 4, 0, 0]} maxBarSize={40}>
                    <LabelList
                      dataKey="1"
                      position="top"
                      formatter={(v: number) => (v >= 10000 ? `${(v / 10000).toFixed(1)}w` : v >= 1000 ? `${(v / 1000).toFixed(1)}k` : v.toFixed(0))}
                      style={{ fontSize: 10, fill: isDark ? '#a1a1aa' : '#94a3b8', fontWeight: 500 }}
                    />
                  </Bar>
                </BarChart>
              </ResponsiveContainer>
            </div>
          </div>
        )}

        {usageErrors.length > 0 && (
          <div className="mt-4 rounded-lg bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-400">
            {usageErrors.map((a) => `${a.name}：${a.error}`).join('；')}
          </div>
        )}
        {!hasUsage && (
          <div className="mt-4 text-xs text-slate-400">
            消耗线暂无数据：首次点击「刷新数据」将全量拉取近一年历史，之后每次仅增量拉取。
          </div>
        )}
      </div>

      {/* 积分到期日历（F-13） */}
      <div className="mt-5 card p-4">
        <h3 className="mb-3 font-medium">积分到期日历</h3>
        <ExpiryCalendar items={expiryItems} emptyHint="暂无到期项：待账号完成签到/积分查询后展示 token、积分包与会员到期时间。" />
      </div>
    </div>
  );
}
