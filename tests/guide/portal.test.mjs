import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { test } from 'node:test';

const context = {};
runInNewContext(readFileSync(new URL('../../src/portal/route-groups.js', import.meta.url), 'utf8'), context);
const group = (routes) => JSON.parse(JSON.stringify(context.routeGroups(routes)));
const route = (alias, effort) => ({ model_alias: alias, display_alias: 'gpt-6-luna', operation: 'chat', exposure: 'never_public', reasoning_effort: effort });

test('one model groups ordered reasoning levels while preserving exact scope indices', () => {
  const groups = group([route('gpt-6-luna', 'medium'), route('gpt-6-luna:high', 'high'), route('gpt-6-luna:low', 'low'), route('gpt-6-luna:medium', 'medium')]);
  assert.equal(groups.length, 1);
  assert.equal(groups[0].alias, 'gpt-6-luna');
  assert.deepEqual(groups[0].levels, [
    { effort: 'low', indices: [2] },
    { effort: 'medium', indices: [0, 3] },
    { effort: 'high', indices: [1] },
  ]);
});

test('ordinary routes retain separate operation scopes and are never inferred from their name', () => {
  const routes = [
    { model_alias: 'chatgpt-luna', operation: 'chat', reasoning_effort: null },
    { model_alias: 'luna:low', operation: 'chat', reasoning_effort: null },
    { model_alias: 'luna', operation: 'transcription', reasoning_effort: null },
  ];
  assert.deepEqual(group(routes).map((group) => group.indices), [[0], [1], [2]]);
});

test('provider sections use configured provider labels and preserve original scope indices', () => {
  const routes = [
    {...route('gpt-6-luna:low', 'low'), provider: 'codex'},
    {model_alias: 'chatgpt-luna', provider: 'chatgpt', operation: 'chat'},
    {...route('gpt-6-luna:high', 'high'), provider: 'codex'},
    {model_alias: 'chatgpt-looking-alias', provider: 'ollama', operation: 'chat'},
    {...route('gpt-6-luna:low', 'low'), provider: 'chatgpt'},
  ];
  const groups = JSON.parse(JSON.stringify(context.providerGroups(routes)));
  assert.deepEqual(groups.map(x => x.label), ['ChatGPT', 'Codex', 'Ollama']);
  assert.deepEqual(groups[0].models.map(x => x.indices), [[1], [4]]);
  assert.deepEqual(groups[1].models[0].indices, [0, 2]);
  assert.deepEqual(groups[2].models[0].indices, [3]);
});
