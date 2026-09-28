export const handler = async () => {
  await new Promise((r) => setTimeout(r, 60_000));
  return 'never reached';
};
