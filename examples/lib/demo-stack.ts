import { join } from 'node:path';
import { Duration, aws_lambda as lambda, Stack, type StackProps } from 'aws-cdk-lib';
import type { Construct } from 'constructs';
import { Zankyo } from 'zankyo-cdk';

// ハンドラは handlers/ に素の JS で置き、esbuild 等のバンドル依存をデモに持ち込まない。
// inline code にしないのは、sam local が inline code を実行しないため
// （README の手順 4 で fixture を再現する）。
const HANDLERS_DIR = join(import.meta.dirname, '../handlers');

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
      code: lambda.Code.fromAsset(HANDLERS_DIR),
      memorySize: 256,
    } as const;

    const thrower = new lambda.Function(this, 'Thrower', {
      ...base,
      handler: 'thrower.handler',
      timeout: Duration.seconds(10),
    });
    const sleeper = new lambda.Function(this, 'Sleeper', {
      ...base,
      handler: 'sleeper.handler',
      // timeout を短くして SHUTDOWN フラッシュ経路を踏む
      timeout: Duration.seconds(3),
    });
    const initError = new lambda.Function(this, 'InitError', {
      ...base,
      handler: 'init-error.handler',
      timeout: Duration.seconds(10),
    });

    for (const fn of [thrower, sleeper, initError]) {
      zankyo.attachTo(fn);
    }
  }
}
