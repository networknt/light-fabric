import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';

const root = path.dirname(fileURLToPath(import.meta.url));
const read = name => JSON.parse(fs.readFileSync(path.join(root, name), 'utf8'));
const manifest = read('tool-manifest.json');
const schemas = read('schemas.json');
const publication = read('gateway-publication.json');
const selected = new Set(publication.tools.map(tool => tool.name));

function resolve(value) {
  if (Array.isArray(value)) return value.map(resolve);
  if (value && typeof value === 'object') {
    if (typeof value.$ref === 'string') {
      const pointer = value.$ref.replace(/^schemas\.json#/, '#');
      if (!pointer.startsWith('#/')) throw new Error(`external schema reference: ${value.$ref}`);
      const target = pointer.slice(2).split('/').reduce((part, key) => part?.[key.replaceAll('~1','/').replaceAll('~0','~')], schemas);
      if (!target) throw new Error(`unresolved schema reference: ${value.$ref}`);
      return resolve(target);
    }
    return Object.fromEntries(Object.entries(value).map(([key, child]) => [key, resolve(child)]));
  }
  return value;
}

const tools = manifest.tools.filter(tool => selected.has(tool.name)).map(tool => ({
  name: tool.name,
  description: tool.description,
  inputSchema: resolve(tool.inputSchema),
  outputSchema: resolve(tool.outputSchema),
  annotations: {
    readOnlyHint: !tool.sideEffect,
    destructiveHint: tool.sideEffect,
    permission: tool.permission,
  },
}));
if (tools.length !== selected.size) throw new Error('Gateway publication references an absent Tool');
const output = path.join(root, 'gateway-tools-list.json');
fs.writeFileSync(output, `${JSON.stringify({tools}, null, 2)}\n`);
console.log(`Generated ${output} with ${tools.length} native Tools`);
