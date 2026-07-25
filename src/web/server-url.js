(function(global) {
    function defaultServerUrlForHostOnly(address) {
        const value = String(address || '').trim();
        if (!value || value.includes('://') || value.startsWith('//')) return null;

        // A bare IPv6 address needs brackets before the URL parser can
        // distinguish it from a host with an explicit port.
        const colonCount = (value.match(/:/g) || []).length;
        const candidate = colonCount > 1 && !value.startsWith('[')
            ? `http://[${value}]`
            : `http://${value}`;

        try {
            const url = new URL(candidate);
            if (url.username || url.password || url.port || url.pathname !== '/' || url.search || url.hash) {
                return null;
            }
            return `http://${url.hostname}:8096`;
        } catch (_) {
            return null;
        }
    }

    global.jmpDefaultServerUrlForHostOnly = defaultServerUrlForHostOnly;
})(typeof window === 'undefined' ? globalThis : window);
