import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const __dirname = dirname(fileURLToPath(import.meta.url));

function escapedJsonForSingleQuotedString(value) {
    return JSON.stringify(value).replace(/\\/g, '\\\\').replace(/'/g, "\\'");
}

function loadNativeShim() {
    const settings = {
        deviceNameDefault: 'test-device',
        hwdecOptions: [ 'auto', 'no' ]
    };
    const pushedUrls = [];
    const replacedUrls = [];
    let serverSwitcherOpenCount = 0;

    let source = readFileSync(join(__dirname, 'native-shim.js'), 'utf8');
    source = source
        .replace('__SETTINGS_JSON__', escapedJsonForSingleQuotedString(settings))
        .replace('__APP_VERSION__', '0.1.0-test')
        .replace('__SERVER_URL__', 'http://server.example')
        .replace('__WINDOW_DECORATIONS__', 'null')
        .replace('__WINDOW_DECORATION_OPTIONS__', '[]')
        .replace('__DEVICE_PROFILE_JSON__', '{}');

    const document = {
        fullscreenElement: null,
        head: { appendChild() {} },
        addEventListener() {},
        createElement() {
            return { style: {}, classList: { add() {}, remove() {} } };
        },
        querySelector() {
            return null;
        }
    };
    const sandbox = {
        console,
        Date,
        document,
        history: {
            pushState(_state, _title, url) {
                pushedUrls.push(url);
            },
            replaceState(_state, _title, url) {
                replacedUrls.push(url);
            }
        },
        location: {
            href: 'http://server.example/web/index.html',
            hash: ''
        },
        MutationObserver: class {
            observe() {}
        },
        navigator: {
            language: 'en-US',
            platform: 'MacIntel',
            userAgent: 'native-shim-test'
        },
        open() {},
        addEventListener() {},
        jmpNative: {
            showServerOverlay() {
                serverSwitcherOpenCount += 1;
            }
        },
        __pushedUrls: pushedUrls,
        __replacedUrls: replacedUrls,
        __serverSwitcherOpenCount() {
            return serverSwitcherOpenCount;
        }
    };
    sandbox.window = sandbox;

    vm.runInNewContext(source, sandbox, { filename: 'native-shim.js' });
    return sandbox;
}

const sandbox = loadNativeShim();
const appHost = sandbox.window.NativeShell.AppHost;

assert.equal(appHost.supports('clientsettings'), true);
assert.equal(
    appHost.supports('multiserver'),
    true,
    'Jellium advertises multiserver only because route changes are handled by the native server overlay'
);
assert.equal(appHost.supports('MULTISERVER'), true);

sandbox.history.pushState({}, '', '/selectserver');
assert.equal(
    sandbox.__serverSwitcherOpenCount(),
    1,
    'Jellyfin Web select-server navigation opens the native server overlay'
);
assert.deepEqual(
    sandbox.__pushedUrls,
    [],
    'native server-switcher routes are not pushed into Jellyfin Web history'
);

sandbox.history.replaceState({}, '', '#/addserver');
assert.equal(
    sandbox.__serverSwitcherOpenCount(),
    2,
    'Jellyfin Web add-server navigation opens the native server overlay'
);
assert.deepEqual(
    sandbox.__replacedUrls,
    [],
    'native add-server routes are not pushed into Jellyfin Web history'
);

sandbox.history.pushState({}, '', '/web/#/home');
assert.deepEqual(
    sandbox.__pushedUrls,
    [ '/web/#/home' ],
    'ordinary Jellyfin Web navigation still reaches history.pushState'
);
