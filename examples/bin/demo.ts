#!/usr/bin/env node
import { App } from 'aws-cdk-lib';
import { DemoStack } from '../lib/demo-stack.ts';

const app = new App();
new DemoStack(app, 'ZankyoDemo', {
  description: 'lambda-zankyo demo: failing functions wired with the Zankyo construct',
});
