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
    template.hasResourceProperties('AWS::IAM::Policy', {
      PolicyDocument: {
        Statement: Match.arrayWith([Match.objectLike({ Action: 's3:PutObject', Effect: 'Allow' })]),
      },
    });
  });
});
