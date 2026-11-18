// node --test scripts/updater-manifest.test.js
'use strict';
const assert = require('node:assert');
const { test } = require('node:test');
const { verify, manifest, releaseUrl } = require('./updater-manifest.js');

// Made with `tauri signer generate` and `tauri signer sign --app-version 0.1.4`
// from a key pair that was deleted afterwards; it signs nothing but this text.
const DATA = Buffer.from('sintbot updater fixture\n');
const PUBKEY = 'dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDhBNzc5MkRDQ0Q3QjQ5MkEKUldRcVNYdk4zSkozaXEzTlA1WTFFYmZ4cFZlLzEzQzNFYzJIUEVZVlhhMkRWYUEvNFcvdTFaK0kK';
const SIGNATURE = 'dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRcVNYdk4zSkozaWsyWkRvWHhIL0FnZlc0YUE4aXNmbVV4MW00N0psUytOb0tBSnJKOWxDTzdiaVJuNjkvV0ZubHR5TnlvajZGYk90VHp6NjJPOGtSenMxb2JhWkQxOWdNPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk0OTg4ODY5CWZpbGU6Zml4dHVyZS5iaW4JdmVyc2lvbjowLjEuNApzU0hwVGs2ck1wY0M3eWlXN2xJNVQ3RzQ0NWwzZUxYTDVIUE1zNndyRmJpNHVVN0w1cHlTWVVHM3lvSEZvT0E3eWI2c1FZUFZpaUJJVTNoZnBsZjZCZz09Cg==';
// The same key under another key id, standing in for a key the app does not embed.
const OTHER_PUBKEY = 'dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDhBNzc5MkRDQ0Q3QjQ5MkIKUldRclNYdk4zSkozaXEzTlA1WTFFYmZ4cFZlLzEzQzNFYzJIUEVZVlhhMkRWYUEvNFcvdTFaK0kK';

const edit = (encoded, change) => Buffer.from(change(Buffer.from(encoded, 'base64').toString('utf8'))).toString('base64');

test('a signature made by the Tauri CLI verifies', () => {
  assert.deepStrictEqual(verify(DATA, SIGNATURE, PUBKEY, '0.1.4'), { keyId: '2a497bcddc92778a', version: '0.1.4' });
  assert.strictEqual(verify(DATA, SIGNATURE, PUBKEY, 'v0.1.4').version, '0.1.4');
});

test('one changed byte in the archive is refused', () => {
  const tampered = Buffer.from(DATA);
  tampered[3] ^= 1;
  assert.throws(() => verify(tampered, SIGNATURE, PUBKEY, '0.1.4'), /does not match the file/);
});

test('a signature for another version is refused, also when the comment is rewritten', () => {
  assert.throws(() => verify(DATA, SIGNATURE, PUBKEY, '0.1.5'), /signed for version 0\.1\.4, not 0\.1\.5/);
  const relabelled = edit(SIGNATURE, text => text.replace('version:0.1.4', 'version:0.1.5'));
  assert.throws(() => verify(DATA, relabelled, PUBKEY, '0.1.5'), /trusted comment was altered/);
});

test('a signature without a version is refused', () => {
  const unbound = edit(SIGNATURE, text => text.replace('\tversion:0.1.4', ''));
  assert.throws(() => verify(DATA, unbound, PUBKEY, '0.1.4'), /trusted comment was altered|not bound to a version/);
});

test('a signature from a key the app does not embed is refused', () => {
  assert.throws(() => verify(DATA, SIGNATURE, OTHER_PUBKEY, '0.1.4'), /installed apps would refuse/);
  assert.throws(() => verify(DATA, SIGNATURE, '', '0.1.4'), /public key/);
});

test('latest.json has what the updater reads', () => {
  const url = releaseUrl('0.1.4', '/tmp/out/SintBot_0.1.4_aarch64.app.tar.gz');
  assert.strictEqual(url, 'https://github.com/sintlab/sintbot-desktop/releases/download/v0.1.4/SintBot_0.1.4_aarch64.app.tar.gz');
  const document = manifest({ version: '0.1.4', signature: `${SIGNATURE}\n`, url, date: new Date('2026-11-18T03:04:05.678Z') });
  assert.deepStrictEqual(document, {
    version: '0.1.4', notes: 'SintBot for macOS 0.1.4', pub_date: '2026-11-18T03:04:05Z',
    platforms: { 'darwin-aarch64': { signature: SIGNATURE, url } },
  });
  assert.strictEqual(manifest({ version: '0.1.4', signature: SIGNATURE, url, notes: 'Fixes' }).notes, 'Fixes');
});

test('latest.json refuses a plain http archive and a malformed version', () => {
  assert.throws(() => manifest({ version: '0.1.4', signature: SIGNATURE, url: 'http://example.com/a.tar.gz' }), /must be https/);
  assert.throws(() => manifest({ version: 'latest', signature: SIGNATURE, url: 'https://example.com/a.tar.gz' }), /not a version/);
  assert.ok(manifest({ version: '0.1.4', signature: SIGNATURE, url: 'http://127.0.0.1:8765/a.tar.gz' }));
});
