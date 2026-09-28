export const handler = async (event) => {
  // PII 混入イベントで scrub の動作も確認する
  throw new Error(`demo failure for ${JSON.stringify(event)}`);
};
