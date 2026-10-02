import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

const workflow = JSON.parse(fs.readFileSync(new URL('../n8n/examples/autopilot-fan-lifecycle.example.json', import.meta.url)));
const render = workflow.nodes.find(n => n.id === 'fl-render').parameters.jsCode;
const validate = workflow.nodes.find(n => n.id === 'fl-validate').parameters.jsCode;
const plan = { template_key: 'crowdrelay.fan.welcome.v2', locale: 'pl', wordmark: 'Tenant', display_name: '' };
function run(p) { return vm.runInNewContext(`(function(){${render}})()`, { $input: { first: () => ({ json: p }) } })[0].json; }

test('the promise survives validation, and the event CTA is sent verbatim once', () => {
  const activation = { kind: 'event', title: 'Real show', url: 'https://tenant.test/l/welcome-action' };
  const event = { type: 'crowdrelay.fan_lifecycle.message_requested', version: 1, data: {
    action_id: 'action', fan_id: 'fan', template_key: plan.template_key,
    brand: { wordmark: 'Tenant' }, fan: { email: 'fan@example.test', locale: 'pl', activation },
  } };
  const p = vm.runInNewContext(`(function(){${validate}})()`, { $json: event })[0].json;
  const rendered = run(p);
  assert.equal(p.activation.url, activation.url);
  assert.ok(rendered.text.includes('Real show'));
  assert.equal(rendered.text.split(activation.url).length - 1, 1);
  assert.ok(!rendered.text.includes('Signal'));
  assert.ok(!rendered.text.includes('polec'));
});

test('video copy offers the actual resource without an install or referral demand', () => {
  const r = run({ ...plan, locale: 'en', activation: { kind: 'video', title: 'Real video', url: 'https://tenant.test/l/welcome-video' } });
  assert.ok(r.text.includes('Start with this video:'));
  assert.ok(r.text.includes('Real video'));
  assert.ok(!r.text.includes('download'));
});

test('a missing public resource yields an honest welcome without invented links', () => {
  const r = run(plan);
  assert.ok(r.text.includes('Dzięki'));
  assert.ok(!r.text.includes('http'));
  assert.ok(!r.text.includes('nagrod'));
});

test('v1 pending requests preserve their original words', () => {
  const r = run({ ...plan, template_key: 'crowdrelay.fan.welcome.v1' });
  assert.equal(r.text, 'Cześć,\n\nDziękujemy za dołączenie. Od teraz koncerty, wiadomości i polecenia są w jednym miejscu.\n\n- Tenant');
});

test('malformed activation or unknown templates fail closed', () => {
  for (const activation of [ { kind: 'prize', title: 'Prize', url: 'https://tenant.test/l/x' },
    { kind: 'video', title: '', url: 'https://tenant.test/l/x' },
    { kind: 'event', title: 'Show', url: 'javascript:alert(1)' } ]) {
    assert.throws(() => run({ ...plan, activation }), /invalid welcome activation/);
  }
  assert.throws(() => run({ ...plan, template_key: 'crowdrelay.fan.welcome.v3' }), /Unknown lifecycle/);
});

test('the upgraded workflow advertises its exact new capability', () => {
  assert.ok(workflow.nodes.find(n => n.id === 'fl-claim').parameters.body.includes("'fan.lifecycle.welcome.v2'"));
});


test('provider claim and reports use valid n8n expression URLs', () => {
  for (const n of workflow.nodes.filter(n => n.type === 'n8n-nodes-base.httpRequest')) {
    assert.ok(n.parameters.url.startsWith('={{'), n.name);
    assert.ok(!n.parameters.url.startsWith('=={{'), n.name);
  }
});
