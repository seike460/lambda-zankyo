/**
 * AWS 境界の構造的ポート。
 * 本番は aws.ts が SDK クライアントをこの形へ適合させ、
 * テストは SDK 型への cast なしでフェイクを差し込める。
 * SDK の send(command) ではなく、必要な操作を名前で絞る。
 */

/** listRecordKeys が実際に読む最小のページ形。SDK の出力はこの上位集合。 */
export interface RecordListPage {
  readonly Contents?:
    | readonly {
        readonly Key?: string | undefined;
        readonly LastModified?: Date | undefined;
      }[]
    | undefined;
  readonly IsTruncated?: boolean | undefined;
  readonly NextContinuationToken?: string | undefined;
}

export interface RecordObjectBody {
  transformToString(encoding?: string): Promise<string>;
}

/** fetchRecord が実際に読む最小のオブジェクト形。 */
export interface RecordObject {
  readonly ContentLength?: number | undefined;
  readonly Body?: RecordObjectBody | undefined;
}

export interface ListInput {
  readonly Bucket: string;
  readonly Prefix: string;
  readonly ContinuationToken?: string | undefined;
  readonly MaxKeys?: number | undefined;
}

export interface GetInput {
  readonly Bucket: string;
  readonly Key: string;
}

/** レコード読み取りに必要な S3 操作だけを持つポート。 */
export interface RecordReader {
  listObjectsV2(input: ListInput): Promise<RecordListPage>;
  getObject(input: GetInput): Promise<RecordObject>;
}

export interface InvokeInput {
  readonly FunctionName: string;
  readonly Payload: Uint8Array;
}

/** invokeFunction が実際に読む最小のレスポンス形。 */
export interface InvokeResult {
  readonly StatusCode?: number | undefined;
  readonly FunctionError?: string | undefined;
  readonly Payload?: Uint8Array | undefined;
}

/** 関数呼び出しに必要な Lambda 操作だけを持つポート。 */
export interface FunctionInvoker {
  invoke(input: InvokeInput): Promise<InvokeResult>;
}
