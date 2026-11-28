#!/usr/bin/env node
// Check an updater archive the way an installed app will, then write the
// latest.json that is uploaded to the GitHub Release next to it.
//
//   node scripts/updater-manifest.js <archive> <version>                  verify only
//   node scripts/updater-manifest.js <archive> <version> --out latest.json [--notes text] [--url url]
//                                    [--target darwin-x86_64] [--merge latest.json]
//
// --target names the platform the archive is for (default darwin-aarch64); --merge
// keeps the other platforms of an existing latest.json for the same version, so
// one manifest serves Apple silicon and Intel.
//
// The signature is read from <archive>.sig and checked against the public key
// in src-tauri/tauri.conf.json (or --pubkey): the same key, the same minisign
// check and the same "signed for this version" rule the app applies. The Tauri
// CLI only warns when the private key does not belong to that public key; here
// it is an error, because such a release is refused by every installed app.
'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const REPOSITORY = 'https://github.com/sint908/sintbot-desktop';
const TARGETS = ['darwin-aarch64', 'darwin-x86_64'];
const ED25519_SPKI = Buffer.from('302a300506032b6570032100', 'hex');

function fail(message) { throw new Error(message); }

// Tauri stores both as base64 of the minisign text file: a comment line, then the key or signature.
function lines(encoded, what) {
  const text = Buffer.from(String(encoded).trim(), 'base64').toString('utf8');
  if (!text.startsWith('untrusted comment:')) fail(`${what} is not a base64-encoded minisign ${what}`);
  return text.split('\n');
}

function publicKey(encoded) {
  const raw = Buffer.from(lines(encoded, 'public key')[1] || '', 'base64');
  if (raw.length !== 42 || raw.subarray(0, 2).toString() !== 'Ed') fail('the public key is not a minisign Ed25519 key');
  return { id: raw.subarray(2, 10).toString('hex'), key: raw.subarray(10) };
}

function signature(encoded) {
  const [, body, comment, global] = lines(encoded, 'signature');
  const raw = Buffer.from(body || '', 'base64');
  const algorithm = raw.subarray(0, 2).toString();
  if (raw.length !== 74 || !['Ed', 'ED'].includes(algorithm)) fail('the signature is not a minisign Ed25519 signature');
  if (!comment || !comment.startsWith('trusted comment: ')) fail('the signature has no trusted comment');
  const trusted = comment.slice('trusted comment: '.length);
  const version = trusted.split('\t').map(field => field.match(/^version:(.*)$/)).find(Boolean);
  return { prehashed: algorithm === 'ED', id: raw.subarray(2, 10).toString('hex'), bytes: raw.subarray(10), trusted,
    global: Buffer.from(global || '', 'base64'), version: version ? version[1] : null };
}

// Throws unless `data` is what `encodedSignature` signs, with the given key, for `version`.
function verify(data, encodedSignature, encodedKey, version) {
  const key = publicKey(encodedKey);
  const sig = signature(encodedSignature);
  if (sig.id !== key.id) fail(`signed with key ${sig.id}, but the app embeds key ${key.id}: installed apps would refuse this update`);
  const spki = crypto.createPublicKey({ key: Buffer.concat([ED25519_SPKI, key.key]), format: 'der', type: 'spki' });
  const message = sig.prehashed ? crypto.createHash('blake2b512').update(data).digest() : data;
  if (!crypto.verify(null, message, spki, sig.bytes)) fail('the signature does not match the file');
  // The trusted comment, which carries the version, is covered by a second signature.
  if (!crypto.verify(null, Buffer.concat([sig.bytes, Buffer.from(sig.trusted)]), spki, sig.global)) fail('the trusted comment was altered');
  if (sig.version === null) fail('the signature is not bound to a version (sign with --app-version); the app requires it');
  if (sig.version.replace(/^v/, '') !== String(version).replace(/^v/, '')) fail(`signed for version ${sig.version}, not ${version}`);
  return { keyId: key.id, version: sig.version };
}

function manifest({ version, signature: encodedSignature, url, notes, target = TARGETS[0], merge, date = new Date() }) {
  if (!TARGETS.includes(target)) fail(`not a supported platform: ${target}`);
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) fail(`not a version: ${version}`);
  if (!/^https:\/\//.test(url) && !/^http:\/\/(127\.0\.0\.1|localhost|\[::1\])[:/]/.test(url)) fail(`the archive URL must be https: ${url}`);
  const platforms = {};
  if (merge) {
    if (merge.version !== version) fail(`cannot merge latest.json for ${merge.version} into ${version}`);
    for (const [name, entry] of Object.entries(merge.platforms || {})) if (name !== target) platforms[name] = entry;
  }
  platforms[target] = { signature: String(encodedSignature).trim(), url };
  return { version, notes: notes || (merge && merge.notes) || `SintBot for macOS ${version}`,
    pub_date: (merge && merge.pub_date) || date.toISOString().replace(/\.\d+Z$/, 'Z'), platforms };
}

function releaseUrl(version, archive) {
  return `${REPOSITORY}/releases/download/v${version}/${encodeURIComponent(path.basename(archive))}`;
}

function main(argv) {
  const [archive, version, ...rest] = argv;
  const options = {};
  for (let i = 0; i < rest.length; i += 2) {
    const name = rest[i].replace(/^--/, '');
    if (!['out', 'notes', 'url', 'pubkey', 'target', 'merge'].includes(name) || rest[i + 1] === undefined) fail(`unknown or incomplete option: ${rest[i]}`);
    options[name] = rest[i + 1];
  }
  if (!archive || !version) fail('usage: updater-manifest.js <archive> <version> [--out latest.json] [--notes text] [--url url] [--pubkey key]');
  const config = require(path.join(__dirname, '..', 'src-tauri', 'tauri.conf.json'));
  const key = options.pubkey || (config.plugins && config.plugins.updater && config.plugins.updater.pubkey) || '';
  if (!key.trim()) fail('no updater public key in src-tauri/tauri.conf.json; run scripts/updater-keygen.sh on the release Mac');
  const encodedSignature = fs.readFileSync(`${archive}.sig`, 'utf8');
  const checked = verify(fs.readFileSync(archive), encodedSignature, key, version);
  console.log(`Signature of ${path.basename(archive)} verifies: key ${checked.keyId}, version ${checked.version}`);
  if (!options.out) return;
  const merge = options.merge ? JSON.parse(fs.readFileSync(options.merge, 'utf8')) : undefined;
  const target = options.target || TARGETS[0];
  const document = manifest({ version, signature: encodedSignature, url: options.url || releaseUrl(version, archive), notes: options.notes, target, merge });
  fs.writeFileSync(options.out, `${JSON.stringify(document, null, 2)}\n`);
  console.log(`Wrote ${options.out}: ${Object.keys(document.platforms).join(', ')}`);
}

module.exports = { verify, manifest, releaseUrl };

if (require.main === module) {
  try { main(process.argv.slice(2)); } catch (error) { console.error(`updater-manifest: ${error.message}`); process.exit(1); }
}
