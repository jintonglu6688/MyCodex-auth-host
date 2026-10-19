// Export the pinned upstream's TypeScript data without installing frontend dependencies.
import { readFileSync, writeFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import { createHash } from 'node:crypto';
import { runInNewContext } from 'node:vm';

const sha256 = (text) => createHash('sha256').update(text).digest('hex');
// Imports in these data-only modules are exclusively TypeScript types.
const evaluate = (source, name) => {
  const executable = stripTypeScriptTypes(source)
    .replace(/^import[\s\S]*?;\s*/gm, '')
    .replace(/^export /gm, '');
  return JSON.parse(runInNewContext(`${executable}\nJSON.stringify(${name})`, {}, { timeout: 1000 }));
};

const source = readFileSync(new URL('../src/config/codexProviderPresets.ts', import.meta.url), 'utf8');
const claudeSource = readFileSync(new URL('../src/config/claudeProviderPresets.ts', import.meta.url), 'utf8');
const presets = evaluate(source, 'codexProviderPresets');
// Upstream keeps a provider's Anthropic-compatible endpoint only in its Claude
// preset of the same name; Claude presets that need format conversion are not
// Anthropic endpoints.
const codexNames = new Set(presets.map((preset) => preset.name));
const anthropicBaseUrls = Object.fromEntries(evaluate(claudeSource, 'providerPresets')
  .filter((preset) => codexNames.has(preset.name) && (preset.apiFormat ?? 'anthropic') === 'anthropic')
  .map((preset) => [preset.name, (preset.settingsConfig?.env?.ANTHROPIC_BASE_URL ?? '').trim().replace(/\/+$/, '')])
  .filter(([, url]) => url !== ''));
const output = JSON.stringify({
  source: 'src/config/codexProviderPresets.ts',
  sourceSha256: sha256(source),
  claudeSource: 'src/config/claudeProviderPresets.ts',
  claudeSourceSha256: sha256(claudeSource),
  presets,
  anthropicBaseUrls,
}, null, 2) + '\n';
const target = new URL('../src-tauri/src/mycodex_host/gui_presets.json', import.meta.url);
if (process.argv.includes('--check')) {
  if (readFileSync(target, 'utf8') !== output) throw new Error('Preset export is stale; rerun this script.');
} else {
  writeFileSync(target, output, 'utf8');
}
