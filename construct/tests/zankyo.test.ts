import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { describe, it } from 'node:test';
import { App, aws_kms as kms, aws_lambda as lambda, Stack, aws_s3 as s3 } from 'aws-cdk-lib';
import { Match, Template } from 'aws-cdk-lib/assertions';
import { Zankyo } from '../src/index.ts';

const PUBLISHED_APPLICATION_ID =
  'arn:aws:serverlessrepo:ap-northeast-1:446537410535:applications/lambda-zankyo';

/** SAR に公開するアプリの定義。construct の既定の版と Output 名は、これと揃える。 */
const sarTemplate = readFileSync(new URL('../../sar/template.yaml', import.meta.url), 'utf8');

function fn(
  stack: Stack,
  id: string,
  architecture: lambda.Architecture = lambda.Architecture.X86_64,
): lambda.Function {
  return new lambda.Function(stack, id, {
    runtime: lambda.Runtime.NODEJS_22_X,
    handler: 'index.handler',
    architecture,
    code: lambda.Code.fromInline('exports.handler = async () => "ok"'),
  });
}

/** スタックに 1 つずつある SAR アプリの論理 ID と、関数に付いた Layer を返す。 */
function sarWiring(t: Template): { appId: string | undefined; layers: unknown } {
  t.resourceCountIs('AWS::Serverless::Application', 1);
  t.resourceCountIs('AWS::Lambda::Function', 1);
  const [appId] = Object.keys(t.findResources('AWS::Serverless::Application'));
  const [f] = Object.values(t.findResources('AWS::Lambda::Function'));
  return { appId, layers: f?.Properties?.Layers };
}

function newStack(): Stack {
  return new Stack(new App(), 'TestStack');
}

function synth(setup: (stack: Stack) => void): Template {
  const stack = newStack();
  setup(stack);
  return Template.fromStack(stack);
}

describe('Zankyo', () => {
  it('creates a private bucket with 30-day lifecycle when none is given', () => {
    const t = synth((stack) => {
      new Zankyo(stack, 'Z', {});
    });
    t.hasResourceProperties('AWS::S3::Bucket', {
      PublicAccessBlockConfiguration: {
        BlockPublicAcls: true,
        BlockPublicPolicy: true,
        IgnorePublicAcls: true,
        RestrictPublicBuckets: true,
      },
      LifecycleConfiguration: {
        Rules: Match.arrayWith([Match.objectLike({ ExpirationInDays: 30 })]),
      },
    });
  });

  it('deploys the published SAR application at the version in sar/template.yaml', () => {
    const semanticVersion = /^ {4}SemanticVersion: (\S+)$/m.exec(sarTemplate)?.[1];
    assert.ok(semanticVersion, 'sar/template.yaml declares SemanticVersion');
    const t = synth((stack) => {
      new Zankyo(stack, 'Z').attachTo(fn(stack, 'Fn'));
    });
    t.hasResourceProperties('AWS::Serverless::Application', {
      Location: { ApplicationId: PUBLISHED_APPLICATION_ID, SemanticVersion: semanticVersion },
    });
    assert.match(sarTemplate, /^Outputs:\n(?:.*\n)*? {2}LayerVersionArn:$/m);
    const { appId, layers } = sarWiring(t);
    assert.deepEqual(layers, [{ 'Fn::GetAtt': [appId, 'Outputs.LayerVersionArn'] }]);
  });

  it('uses the arm64 SAR output when arm64 is set', () => {
    const t = synth((stack) => {
      new Zankyo(stack, 'Z', { arm64: true }).attachTo(fn(stack, 'Fn', lambda.Architecture.ARM_64));
    });
    assert.match(sarTemplate, /^Outputs:\n(?:.*\n)*? {2}LayerVersionArnArm64:$/m);
    const { appId, layers } = sarWiring(t);
    assert.deepEqual(layers, [{ 'Fn::GetAtt': [appId, 'Outputs.LayerVersionArnArm64'] }]);
  });

  it('attachTo wires env, layer and PutObject permission', () => {
    const t = synth((stack) => {
      const z = new Zankyo(stack, 'Z', {
        layer: lambda.LayerVersion.fromLayerVersionArn(
          stack,
          'L',
          'arn:aws:lambda:us-east-1:123456789012:layer:zankyo:1',
        ),
      });
      z.attachTo(fn(stack, 'Fn'));
    });
    t.hasResourceProperties('AWS::Lambda::Function', {
      Environment: {
        Variables: Match.objectLike({
          AWS_LAMBDA_EXEC_WRAPPER: '/opt/zankyo-wrapper',
          ZANKYO_BUCKET: Match.anyValue(),
        }),
      },
      Layers: Match.anyValue(),
    });
    t.hasResourceProperties('AWS::IAM::Policy', {
      PolicyDocument: {
        Statement: Match.arrayWith([
          Match.objectLike({
            Action: 's3:PutObject',
            Effect: 'Allow',
          }),
        ]),
      },
    });
  });

  it('scopes PutObject to each function own key prefix', () => {
    const fnRefs: unknown[] = [];
    const t = synth((stack) => {
      const z = new Zankyo(stack, 'Z', {
        layer: lambda.LayerVersion.fromLayerVersionArn(
          stack,
          'L',
          'arn:aws:lambda:us-east-1:123456789012:layer:zankyo:1',
        ),
      });
      for (const id of ['A', 'B']) {
        const f = fn(stack, id);
        z.attachTo(f);
        fnRefs.push(stack.resolve(f.functionName));
      }
    });
    for (const fnRef of fnRefs) {
      t.hasResourceProperties('AWS::IAM::Policy', {
        PolicyDocument: {
          Statement: [
            Match.objectLike({
              Action: 's3:PutObject',
              Resource: { 'Fn::Join': ['', [Match.anyValue(), '/zankyo/', fnRef, '/*']] },
            }),
          ],
        },
      });
    }
  });

  it('uses an existing bucket and emits scrub fields', () => {
    const t = synth((stack) => {
      const bucket = s3.Bucket.fromBucketName(stack, 'B', 'existing-records');
      const z = new Zankyo(stack, 'Z', {
        bucket,
        scrubFields: ['my-secret'],
        layer: lambda.LayerVersion.fromLayerVersionArn(
          stack,
          'L',
          'arn:aws:lambda:us-east-1:123456789012:layer:zankyo:1',
        ),
      });
      z.attachTo(fn(stack, 'Fn'));
    });
    t.hasResourceProperties('AWS::Lambda::Function', {
      Environment: {
        Variables: Match.objectLike({
          ZANKYO_BUCKET: 'existing-records',
          ZANKYO_SCRUB_FIELDS: 'my-secret',
        }),
      },
    });
  });

  it('grants kms encrypt when a key is provided', () => {
    const t = synth((stack) => {
      const key = new kms.Key(stack, 'K');
      const z = new Zankyo(stack, 'Z', {
        kmsKey: key,
        layer: lambda.LayerVersion.fromLayerVersionArn(
          stack,
          'L',
          'arn:aws:lambda:us-east-1:123456789012:layer:zankyo:1',
        ),
      });
      z.attachTo(fn(stack, 'Fn'));
    });
    t.hasResourceProperties('AWS::IAM::Policy', {
      PolicyDocument: {
        Statement: Match.arrayWith([
          Match.objectLike({
            Action: Match.arrayWith(['kms:Encrypt', 'kms:GenerateDataKey']),
            Effect: 'Allow',
          }),
        ]),
      },
    });
  });
});
