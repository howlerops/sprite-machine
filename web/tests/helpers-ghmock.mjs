// A tiny mock GitHub for the browser tests (WEB-006): the token and user endpoints the hub calls
// when GITHUB_API_BASE points here. Started by helpers-hub.sh; never the real github.com.
//
// Stateless: the account is carried in the code and the access token, so a test picks who signs in
// by the code `ghcode.<login>.<id>`; the authorize page reads the account from a cookie.
//   GET  /login/oauth/authorize     cookie gh_account=<login>.<id> -> 302 to redirect_uri?code&state
//   POST /login/oauth/access_token  form: client_id, client_secret, code, code_verifier, ...
//                                   -> {access_token: "ghtok.<login>.<id>", token_type: "bearer"}
//   GET  /user  (Bearer ghtok.<login>.<id>) -> {id, login, name}
// Usage: node helpers-ghmock.mjs <port> <client id> <client secret>

import { createServer } from 'node:http';

const [port, clientId, clientSecret] = [process.argv[2], process.argv[3], process.argv[4]];
const ACCOUNT = /^(ghcode|ghtok)\.([a-z0-9-]{1,39})\.(\d{1,12})$/;

const json = (response, status, body) => {
  response.writeHead(status, { 'content-type': 'application/json' });
  response.end(JSON.stringify(body));
};

createServer((request, response) => {
  const url = new URL(request.url, 'http://mock');
  const chunks = [];
  request.on('data', (chunk) => chunks.push(chunk));
  request.on('end', () => {
    if (request.method === 'GET' && url.pathname === '/login/oauth/authorize') {
      // The sign-in page: who signs in is the cookie `gh_account=<login>.<id>` the test set for
      // 127.0.0.1 (cookies ignore the port). It checks what GitHub would, then goes back to the hub.
      const cookie = /(?:^|;\s*)gh_account=([a-z0-9-]{1,39})\.(\d{1,12})(?:;|$)/.exec(
        request.headers.cookie || '',
      );
      const back = url.searchParams.get('redirect_uri');
      if (
        url.searchParams.get('client_id') !== clientId ||
        !back ||
        !url.searchParams.get('state') ||
        url.searchParams.get('code_challenge_method') !== 'S256' ||
        !/^[A-Za-z0-9_-]{43}$/.test(url.searchParams.get('code_challenge') || '') ||
        !cookie
      ) {
        response.writeHead(400, { 'content-type': 'text/plain' });
        return response.end('mock GitHub: bad authorize request');
      }
      const target = new URL(back);
      target.searchParams.set('code', `ghcode.${cookie[1]}.${cookie[2]}`);
      target.searchParams.set('state', url.searchParams.get('state'));
      response.writeHead(302, { location: target.toString() });
      return response.end();
    }
    if (request.method === 'POST' && url.pathname === '/login/oauth/access_token') {
      const form = new URLSearchParams(Buffer.concat(chunks).toString());
      const match = ACCOUNT.exec(form.get('code') || '');
      // Exactly what GitHub would check: the app's id and secret, a code it issued, and the PKCE
      // verifier of the hub's own leg.
      if (
        form.get('client_id') !== clientId ||
        form.get('client_secret') !== clientSecret ||
        !form.get('code_verifier') ||
        !match ||
        match[1] !== 'ghcode'
      ) {
        return json(response, 200, { error: 'bad_verification_code' });
      }
      return json(response, 200, {
        access_token: `ghtok.${match[2]}.${match[3]}`,
        token_type: 'bearer',
        scope: 'read:user',
      });
    }
    if (request.method === 'GET' && url.pathname === '/user') {
      const bearer = (request.headers.authorization || '').replace(/^Bearer /i, '');
      const match = ACCOUNT.exec(bearer);
      if (!match || match[1] !== 'ghtok') return json(response, 401, { message: 'Bad credentials' });
      return json(response, 200, { id: Number(match[3]), login: match[2], name: match[2] });
    }
    return json(response, 404, { message: 'Not Found' });
  });
}).listen(Number(port), '127.0.0.1');
