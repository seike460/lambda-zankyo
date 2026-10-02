#!/usr/bin/env node
//! リリースで上げる版が、すべての配布物でそろっているかを確かめる。
//! 対象は npm の 2 パッケージ・proxy の crate（Cargo.lock を含む）・SAR の SemanticVersion。
//! あわせて、SAR の SourceCodeUrl がその版のタグを指すこと、
//! CHANGELOG.md にその版の見出しがあることを確かめる。
//! construct の既定の SAR 版は、construct/tests/zankyo.test.ts が sar/template.yaml と突き合わせる。

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

const read = (path: string) => readFileSync(join(ROOT, path), 'utf8');
const capture = (text: string | undefined, pattern: RegExp) =>
  text === undefined ? undefined : pattern.exec(text)?.[1];

const sarTemplate = read('sar/template.yaml');
const sarMetadata = (key: string) => capture(sarTemplate, new RegExp(`^ {4}${key}: (\\S+)$`, 'm'));
const cargoPackage = capture(read('proxy/Cargo.toml'), /^\[package\]\n((?:(?!\[).*\n)*)/m);

const versions: [source: string, version: string | undefined][] = [
  ['cli/package.json', JSON.parse(read('cli/package.json')).version],
  ['construct/package.json', JSON.parse(read('construct/package.json')).version],
  ['proxy/Cargo.toml', capture(cargoPackage, /^version = "([^"]+)"$/m)],
  ['Cargo.lock (zankyo)', capture(read('Cargo.lock'), /^name = "zankyo"\nversion = "([^"]+)"$/m)],
  ['sar/template.yaml SemanticVersion', sarMetadata('SemanticVersion')],
];

const problems: string[] = [];
const version = versions[0]?.[1];
if (version === undefined || versions.some(([, v]) => v !== version)) {
  problems.push(
    'versions differ:',
    ...versions.map(([source, v]) => `  ${source}: ${v ?? '(not found)'}`),
  );
} else {
  const sourceCodeUrl = `${sarMetadata('HomePageUrl')}/tree/v${version}`;
  if (sarMetadata('SourceCodeUrl') !== sourceCodeUrl) {
    problems.push(`sar/template.yaml SourceCodeUrl is not ${sourceCodeUrl}`);
  }
  if (!read('CHANGELOG.md').includes(`\n## [${version}] - `)) {
    problems.push(`CHANGELOG.md has no "## [${version}] - <date>" heading`);
  }
}

if (problems.length > 0) {
  console.error(problems.join('\n'));
  process.exit(1);
}
console.log(`versions agree: ${version}`);
