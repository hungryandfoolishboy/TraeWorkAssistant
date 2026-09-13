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
  { key: 'today', label: '今天' },
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

  // 趋势数据：消耗线取接口明细（credits_float 实际口径）；总数/获得线取余额快照
  //（快照缺失的日期为 null，recharts 跳点不画，避免误导性 0 值）
  const trend = useMemo(() => {
    const snapMap = new Map(creditsDaily.map((s) => [s.date, s]));
    return rangeDates(range).map((date) => {
      const snap = snapMap.get(date);
      return {
        label:
          range === 'year'
            ? `${date.slice(0, 4)}/${+date.slice(5, 7)}/${+date.slice(8, 10)}`
            : `${+date.slice(5, 7)}/${+date.slice(8, 10)}`,
        total: snap?.total ?? null,
        earned: snap?.earned ?? null,
        consumed: usageMap.get(date.replace(/-/g, '')) ?? null,
      };
    });
  }, [range, creditsDaily, usageMap]);

  const hasTrend = trend.some(
    (d) => d.total != null || d.earned != null || d.consumed != null,
  );
  const hasUsage = (usage?.accounts ?? []).some((a) => a.daily.length > 0);
  const showDots = range === 'today' || range === '7d';

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
        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="font-medium">积分趋势</h3>
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
          </div>
          <div className="flex items-center gap-3">
            {usage && (
              <span className="text-xs text-slate-400" title="消耗明细最近更新时间（接口口径）">
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
              {usageLoading ? '拉取中' : '更新消耗明细'}
            </button>
          </div>
        </div>
        <div className="mb-3 flex items-center gap-3 text-xs text-slate-400">
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
            消耗积分（接口）
          </span>
        </div>
        {accounts.length === 0 ? (
          <EmptyState icon={<Coins size={28} />} title="尚无账号数据" hint="添加账号后这里会展示积分趋势。" />
        ) : !hasTrend ? (
          <EmptyState
            icon={<Coins size={28} />}
            title="暂无趋势数据"
            hint="执行签到或刷新积分后展示余额趋势；点击右上角「更新消耗明细」可从接口拉取历史消耗。"
          />
        ) : (
          <>
            <div className={showDots ? 'h-56' : 'h-56'}>
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
            {modelChart.length > 0 && (
              <div className="mt-4">
                <div className="mb-2 flex items-center gap-2">
                  <h4 className="text-sm font-medium">各模型消耗积分</h4>
                  <span className="text-xs text-slate-400">当前区间 · 跨账号合计</span>
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
              <div className="mt-3 rounded-lg bg-amber-50 px-3 py-2 text-xs text-amber-700 dark:bg-amber-500/10 dark:text-amber-400">
                {usageErrors.map((a) => `${a.name}：${a.error}`).join('；')}
              </div>
            )}
            {!hasUsage && (
              <div className="mt-3 text-xs text-slate-400">
                消耗线暂无数据：首次点击「更新消耗明细」将全量拉取近一年历史，之后每次仅增量拉取。
              </div>
            )}
          </>
        )}
      </div>

      {/* 到期日历（F-13） */}
      <div className="mt-5 card p-4">
        <h3 className="mb-3 font-medium">到期日历</h3>
        <ExpiryCalendar items={expiryItems} emptyHint="暂无到期项：待账号完成签到/积分查询后展示 token、积分包与会员到期时间。" />
      </div>
    </div>
  );
}
