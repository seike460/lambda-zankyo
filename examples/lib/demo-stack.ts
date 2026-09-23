import { Duration, aws_lambda as lambda, Stack, type StackProps } from 'aws-cdk-lib';
import type { Construct } from 'constructs';
import { Zankyo } from 'zankyo-cdk';

// ハンドラは検証目的を一目で分かるよう inline で書く。
// esbuild 等のバンドル依存をデモに持ち込まないための選択。
const THROWING = `
exports.handler = async (event) => {
  // PII 混入イベントで scrub の動作も確認する
  throw new Error('demo failure for ' + JSON.stringify(event));
};
`;

const SLEEPING = `
exports.handler = async () => {
  await new Promise((r) => setTimeout(r, 60_000));
  return 'never reached';
};
`;

// モジュール評価時点で投げる = init error の再現
const INIT_ERROR = `
throw new Error('demo init failure');
exports.handler = async () => 'never reached';
`;

/**
 * 3 種類の失敗（handler error / timeout / init error）を起こす関数を
 * Zankyo 付きでデプロイする検証用スタック。
 * E2E 手順は SPEC.md「E2E 検証手順」および examples/README.md を参照。
 */
export class DemoStack extends Stack {
  constructor(scope: Construct, id: string, props?: StackProps) {
    super(scope, id, props);

    const zankyo = new Zankyo(this, 'Zankyo', {
      // layer: 未指定 → SAR 参照。セルフホスト時は fromLayerVersionArn で渡す
      scrubFields: ['demo-secret'],
    });

    const base = {
      runtime: lambda.Runtime.NODEJS_22_X,
      handler: 'index.handler',
      memorySize: 256,
    } as const;

    const thrower = new lambda.Function(this, 'Thrower', {
      ...base,
      code: lambda.Code.fromInline(THROWING),
      timeout: Duration.seconds(10),
    });
    const sleeper = new lambda.Function(this, 'Sleeper', {
      ...base,
      code: lambda.Code.fromInline(SLEEPING),
      // timeout を短くして SHUTDOWN フラッシュ経路を踏む
      timeout: Duration.seconds(3),
    });
    const initError = new lambda.Function(this, 'InitError', {
      ...base,
      code: lambda.Code.fromInline(INIT_ERROR),
      timeout: Duration.seconds(10),
    });

    for (const fn of [thrower, sleeper, initError]) {
      zankyo.attachTo(fn);
    }
  }
}
