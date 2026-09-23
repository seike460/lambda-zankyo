import {
  Duration,
  aws_iam as iam,
  type aws_kms as kms,
  aws_lambda as lambda,
  RemovalPolicy,
  aws_s3 as s3,
  aws_sam as sam,
} from 'aws-cdk-lib';
import { Construct } from 'constructs';

/**
 * SAR 上のアプリケーション ID。
 * SAR の applicationId は公開時に確定する ARN のため、初回 publish までは
 * `layer` props でのセルフホストを推奨する（SPEC: GitHub Releases zip 同梱）。
 * props.sarApplicationId で任意の公開済み ARN を指すこともできる。
 */
export const ZANKYO_APPLICATION_ID =
  'arn:aws:serverlessrepo:us-east-1:000000000000:applications/lambda-zankyo';

/** SAR アプリが出力する LayerVersion ARN の Output 名（sar/template.yaml と契約）。 */
const SAR_LAYER_OUTPUT = 'LayerVersionArn';
const SAR_LAYER_OUTPUT_ARM64 = 'LayerVersionArnArm64';

const DEFAULT_SEMANTIC_VERSION = '0.1.0';
const DEFAULT_RETENTION_DAYS = 30;
const WRAPPER_PATH = '/opt/zankyo-wrapper';

export interface ZankyoProps {
  /**
   * 失敗レコードの保存先。未指定ならライフサイクル
   * `recordRetentionDays`・パブリックアクセス全ブロックのバケットを
   * 新規作成する。
   */
  readonly bucket?: s3.IBucket;
  /** 新規バケットのレコード保持日数（既定 30）。bucket 指定時は無関係。 */
  readonly recordRetentionDays?: number;
  /** SSE-KMS に使うキー。未指定なら SSE-S3。 */
  readonly kmsKey?: kms.IKey;
  /** 追加の scrub フィールド名。`ZANKYO_SCRUB_FIELDS` に展開される。 */
  readonly scrubFields?: readonly string[];
  /**
   * SAR アプリケーションの ARN。既定は ZANKYO_APPLICATION_ID。
   * layer を渡した場合は SAR 参照自体を作らない。
   */
  readonly sarApplicationId?: string;
  /** SAR アプリのセマンティックバージョン。 */
  readonly semanticVersion?: string;
  /**
   * セルフホストする LayerVersion。指定時は SAR アプリをデプロイせず
   * この Layer をそのまま使う（SAR 審査中・独自ビルド向け）。
   */
  readonly layer?: lambda.ILayerVersion;
  /**
   * SAR アプリの arm64 用 Output を使う（既定 false = x86_64）。
   * layer を渡した場合は無関係。
   */
  readonly arm64?: boolean;
}

/**
 * zankyo の配線を一発で行う construct。
 * bucket/kmsKey/scrubFields だけ渡せば、Layer 参照・環境変数・
 * s3:PutObject(+kms) 権限まで attachTo でまとめて設定する。
 */
export class Zankyo extends Construct {
  /** 失敗レコードの保存先バケット。 */
  readonly bucket: s3.IBucket;
  private readonly layer: lambda.ILayerVersion;
  private readonly kmsKey: kms.IKey | undefined;
  private readonly scrubFields: readonly string[] | undefined;

  constructor(scope: Construct, id: string, props: ZankyoProps = {}) {
    super(scope, id);
    this.kmsKey = props.kmsKey;
    this.scrubFields = props.scrubFields;
    this.bucket =
      props.bucket ??
      new s3.Bucket(this, 'Records', {
        blockPublicAccess: s3.BlockPublicAccess.BLOCK_ALL,
        enforceSSL: true,
        ...(props.kmsKey
          ? { encryption: s3.BucketEncryption.KMS, encryptionKey: props.kmsKey }
          : { encryption: s3.BucketEncryption.S3_MANAGED }),
        lifecycleRules: [
          { expiration: Duration.days(props.recordRetentionDays ?? DEFAULT_RETENTION_DAYS) },
        ],
        // 失敗レコードは監査証跡になりうるため、誤削除より残す方を既定にする
        removalPolicy: RemovalPolicy.RETAIN,
      });
    this.layer =
      props.layer ??
      lambda.LayerVersion.fromLayerVersionArn(this, 'SarLayer', this.sarLayerArn(props));
  }

  /** SAR アプリケーションをデプロイして LayerVersion ARN を取り出す。 */
  private sarLayerArn(props: ZankyoProps): string {
    const app = new sam.CfnApplication(this, 'SarApp', {
      location: {
        applicationId: props.sarApplicationId ?? ZANKYO_APPLICATION_ID,
        semanticVersion: props.semanticVersion ?? DEFAULT_SEMANTIC_VERSION,
      },
    });
    const output = props.arm64 ? SAR_LAYER_OUTPUT_ARM64 : SAR_LAYER_OUTPUT;
    return app.getAtt(`Outputs.${output}`).toString();
  }

  /**
   * 関数に zankyo を取り付ける: Layer 追加・exec wrapper/env 注入・
   * 記録先への PutObject（+ KMS）権限付与。
   */
  public attachTo(fn: lambda.Function): void {
    fn.addLayers(this.layer);
    fn.addEnvironment('AWS_LAMBDA_EXEC_WRAPPER', WRAPPER_PATH);
    fn.addEnvironment('ZANKYO_BUCKET', this.bucket.bucketName);
    if (this.kmsKey) {
      fn.addEnvironment('ZANKYO_KMS_KEY', this.kmsKey.keyArn);
    }
    if (this.scrubFields && this.scrubFields.length > 0) {
      fn.addEnvironment('ZANKYO_SCRUB_FIELDS', this.scrubFields.join(','));
    }
    // 記録に必要なのは PutObject のみ。grantPut は LegalHold/Tagging 等まで
    // 広く付くため、最小権限の方針に合わせアクションを明示する。
    fn.addToRolePolicy(
      new iam.PolicyStatement({
        actions: ['s3:PutObject'],
        resources: [this.bucket.arnForObjects('*')],
      }),
    );
    // SSE-KMS 書き込みに必要なのは Encrypt と GenerateDataKey。Decrypt は不要。
    this.kmsKey?.grant(fn, 'kms:Encrypt', 'kms:GenerateDataKey');
  }
}
