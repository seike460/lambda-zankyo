export const handler = async () => {
  // scrub の確認はレコードの event で行う。例外メッセージには denylist が効かないため、
  // イベントを埋め込まない（README「PII scrub」）
  throw new Error('demo failure');
};
