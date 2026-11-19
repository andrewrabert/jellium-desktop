import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const shimSource = readFileSync(new URL('../src/web/native-shim.js', import.meta.url), 'utf8');
const SERVER_A = 'https://server-a.example.org';
const SERVER_B = 'https://server-b.example.org';

/** @param {string} savedUrl */
function fillPlaceholders(savedUrl) {
    // Match the first-occurrence replacement semantics in app.rs::run_user_scripts.
    /** @type {ReadonlyArray<readonly [string, string]>} */
    const values = [
        ['__SERVER_URL__', savedUrl],
        ['__SETTINGS_JSON__', '{}'],
        ['__APP_VERSION__', 'test'],
        ['__WINDOW_DECORATION_OPTIONS__', '[]'],
        ['__DEVICE_PROFILE_JSON__', '{}'],
        ['__WINDOW_DECORATIONS__', 'null'],
    ];
    let code = shimSource;
    for (const [placeholder, value] of values) {
        code = code.replace(placeholder, value);
    }
    assert.doesNotMatch(code, /__[A-Z_]+__/);
    return code;
}

/**
 * @param {vm.Context} context
 * @param {string} code
 * @returns {unknown}
 */
function run(context, code) {
    return vm.runInContext(code, context);
}

/** @param {string} savedUrl */
function realm(savedUrl) {
    const context = vm.createContext({});
    run(
        context,
        `
        globalThis.window = globalThis;
        globalThis.navigator = { platform: 'Win32', userAgent: 'test' };
        globalThis.document = { addEventListener() {}, querySelector() { return null; } };
        globalThis.errors = [];
        globalThis.console = {
            debug() {}, log() {}, info() {}, warn() {},
            error(...args) { errors.push(args); },
        };
        globalThis.saved = [];
        window.jmpNative = { saveServerUrl(url) { saved.push(url); } };
        globalThis.client = (id, address) => ({
            serverId() { return id; },
            serverAddress() { return address; },
        });
        globalThis.signIn = (apiClient, serverId) => {
            window.ApiClient = apiClient;
            return window.NativeShell.onLocalUserSignedIn({ ServerId: serverId }, 'token');
        };
    `,
    );
    vm.runInContext(fillPlaceholders(savedUrl), context, { filename: 'native-shim.js' });
    return context;
}

/**
 * @param {vm.Context} context
 * @param {string} code
 */
async function signIn(context, code) {
    const result = run(context, `globalThis.signInResult = ${code}`);
    assert.equal(run(context, 'signInResult instanceof Promise'), true);
    assert.equal(await result, undefined);
}

/**
 * @param {vm.Context} context
 * @param {string[]} expected
 */
function expectSaved(context, expected) {
    assert.equal(run(context, 'JSON.stringify(saved)'), JSON.stringify(expected));
}

test('a successful sign-in on another server saves its address', async () => {
    const context = realm(SERVER_A);
    await signIn(context, `signIn(client('b', '${SERVER_B}'), 'b')`);
    expectSaved(context, [SERVER_B]);
});

test('the loaded server is not saved again, with or without trailing slashes', async () => {
    const context = realm(SERVER_A);
    await signIn(context, `signIn(client('a', '${SERVER_A}/'), 'a')`);
    await signIn(context, `signIn(client('a', '${SERVER_A}'), 'a')`);
    expectSaved(context, []);
});

test('each switch saves once and switching back saves the original server', async () => {
    const context = realm(SERVER_A);
    await signIn(context, `signIn(client('b', '${SERVER_B}'), 'b')`);
    await signIn(context, `signIn(client('b', '${SERVER_B}/'), 'b')`);
    await signIn(context, `signIn(client('a', '${SERVER_A}'), 'a')`);
    expectSaved(context, [SERVER_B, SERVER_A]);
});

test('a client that does not belong to the signed-in user is ignored', async () => {
    const context = realm(SERVER_A);
    await signIn(context, `signIn(client('a', '${SERVER_B}'), 'b')`);
    expectSaved(context, []);
});

test('a missing user, client or address does not save a server', async () => {
    const context = realm(SERVER_A);
    await signIn(context, 'window.NativeShell.onLocalUserSignedIn(null)');
    await signIn(context, "signIn(undefined, 'b')");
    await signIn(context, "signIn(client('b', ''), 'b')");
    expectSaved(context, []);
});

test('a missing native bridge does not reject the sign-in', async () => {
    const context = realm(SERVER_A);
    run(context, 'delete window.jmpNative');
    await signIn(context, `signIn(client('b', '${SERVER_B}'), 'b')`);
    expectSaved(context, []);
});

test('a missing save function does not reject the sign-in', async () => {
    const context = realm(SERVER_A);
    run(context, 'delete window.jmpNative.saveServerUrl');
    await signIn(context, `signIn(client('b', '${SERVER_B}'), 'b')`);
    expectSaved(context, []);
});

test('a throwing client is logged and does not reject the sign-in', async () => {
    const context = realm(SERVER_A);
    await signIn(
        context,
        `signIn({ serverId() { return 'b'; }, serverAddress() { throw new Error('test'); } }, 'b')`,
    );
    expectSaved(context, []);
    assert.equal(run(context, 'errors.length'), 1);
});

test('a failed native save is logged and a later sign-in can retry', async () => {
    const context = realm(SERVER_A);
    run(context, "window.jmpNative.saveServerUrl = () => { throw new Error('test'); }");
    await signIn(context, `signIn(client('b', '${SERVER_B}'), 'b')`);
    expectSaved(context, []);
    assert.equal(run(context, 'errors.length'), 1);
    run(context, 'window.jmpNative.saveServerUrl = url => saved.push(url)');
    await signIn(context, `signIn(client('b', '${SERVER_B}'), 'b')`);
    expectSaved(context, [SERVER_B]);
});
