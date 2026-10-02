export const handler = async () => 'never reached';

// モジュール評価時点で投げる = init error の再現
throw new Error('demo init failure');
