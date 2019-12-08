// dash.cloudflare.com

addEventListener('fetch', event => {
    const request = event.request;
    const url = new URL(request.url);
    // trim first path component
    const relpath = url.pathname.replace(/^\/[^\/]+\//, '') + url.search;

    const headers = new Headers(request.headers);
    headers.set('user-agent', `${headers.get('user-agent')} (lib.rs proxied)`);

    const gh_req = new Request(`https://raw.githubusercontent.com/rust-lang/crates.io-index/master/${relpath}`, {
        headers,
    });
    return event.respondWith(fetch(gh_req).then(response => {
        const ifnone = headers.get("if-none-match");
        const etag = response.headers.get('etag');
        if (ifnone && etag) {
            const etag_trimmed = etag.replace(/^W\//, '');
            if (ifnone.indexOf(etag_trimmed) != -1) {
                const headers = new Headers(response.headers);
                return new Response("", {
                    status: 304,
                    headers,
                })
            }
        }
        return response;
    }));
})
