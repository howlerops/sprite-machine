# web/ — interim mobile web page

The phone page of plan §4.4 (WEB-001: pairing and the session list; the thread view is WEB-002).
Plain HTML and ES modules, no bundler, no framework, no runtime dependency. The hub serves this
directory with Workers Static Assets (`crates/hub/wrangler.toml` `[assets]`).

| Path                     | What                                                               |
| ------------------------ | ------------------------------------------------------------------ |
| `index.html`, `styles.css` | the shell and the mobile-first styles (light and dark)           |
| `src/app.js`             | routing and screens: sign-in, confirm, list, thread stub           |
| `src/api.js`             | the token in `localStorage` (`sm.session`) and the fetch wrapper; a 401 returns to sign-in |
| `src/pairing.js`         | `POST /v1/auth/links/redeem`; `POST /v1/auth/device/{code,token}` polling with `interval` / `slow_down` |
| `src/sessions.js`        | `GET /v1/machines` + `GET /v1/threads`, grouped by machine, polled every 5 s while visible |
| `.assetsignore`          | keeps `node_modules`, tests and package files out of the assets    |

## Routes

`/`, `/pair?code=…` (a runner's QR link), `/pair/device` (the page's own device code) and
`/#/t/<id>` are all `index.html` (`not_found_handling = "single-page-application"`); asset URLs
in the page are absolute. Everything except the page's own files and `/pair…` runs the Worker first
(`run_worker_first` with `!` exclusions), so `/v1/*`, `/t/*` (the hub's 302 to `/#/t/<id>`),
`/.well-known/*`, `/healthz` and unknown paths keep the hub's answers (JSON 404) and are never
answered with the page. The session token is stored only after the person confirms the user and org
it names; "Sign out" revokes it on the hub and clears it.

## Tests

```sh
cd web && npm ci && npx playwright test tests/pairing.spec.js
```

`playwright.config.js` starts the hub (`tests/helpers-hub.sh`): D1 migrations into a temp dir, then
`wrangler dev --local` with `DEV_FIXTURES=1` and random per-run secrets. Chromium is
`/opt/pw-browsers/chromium` (do not run `playwright install`). The first run builds the hub wasm
(minutes). `tests/helpers.js` calls the hub API and the dev-only fixture routes
(`POST /v1/dev/fixtures/machines/{id}/presence`, `POST /v1/dev/fixtures/threads`), which answer 404
unless `DEV_FIXTURES=1` (`crates/hub/src/dev_fixtures*`).
