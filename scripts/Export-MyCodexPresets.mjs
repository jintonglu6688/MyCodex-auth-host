// Export the pinned upstream's TypeScript data without installing frontend dependencies.
import { readFileSync, writeFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import { createHash } from 'node:crypto';
import { runInNewContext } from 'node:vm';

const source = readFileSync(new URL('../src/config/codexProviderPresets.ts', import.meta.url), 'utf8');
// Imports in this data-only module are exclusively TypeScript types.
const executable = stripTypeScriptTypes(source)
  .replace(/^import[\s\S]*?;\s*/gm, '')
  .replace(/^export /gm, '');
const presets = runInNewContext(`${executable}\nJSON.stringify(codexProviderPresets)`, {}, { timeout: 1000 });
const output = JSON.stringify({
  source: 'src/config/codexProviderPresets.ts',
  sourceSha256: createHash('sha256').update(source).digest('hex'),
  presets: JSON.parse(presets),
}, null, 2) + '\n';
const target = new URL('../src-tauri/src/mycodex_host/gui_presets.json', import.meta.url);
if (process.argv.includes('--check')) {
  if (readFileSync(target, 'utf8') !== output) throw new Error('Preset export is stale; rerun this script.');
} else {
  writeFileSync(target, output, 'utf8');
}
