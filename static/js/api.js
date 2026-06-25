/**
 * rustpbx console API client.
 *
 * Unified fetch wrapper for the JSON+Alpine refactor:
 *  - prefixes requests with window.__consoleApiPrefix (default "/api")
 *  - injects X-CSRF-Token (double-submit cookie) on mutations
 *  - unwraps the {status, data?, message?} envelope
 *  - redirects to /login on 401
 *  - surfaces errors via a global event consumed by consoleNotifications
 *
 * Usage:
 *   const list = await apiGet('/extensions', { page: 1 })
 *   const created = await apiPost('/extensions', { extension: '1001' })
 */
(function () {
  const API_PREFIX = window.__consoleApiPrefix || '/api';
  const MUTATIONS = new Set(['POST', 'PUT', 'PATCH', 'DELETE']);

  function readCookie(name) {
    const m = document.cookie.match(new RegExp('(?:^|; )' + name.replace(/([.$?*|{}()[\]\\/+^])/g, '\\$1') + '=([^;]*)'));
    return m ? decodeURIComponent(m[1]) : '';
  }

  function buildUrl(url) {
    if (/^(https?:)?\/\//.test(url) || url.startsWith(API_PREFIX)) return url;
    if (url.startsWith('/')) return API_PREFIX + url;
    return API_PREFIX + '/' + url;
  }

  function toast(type, message) {
    document.dispatchEvent(new CustomEvent('console:notify', { detail: { type, message } }));
    // Fallback alert if consoleNotifications hasn't mounted yet.
    if (type === 'error') console.error('[api]', message);
  }

  async function request(url, options) {
    options = options || {};
    const method = (options.method || 'GET').toUpperCase();
    const headers = Object.assign({}, options.headers || {});

    // CSRF double-submit: mutations must carry the token header.
    if (MUTATIONS.has(method)) {
      const token = readCookie('csrf_token');
      if (token) headers['X-CSRF-Token'] = token;
    }

    let body = options.body;
    if (body !== undefined && body !== null && typeof body !== 'string') {
      if (!(body instanceof FormData) && !(body instanceof Blob) && !(body instanceof ArrayBuffer)) {
        headers['Content-Type'] = headers['Content-Type'] || 'application/json';
        body = JSON.stringify(body);
      }
    }

    let resp;
    try {
      resp = await fetch(buildUrl(url), {
        method,
        headers,
        body,
        credentials: 'same-origin',
        signal: options.signal,
      });
    } catch (e) {
      toast('error', (window.__i18n_t && window.__i18n_t.errors && window.__i18n_t.errors.network) || 'Network error');
      throw e;
    }

    // 401 → bounce to login (preserve current location as next).
    if (resp.status === 401) {
      const loginUrl = (window.__consoleBasePath || '/console') + '/login?next=' + encodeURIComponent(location.pathname + location.search);
      location.href = loginUrl;
      throw new Error('unauthorized');
    }

    const ctype = resp.headers.get('content-type') || '';
    const isJson = ctype.includes('application/json');
    const payload = isJson ? await resp.json().catch(() => null) : null;

    if (!resp.ok) {
      const message = (payload && (payload.message || payload.error)) || resp.statusText || ('HTTP ' + resp.status);
      if (resp.status >= 500) toast('error', message);
      else if (resp.status !== 404) toast('error', message);
      const err = new Error(message);
      err.status = resp.status;
      err.payload = payload;
      throw err;
    }

    // Non-JSON (SSE / binary / text) → return raw response for the caller.
    if (!isJson) return resp;

    // Unwrap envelope. Tolerate handlers that still return bare data.
    if (payload && typeof payload === 'object' && 'status' in payload) {
      if (payload.status === 'error') {
        toast('error', payload.message || 'Request failed');
        const err = new Error(payload.message || 'Request failed');
        err.payload = payload;
        throw err;
      }
      return payload.data !== undefined ? payload.data : payload;
    }
    return payload;
  }

  const api = {
    get: (url, query) => {
      if (query) {
        const qs = new URLSearchParams(
          Object.entries(query).filter(([, v]) => v !== undefined && v !== null && v !== '')
            .reduce((acc, [k, v]) => { acc[k] = v; return acc; }, {})
        ).toString();
        if (qs) url += (url.includes('?') ? '&' : '?') + qs;
      }
      return request(url, { method: 'GET' });
    },
    post: (url, body, opts) => request(url, Object.assign({ method: 'POST', body }, opts)),
    put: (url, body, opts) => request(url, Object.assign({ method: 'PUT', body }, opts)),
    patch: (url, body, opts) => request(url, Object.assign({ method: 'PATCH', body }, opts)),
    del: (url, opts) => request(url, Object.assign({ method: 'DELETE' }, opts)),
    raw: request,
  };

  window.apiGet = api.get;
  window.apiPost = api.post;
  window.apiPut = api.put;
  window.apiPatch = api.patch;
  window.apiDelete = api.del;
  window.apiRequest = api.raw;
  window.RustPBX = window.RustPBX || {};
  window.RustPBX.api = api;

  // ── Global fetch interceptor ────────────────────────────────
  // Auto-inject X-CSRF-Token on same-origin mutations so that legacy
  // inline `fetch(...)` calls (not yet migrated to apiGet/apiPost) are
  // also CSRF-protected once csrf_guard is enabled (stage E).
  const nativeFetch = window.fetch.bind(window);
  const MUTATION_RE = /^(POST|PUT|PATCH|DELETE)$/i;
  window.fetch = function (input, init) {
    init = init || {};
    const method = (init.method || 'GET').toUpperCase();
    // Determine the URL string for same-origin check.
    const url = typeof input === 'string' ? input : (input && input.url) || '';
    const sameOrigin = !url || url.startsWith('/') || url.startsWith(location.origin) ||
      url.startsWith(window.__consoleApiPrefix || '/api');
    if (MUTATION_RE.test(method) && sameOrigin && !init.headers) {
      const token = readCookie('csrf_token');
      if (token) {
        init.headers = { 'X-CSRF-Token': token };
      }
    } else if (MUTATION_RE.test(method) && sameOrigin && init.headers) {
      // Inject into existing Headers object without clobbering other headers.
      const token = readCookie('csrf_token');
      if (token) {
        if (init.headers instanceof Headers) {
          if (!init.headers.has('X-CSRF-Token')) init.headers.set('X-CSRF-Token', token);
        } else if (Array.isArray(init.headers)) {
          if (!init.headers.some(([k]) => k.toLowerCase() === 'x-csrf-token')) {
            init.headers.push(['X-CSRF-Token', token]);
          }
        } else if (typeof init.headers === 'object') {
          if (!init.headers['X-CSRF-Token']) init.headers['X-CSRF-Token'] = token;
        }
      }
    }
    return nativeFetch(input, init);
  };

  // ── Progressive form enhancement ─────────────────────────────
  // Intercept traditional <form method="POST"> submissions targeted at
  // wholesale/console mutation routes and upgrade them to JSON fetch
  // calls. This avoids rewriting 27 wholesale form templates — each
  // form is auto-converted on submit.
  //
  // Forms can opt out via `data-no-json` or opt in explicitly via
  // `data-json-submit`. By default, any POST form whose action
  // matches console/wholesale mutation patterns is intercepted.
  document.addEventListener('submit', async (e) => {
    const form = e.target;
    if (!(form instanceof HTMLFormElement)) return;
    if (form.hasAttribute('data-no-json')) return;

    const action = form.getAttribute('action') || '';
    const method = (form.getAttribute('method') || 'GET').toUpperCase();
    if (method !== 'POST') return; // only POST forms

    // Intercept any POST form targeting a wholesale mutation route.
    const isWholesalePost = method === 'POST' && action.includes('/wholesale/');
    const isExplicit = form.hasAttribute('data-json-submit');
    if (!isWholesalePost && !isExplicit) return;

    e.preventDefault();
    e.stopImmediatePropagation();

    // Build JSON payload from FormData, converting checkbox booleans.
    const fd = new FormData(form);
    const payload = {};
    for (const [key, value] of fd.entries()) {
      // Checkbox values: "on" → true
      const el = form.querySelector(`[name="${key}"][type="checkbox"]`);
      if (el) {
        payload[key] = value === 'on' || value === 'true' || value === '1';
      } else if (payload[key] !== undefined) {
        if (!Array.isArray(payload[key])) payload[key] = [payload[key]];
        payload[key].push(value);
      } else {
        payload[key] = value;
      }
    }
    // Handle unchecked checkboxes (FormData omits them).
    form.querySelectorAll('input[type="checkbox"][name]').forEach((cb) => {
      if (!(cb.name in payload)) payload[cb.name] = cb.checked;
    });

    const submitBtn = form.querySelector('[type="submit"]');
    if (submitBtn) submitBtn.disabled = true;

    try {
      // Inject CSRF token explicitly (nativeFetch bypasses the window.fetch
      // interceptor that normally adds it).
      const httpMethod = form.dataset.method || method;
      const csrfToken = readCookie('csrf_token');
      const resp = await nativeFetch(action, {
        method: httpMethod,
        headers: { 'Content-Type': 'application/json', ...(csrfToken ? { 'X-CSRF-Token': csrfToken } : {}) },
        body: JSON.stringify(payload),
        credentials: 'same-origin',
      });

      const ctype = resp.headers.get('content-type') || '';
      const body = ctype.includes('application/json') ? await resp.json().catch(() => null) : null;

      if (!resp.ok || (body && body.status === 'error')) {
        const msg = (body && (body.message || body.error)) || `HTTP ${resp.status}`;
        document.dispatchEvent(new CustomEvent('console:notify', { detail: { type: 'error', message: msg } }));
        return;
      }

      // Success: redirect or notify.
      let redirect = form.dataset.redirect;
      if (!redirect) {
        // Derive list page from action: strip trailing /{id}/edit, /{id}/delete, etc.
        // /console/wholesale/tenants/123/edit → /console/wholesale/tenants
        // /console/wholesale/tenants          → /console/wholesale/tenants
        const parts = action.replace(/^https?:\/\/[^/]+/, '').split('/').filter(Boolean);
        while (parts.length > 1) {
          const last = parts[parts.length - 1];
          if (/^\d+$/.test(last) || ['edit', 'delete', 'new', 'create', 'update'].includes(last)) {
            parts.pop();
          } else {
            break;
          }
        }
        redirect = '/' + parts.join('/');
      }
      if (redirect) {
        window.location.href = redirect;
      } else {
        document.dispatchEvent(new CustomEvent('console:notify', { detail: { type: 'success', message: (body && body.message) || 'Saved successfully' } }));
      }
    } catch (err) {
      document.dispatchEvent(new CustomEvent('console:notify', { detail: { type: 'error', message: err.message || 'Network error' } }));
    } finally {
      if (submitBtn) submitBtn.disabled = false;
    }
  }, true); // capture phase to beat Alpine's handler
})();
