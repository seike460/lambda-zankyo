import assert from 'node:assert/strict';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { describe, it } from 'node:test';
import { App } from 'aws-cdk-lib';
import { Match, Template } from 'aws-cdk-lib/assertions';
import { DemoStack } from '../lib/demo-stack.ts';

describe('DemoStack', () => {
  const app = new App();
  const template = Template.fromStack(new DemoStack(app, 'Demo'));

  it('attaches the zankyo wrapper to all three demo functions', () => {
    template.resourceCountIs('AWS::Lambda::Function', 3);
    template.allResourcesProperties('AWS::Lambda::Function', {
      Environment: {
        Variables: Match.objectLike({
          AWS_LAMBDA_EXEC_WRAPPER: '/opt/zankyo-wrapper',
          ZANKYO_SCRUB_FIELDS: 'demo-secret',
        }),
      },
      Layers: Match.anyValue(),
    });
  });

  it('creates a private records bucket via the construct default', () => {
    template.hasResourceProperties('AWS::S3::Bucket', {
      PublicAccessBlockConfiguration: {
        BlockPublicAcls: true,
        BlockPublicPolicy: true,
        IgnorePublicAcls: true,
        RestrictPublicBuckets: true,
      },
    });
  });

  it('grants only s3:PutObject to demo functions', () => {
    template.resourceCountIs('AWS::IAM::Policy', 3);
    template.allResourcesProperties('AWS::IAM::Policy', {
      PolicyDocument: {
        Statement: [Match.objectLike({ Action: 's3:PutObject', Effect: 'Allow' })],
      },
      Roles: [{ Ref: Match.stringLikeRegexp('^(Thrower|Sleeper|InitError)ServiceRole') }],
    });
    template.allResourcesProperties('AWS::IAM::Role', {
      Policies: Match.absent(),
      ManagedPolicyArns: [
        {
          'Fn::Join': [
            '',
            [
              'arn:',
              { Ref: 'AWS::Partition' },
              ':iam::aws:policy/service-role/AWSLambdaBasicExecutionRole',
            ],
          ],
        },
      ],
    });
  });

  it('ships the handlers as files, which sam local can run', () => {
    template.allResourcesProperties('AWS::Lambda::Function', {
      Code: { S3Key: Match.anyValue(), ZipFile: Match.absent() },
    });
    for (const fn of Object.values(template.findResources('AWS::Lambda::Function'))) {
      const module = String(fn.Properties.Handler).replace(/\.handler$/, '');
      assert.ok(existsSync(join(import.meta.dirname, '../handlers', `${module}.mjs`)), module);
    }
  });
});
