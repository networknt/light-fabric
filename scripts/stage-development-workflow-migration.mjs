// Regenerate the existing development bundle from canonical SQL. No database writes.
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import {fileURLToPath} from 'node:url';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const canonical = path.join(root, 'crates/operational-store/release/bundle');
const manifest = JSON.parse(fs.readFileSync(path.join(canonical, 'manifest.json'), 'utf8'));
for (const [owner, schema, migrationId] of [
  ['workflow-store', 'workflow_ops', '0008_development_workflow'],
  ['workflow-store', 'workflow_ops', '0009_workflow_agent_dispatch'],
  ['agent-store', 'agent_ops', '0004_workflow_job_transport'],
  ['workflow-store', 'workflow_ops', '0010_workflow_action_runtime_privileges'],
  ['agent-store', 'agent_ops', '0005_workflow_job_owner'],
]) {
  const migrationPath = `crates/${owner}/migrations/${schema.replace('_ops', '-postgres')}/${migrationId}.sql`;
  if (!manifest.orderedMigrations.some(m => m.migrationId === migrationId && m.owner === owner)) {
    manifest.orderedMigrations.push({order: manifest.orderedMigrations.length + 1,
      owner, schema, migrationId, path: migrationPath});
  }
}
const digest = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
for (const migration of manifest.orderedMigrations) {
  const bytes = fs.readFileSync(path.join(root, migration.path));
  migration.sha256 = digest(bytes);
}
const targets = [canonical, ...process.argv.slice(2).map(p => path.resolve(p))];
for (const target of targets) {
  if (!fs.existsSync(path.join(target, 'manifest.json'))) throw new Error(`Not a bundle: ${target}`);
  const files = new Map();
  files.set('manifest.json', Buffer.from(JSON.stringify(manifest, null, 2) + '\n'));
  files.set('migration-order.tsv', Buffer.from('# order\towner\tschema\tmigration_id\tpath\tsha256\n' +
    manifest.orderedMigrations.map(m => [m.order,m.owner,m.schema,m.migrationId,m.path,m.sha256].join('\t')).join('\n') + '\n'));
  for (const migration of manifest.orderedMigrations) files.set(migration.path, fs.readFileSync(path.join(root, migration.path)));
  for (const [name, bytes] of files) {
    fs.mkdirSync(path.dirname(path.join(target, name)), {recursive:true});
    fs.writeFileSync(path.join(target, name), bytes);
  }
  fs.writeFileSync(path.join(target, 'bundle.sha256'), [...files].map(([name, bytes]) => `${digest(bytes)}  ${name}\n`).join(''));
  console.log(`Staged ${manifest.orderedMigrations.length} migrations: ${target}`);
}
