const assert = require('node:assert/strict');
const test = require('node:test');

require('./server-url.js');

test('suggests the Jellyfin default URL for host-only input', () => {
    assert.equal(jmpDefaultServerUrlForHostOnly('jellyfin.local'), 'http://jellyfin.local:8096');
    assert.equal(jmpDefaultServerUrlForHostOnly('  192.168.1.50  '), 'http://192.168.1.50:8096');
    assert.equal(jmpDefaultServerUrlForHostOnly('::1'), 'http://[::1]:8096');
});

test('does not suggest a URL when connection details were supplied', () => {
    assert.equal(jmpDefaultServerUrlForHostOnly('http://jellyfin.local'), null);
    assert.equal(jmpDefaultServerUrlForHostOnly('jellyfin.local:8096'), null);
    assert.equal(jmpDefaultServerUrlForHostOnly('jellyfin.local/web'), null);
    assert.equal(jmpDefaultServerUrlForHostOnly(''), null);
});
