# Admin console for rustpbx

## Static assets

Build the console stylesheet and bundled Chart.js from the repository root:

```sh
npm --prefix frontend ci --ignore-scripts
npm --prefix frontend run build
```

Commit the generated `static/css/console.css` and `static/vendor/` files so
local binaries can serve them without Node.js installed. Rebuild after changing
template classes. The old console E2E `npm run build:css` command delegates to
this shared frontend package; install its dependencies with the command above.

Tailwind scans the core templates and any sibling `rustpbx-commerce`,
`rustpbx-wholesale`, and `rustpbx-cc` checkout. Community, commerce, and wholesale
Docker builds regenerate the assets with the templates available to that edition.

Dashboard and Reports use `/static/vendor/chart.umd.min.js` by default, respecting
`console.static_path`. `console.chart_js` can override that URL. Alpine.js and the
login page's browser Tailwind runtime retain their existing configurable CDN URLs.
