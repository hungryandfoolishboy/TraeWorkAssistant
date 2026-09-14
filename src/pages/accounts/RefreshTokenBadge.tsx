import { AlertTriangle, Clock, ShieldOff } from 'lucide-react';
import { Badge } from '../../components/ui';

/**
 * F-78 批次 3：refresh_token 生命周期徽标（账号管理 + API 服务账号池两处消费）。
 * 优先级：已判定失效 > 连续刷新失败 > 即将过期；无异常不渲染。
 */
export function RefreshTokenBadge({
  invalid,
  fails,
  expiresAt,
}: {
  invalid?: boolean;
  fails?: number;
  expiresAt?: number | null;
}) {
  if (invalid) {
    return (
      <Badge tone="red" title="refresh_token 已失效，需重新 OAuth 登录后恢复">
        <ShieldOff size={12} /> Token 失效
      </Badge>
    );
  }
  if ((fails ?? 0) > 0) {
    return (
      <Badge tone="amber" title={`refresh_token 连续刷新失败 ${fails} 次（累计 3 次判定失效）`}>
        <AlertTriangle size={12} /> 刷新失败×{fails}
      </Badge>
    );
  }
  if (expiresAt && expiresAt > 0) {
    const days = (expiresAt - Date.now() / 1000) / 86400;
    if (days <= 0) {
      return (
        <Badge tone="red" title="refresh_token 已过期，自动续期将不可用">
          <Clock size={12} /> Refresh 过期
        </Badge>
      );
    }
    if (days <= 7) {
      return (
        <Badge tone="amber" title={`refresh_token 约 ${days.toFixed(1)} 天后过期`}>
          <Clock size={12} /> Refresh {days.toFixed(0)}d
        </Badge>
      );
    }
  }
  return null;
}
