const performanceTimeOrigin = performance.timeOrigin;
const performanceNow = performance.now.bind(performance);

export function getSafeTimestamp(): number {
  return Math.floor(performanceTimeOrigin + performanceNow());
}
