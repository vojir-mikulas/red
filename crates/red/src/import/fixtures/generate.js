// Generates known-answer crypto fixtures for the connection-import tests.
// Uses node's crypto, which is byte-for-byte what DBeaver (AES-128-CBC) and
// DBGate's `simple-encryptor` (AES-256-CBC + SHA-256 key + HMAC-SHA256) produce.
// Fixed IVs keep the checked-in fixtures deterministic across regeneration.
const crypto = require('crypto');
// Regenerate:  node crates/red/src/import/fixtures/generate.js crates/red/src/import/fixtures
const fs = require('fs');
const path = require('path');

const ROOT = process.argv[2] || __dirname;
const DBEAVER = path.join(ROOT, 'dbeaver');
const DBGATE = path.join(ROOT, 'dbgate');

// --- DBGate simple-encryptor (faithful reimplementation of node-simple-encryptor) ---
function simpleEncrypt(key, value) {
  const cryptoKey = crypto.createHash('sha256').update(key).digest();
  const iv = Buffer.alloc(16, 7);
  const cipher = crypto.createCipheriv('aes-256-cbc', cryptoKey, iv);
  const json = JSON.stringify(value);
  const ct = cipher.update(json, 'utf8', 'base64') + cipher.final('base64');
  const rest = iv.toString('hex') + ct;
  const mac = crypto.createHmac('sha256', cryptoKey).update(rest).digest('hex');
  return mac + rest;
}

// --- DBeaver AES-128-CBC credentials blob: [IV(16)][ciphertext], raw binary ---
function dbeaverEncrypt(obj) {
  const key = Buffer.from('babb4a9f774ab853c96c2d653dfe544a', 'hex');
  const iv = Buffer.alloc(16, 3);
  const cipher = crypto.createCipheriv('aes-128-cbc', key, iv);
  const enc = Buffer.concat([cipher.update(JSON.stringify(obj), 'utf8'), cipher.final()]);
  return Buffer.concat([iv, enc]);
}

// ===== DBeaver fixtures =====
const dbeaverCreds = {
  'postgres-jdbc-1': {
    '#connection': { user: 'appuser', password: 's3cret' },
    'network/ssh_tunnel': { user: 'ec2-user', password: 'keypass' },
  },
  'mysql8-1': { '#connection': { user: 'root', password: 'rootpw' } },
};
const dbeaverDataSources = {
  connections: {
    'postgres-jdbc-1': {
      provider: 'postgresql', driver: 'postgres-jdbc', name: 'PostgreSQL - prod',
      'save-password': true, 'read-only': true, folder: 'PG',
      configuration: {
        host: 'db.example.com', port: '5432', database: 'app',
        url: 'jdbc:postgresql://db.example.com:5432/app',
        handlers: {
          ssh_tunnel: {
            type: 'tunnel', enabled: true, user: 'ec2-user',
            properties: { host: 'bastion.example.com', port: '2222', authType: 'PUBLIC_KEY', keyPath: '/home/me/.ssh/id_ed25519' },
          },
        },
      },
    },
    'mysql8-1': {
      provider: 'mysql', driver: 'mariaDB', name: 'MariaDB local', 'save-password': true,
      configuration: { host: '127.0.0.1', port: '', database: 'shop' },
    },
    'sqlite_jdbc-1': {
      provider: 'sqlite', driver: 'sqlite_jdbc', name: 'Local SQLite',
      configuration: { url: 'jdbc:sqlite:/data/app.db' },
    },
    'mssql-1': {
      provider: 'sqlserver', driver: 'mssql', name: 'SQL Server',
      configuration: { host: 'win.example.com', port: '1433' },
    },
  },
};
fs.writeFileSync(path.join(DBEAVER, 'data-sources.json'), JSON.stringify(dbeaverDataSources, null, 2));
fs.writeFileSync(path.join(DBEAVER, 'credentials-config.json'), dbeaverEncrypt(dbeaverCreds));

// ===== DBGate fixtures =====
const encryptionKey = '1122334455667788990011223344556677889900112233445566778899001122';
fs.writeFileSync(path.join(DBGATE, '.key'), simpleEncrypt('mQAUaXhavRGJDxDTXSCg7Ej0xMmGCrx6OKA07DIMBiDcYYkvkaXjTAzPUEHEHEf9', { encryptionKey }));
const enc = (v) => 'crypt:' + simpleEncrypt(encryptionKey, v);

const lines = [
  {
    _id: 'pg1', engine: 'postgres@dbgate-plugin-postgres', displayName: 'Prod Postgres',
    server: 'db.example.com', port: '5432', user: 'app_user', password: enc('pgsecret'),
    passwordMode: 'saveEncrypted', defaultDatabase: 'appdb',
    useSshTunnel: true, sshHost: 'bastion.example.com', sshPort: '2222', sshLogin: 'ec2-user',
    sshMode: 'keyFile', sshKeyfile: '/home/me/.ssh/id_rsa', sshKeyfilePassword: enc('keyphrase'),
  },
  { _id: 'my1', engine: 'mariadb@dbgate-plugin-mysql', displayName: 'Maria', server: '127.0.0.1', port: 3306, user: 'root', password: enc('mariapw'), passwordMode: 'saveEncrypted', defaultDatabase: 'shop' },
  { _id: 'raw1', engine: 'postgres@dbgate-plugin-postgres', displayName: 'Raw Pw', server: 'h', port: 5432, user: 'u', password: 'plainpw', passwordMode: 'saveRaw' },
  { _id: 'lite1', engine: 'sqlite@dbgate-plugin-sqlite', displayName: 'Local SQLite', databaseFile: '/data/app.sqlite' },
  { _id: 'ms1', engine: 'mssql@dbgate-plugin-mssql', displayName: 'SQL Server', server: 'win', port: 1433 },
  { _id: 'folder1', folder: 'Group A' },
];
fs.writeFileSync(path.join(DBGATE, 'connections.jsonl'), lines.map((l) => JSON.stringify(l)).join('\n') + '\n');

console.log('fixtures written to', ROOT);
