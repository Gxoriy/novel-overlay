/**
 * 自动滚动的位移规划。
 *
 * 为什么不能直接 `scrollTop += speed * 帧时长 / 1000`：
 * 滚动位置会被浏览器取整到整像素，每帧不足 1px 的位移在取整时被抹掉，
 * 而且下一帧又是从被抹过的 `scrollTop` 起算 —— 误差不会累积，只会永远归零。
 * 表现就是「速度调到 19 以下就完全不动」（15px/s 时每帧只有 0.495px，
 * 四舍五入到 0；19px/s 时是 0.627px，进位到 1 才动得起来）。
 *
 * 这里把不足 1px 的余量交还给调用方保存，攒够整像素再落地，
 * 低速也能按真实速度均匀推进，而不是卡住或跳着走。
 */

export interface ScrollStep {
  /** 下一帧应写入的 scrollTop */
  scrollTop: number;
  /** 本帧没落地的亚像素余量，范围 [0, 1)，需由调用方保存到下一帧 */
  remainder: number;
}

export function planScroll(
  current: number,
  remainder: number,
  deltaPx: number,
  maxScroll: number,
): ScrollStep {
  const limit = Math.max(0, maxScroll);
  const accumulated = Math.max(0, remainder) + Math.max(0, deltaPx);
  const whole = Math.floor(accumulated);
  const base = Math.max(0, Math.round(current));
  const scrollTop = Math.min(base + whole, limit);
  // 已经到底：余量清零，避免停留期间越攒越多、翻章后一上来就跳一段
  if (scrollTop >= limit) {
    return { scrollTop, remainder: 0 };
  }
  return { scrollTop, remainder: accumulated - whole };
}
