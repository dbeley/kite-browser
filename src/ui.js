/*
 * kite UI layer — injected into the top frame of every document.
 *
 * Why in-page instead of a native widget: the engine owns the window, so the
 * keyboard path is Chromium's native key path plus one JS handler — no IPC, no
 * synthetic events, no focus stealing (which is unsolvable on Wayland for a
 * separate UI process). The chrome itself is a shadow-DOM overlay, so page CSS
 * cannot style it and it cannot be reflowed by the page.
 *
 * Privileged work (tabs, navigation, history, clipboard, quit) is delegated to
 * the Rust controller over a CDP binding, but nothing on the hot path is.
 */
(() => {
  'use strict';
  const VERSION = '0.3.0';
  if (window.__kite && window.__kite.version === VERSION) return;
  let isTop = false;
  try { isTop = window.top === window; } catch (e) { isTop = false; }
  if (!isTop) return;

  /* ------------------------------------------------------------------ state */
  const S = {
    version: VERSION,
    mode: 'normal',            // normal | insert | passthrough | hint | command
    cfg: { settings: {}, keymap: {}, search: {} },
    tabs: [],
    activeIndex: 0,
    url: location.href,
    title: document.title,
    count: '',
    seq: [],
    seqTimer: 0,
    hideTimer: 0,
    pinned: false,
    hint: null,
    cmd: null,
    find: { last: '', dir: 1 },
    home: location.href,
    homeUrl: location.href,
    lastScroll: 0,
    loaded: false,
  };

  /* --------------------------------------------------------------- bridge */
  let rid = 1;
  const pending = new Map();
  function send(cmd, args) {
    return new Promise((resolve) => {
      const id = rid++;
      pending.set(id, resolve);
      const fire = () => {
        try {
          window.__kiteSend(JSON.stringify({ cmd, args: args || {}, rid: id }));
        } catch (e) {
          pending.delete(id);
          resolve(null);
        }
      };
      if (typeof window.__kiteSend === 'function') fire();
      else setTimeout(() => {
        if (typeof window.__kiteSend === 'function') fire();
        else { pending.delete(id); resolve(null); }
      }, 40);
    });
  }
  // fire and forget, never blocks a keystroke
  const tell = (cmd, args) => { send(cmd, args); };

  /* ------------------------------------------------------------------ dom */
  // Built lazily. Injecting at document-start means documentElement may not
  // exist yet, and appending to `document` at that moment aborts the HTML
  // parser — which silently produced a blank page. Never touch DOM before the
  // parser has something to attach to.
  let HOST = null, ROOT = null, UI = false;
  let chromeEl, bar, tabsEl, urlEl, chip, badges, pendingEl, titleEl, spinEl;
  let pal, pinput, plist, pmeta, psig, hintLayer, toasts, helpEl;
  const CSS = `
    :host { all: initial; }
    * { box-sizing: border-box; }
    #chrome, #help {
      font-family: var(--kite-font, ui-monospace, "Iosevka Term", "Overpass Mono",
                   "JetBrains Mono", "DejaVu Sans Mono", Menlo, monospace);
      font-variant-ligatures: none; text-rendering: geometricPrecision;
      -webkit-font-smoothing: antialiased;
    }
    #chrome {
      position: fixed; left: 0; right: 0; bottom: 0; z-index: 2147483647;
      display: flex; flex-direction: column;
      background: #0b0e11; color: #c2cad4;
      font-size: 11.5px; line-height: 1; letter-spacing: 0; font-weight: 400;
      border-top: 1px solid #20272f;
      transform: translateY(100%); transition: transform .1s steps(2, end);
      will-change: transform; pointer-events: none;
    }
    #chrome.on { transform: none; pointer-events: auto; }
    #chrome.off { display: none; }

    /* tabs: one line, active tab in inverse video */
    #tabs { display: flex; align-items: stretch; height: 19px; overflow: hidden; background: #0b0e11; }
    #tabs.off { display: none; }
    #tabs:not(.off):not(:empty) { border-bottom: 1px solid #1a2028; }
    .tab {
      display: flex; align-items: center; gap: 7px; padding: 0 9px; max-width: 30ch;
      white-space: nowrap; overflow: hidden; color: #626e7d; cursor: pointer; font-size: 11px;
    }
    .tab .ix { color: #3d4652; }
    .tab .lbl { overflow: hidden; text-overflow: ellipsis; }
    .tab.act { background: #c2cad4; color: #0b0e11; }
    .tab.act .ix { color: #0b0e11; opacity: .5; }
    .tab.priv .lbl { font-style: italic; }

    /* status line */
    #bar { display: flex; align-items: center; gap: 10px; height: 21px; padding: 0 9px; overflow: hidden; }
    #bar.off { display: none; }
    #chip { color: #8fbf8f; font-weight: 700; flex: 0 0 auto; }
    #chip.ins { color: #d7b06a; }
    #chip.hnt, #chip.cmd { color: #9fb0c2; }
    #chip.psv { color: #626e7d; }
    #pending { color: #d7b06a; flex: 0 0 auto; }
    #pending:empty { display: none; }
    #title { color: #626e7d; flex: 0 1 auto; max-width: 34%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    #title:empty { display: none; }
    #url { color: #c2cad4; flex: 1 1 auto; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    #url b { color: #8fbf8f; font-weight: 400; }
    #badges { display: flex; align-items: center; gap: 10px; color: #626e7d; flex: 0 0 auto; }
    #badges:empty { display: none; }
    #spin { width: 1.2ch; flex: 0 0 auto; text-align: center; color: #8fbf8f; }

    /* command line: full-width panel above the status line */
    #pal { display: none; flex-direction: column; max-height: 44vh; background: #0d1117; border-bottom: 1px solid #20272f; }
    #pal.on { display: flex; }
    #prow { display: flex; align-items: center; gap: 8px; padding: 8px 10px; border-bottom: 1px solid #19202a; }
    #psig { color: #8fbf8f; flex: 0 0 auto; }
    #pinput {
      flex: 1 1 auto; min-width: 0; padding: 0; border: 0; outline: 0; background: transparent;
      color: #dfe5ec; font: inherit; font-size: 12.5px; caret-color: #8fbf8f;
    }
    #pinput::placeholder { color: #414b57; }
    #pmeta { flex: 0 0 auto; color: #414b57; font-size: 11px; }
    #plist { overflow-y: auto; overscroll-behavior: contain; padding: 3px 0 5px; }
    #plist:empty { display: none; }
    .it { display: flex; align-items: baseline; gap: 10px; padding: 3px 10px; white-space: nowrap; cursor: pointer; font-size: 12px; }
    .it .mk { width: 1ch; flex: 0 0 auto; color: #414b57; }
    .it .k { width: 9ch; flex: 0 0 auto; color: #515c69; font-size: 11px; }
    .it .t { flex: 0 1 24ch; overflow: hidden; text-overflow: ellipsis; color: #dfe5ec; }
    .it .d { flex: 0 1 auto; overflow: hidden; text-overflow: ellipsis; color: #515c69; font-size: 11px; }
    .it.sel { background: #c2cad4; }
    .it.sel .mk, .it.sel .k, .it.sel .t, .it.sel .d { color: #0b0e11; }

    /* hint labels */
    #hints { position: fixed; inset: 0; pointer-events: none; }
    .h {
      position: absolute; padding: 2px 4px; font-size: 10.5px; font-weight: 700; line-height: 1;
      letter-spacing: .04em; background: #8fbf8f; color: #0b0e11; border: 1px solid #0b0e11;
      text-transform: lowercase;
    }
    .h.dim { opacity: .16; }
    .h.match { background: #0b0e11; color: #8fbf8f; border-color: #8fbf8f; }

    /* messages */
    #toasts { display: flex; flex-direction: column; align-items: flex-start; gap: 3px; padding: 5px 9px 0; }
    #toasts:empty { display: none; }
    .toast { padding: 3px 7px; background: #12171e; color: #a9b6c6; border-left: 2px solid #8fbf8f; font-size: 11px; }

    /* help: a full-screen pager, two columns of keys */
    #help {
      position: fixed; inset: 0; display: none; z-index: 2147483647; overflow: auto;
      background: #0b0e11; color: #c2cad4; font-size: 12px; line-height: 1.35;
    }
    #help.on { display: block; }
    #help .head {
      position: sticky; top: 0; display: flex; justify-content: space-between; gap: 12px;
      padding: 7px 12px; background: #0b0e11; border-bottom: 1px solid #1a2028; color: #626e7d;
    }
    #help .head b { color: #8fbf8f; font-weight: 700; }
    #help .body { padding: 4px 12px 32px; }
    #help .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 0 30px; }
    #help h2 {
      display: flex; align-items: center; gap: 9px;
      margin: 16px 0 6px; font-size: 11px; font-weight: 400; color: #626e7d;
    }
    #help h2::after { content: ""; flex: 1 1 auto; height: 1px; background: #1a2028; }
    #help .row { display: flex; gap: 10px; padding: 1px 0; min-width: 0; }
    #help .row .k { flex: 0 0 13ch; color: #dfe5ec; }
    #help .row .a { color: #626e7d; }
    #help kbd { background: none; border: 0; padding: 0; color: inherit; font: inherit; }
  `;

  const HTML = `
    <div id="chrome">
      <div id="toasts"></div>
      <div id="pal">
        <div id="prow"><span id="psig">:</span><input id="pinput" spellcheck="false" autocomplete="off" placeholder=""><span id="pmeta"></span></div>
        <div id="plist"></div>
      </div>
      <div id="tabs"></div>
      <div id="bar">
        <span id="chip">NOR</span><span id="pending"></span><span id="title"></span><span id="url"></span><span id="spin"></span><span id="badges"></span>
      </div>
    </div>
    <div id="hints"></div>
    <div id="help"></div>
  `;

  function ensureUI() {
    if (UI) return true;
    const de = document.documentElement;
    if (!de) return false; // parser has not created <html> yet
    HOST = document.createElement('div');
    HOST.id = '__kite_host__';
    HOST.style.cssText = 'all:initial;position:fixed;top:0;left:0;width:0;height:0;z-index:2147483647;';
    ROOT = HOST.attachShadow({ mode: 'closed' });
    ROOT.innerHTML = '<style>' + CSS + '</style>' + HTML;
    de.appendChild(HOST);
    const $ = (id) => ROOT.getElementById(id);
    chromeEl = $('chrome'); bar = $('bar'); tabsEl = $('tabs'); urlEl = $('url');
    chip = $('chip'); badges = $('badges'); pendingEl = $('pending'); titleEl = $('title');
    spinEl = $('spin'); pal = $('pal'); pinput = $('pinput'); plist = $('plist');
    pmeta = $('pmeta'); psig = $('psig'); hintLayer = $('hints'); toasts = $('toasts');
    helpEl = $('help');
    UI = true;
    bindUI();
    return true;
  }
  ensureUI();

  /* ---------------------------------------------------------------- view */
  const MODE_CHIP = { normal: ['NOR', ''], insert: ['INS', 'ins'], passthrough: ['PSV', 'psv'], hint: ['HNT', 'hnt'], command: ['CMD', 'cmd'] };

  function prettyUrl(u) {
    const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
    if (u && u === S.homeUrl) return '<b>kite</b>:home';
    try {
      const x = new URL(u);
      if (x.protocol === 'https:' || x.protocol === 'http:') {
        const host = x.host;
        const rest = (x.pathname + x.search + x.hash).replace(/^\/$/, '');
        return '<b>' + esc(host) + '</b>' + esc(rest || '/');
      }
    } catch (e) {}
    return esc(u);
  }

  function renderBar() {
    if (!UI && !ensureUI()) return;
    const [label, cls] = MODE_CHIP[S.mode] || MODE_CHIP.normal;
    chip.textContent = label;
    chip.className = cls;
    pendingEl.textContent = S.count || S.seq.join('');
    const title = (S.title || document.title || '').trim();
    titleEl.textContent = title && title !== S.url ? title : '';
    urlEl.innerHTML = prettyUrl(S.url || location.href);
    let b = '';
    if (S.bookmarked) b += '<span class="b">bm</span>';
    if (S.private) b += '<span class="b">priv</span>';
    if (S.tabs.length > 1) b += '<span class="b">' + (S.activeIndex + 1) + '/' + S.tabs.length + '</span>';
    if (S.zoom && S.zoom !== 1) b += '<span class="b">' + Math.round(S.zoom * 100) + '%</span>';
    badges.innerHTML = b;
    chromeEl.classList.toggle('off', S.cfg.settings.status === false);

    if (S.tabs.length > 1) {
      let h = '';
      S.tabs.forEach((t, i) => {
        const lbl = (t.title || t.url || 'new tab').replace(/\s+/g, ' ').slice(0, 40);
        h += '<div class="tab' + (i === S.activeIndex ? ' act' : '') + (t.private ? ' priv' : '') +
          '" data-i="' + i + '"><span class="ix">' + (i + 1) + '</span><span class="lbl">' +
          esc(lbl) + '</span></div>';
      });
      tabsEl.innerHTML = h;
      tabsEl.classList.remove('off');
    } else {
      tabsEl.innerHTML = '';
      tabsEl.classList.add('off');
    }
  }

  function showBar(sticky) {
    if (!UI && !ensureUI()) return;
    renderBar();
    chromeEl.classList.add('on');
    if (sticky) S.pinned = true;
    clearTimeout(S.hideTimer);
    if (S.cfg.settings.bar === 'always') return;
    if (!sticky) S.hideTimer = setTimeout(maybeHideBar, 1600);
  }
  function maybeHideBar() {
    if (S.cfg.settings.bar === 'always') return;
    if (S.pinned && (S.mode !== 'normal' || (S.cmd && S.cmd.open))) return;
    if (S.mode !== 'normal' || (S.cmd && S.cmd.open)) return;
    chromeEl.classList.remove('on');
  }
  function unpin() { S.pinned = false; clearTimeout(S.hideTimer); S.hideTimer = setTimeout(maybeHideBar, 600); }

  function toast(msg, ms) {
    if (!UI && !ensureUI()) return;
    // The message lives in the same bottom stack, so reveal it with the bar.
    showBar();
    const el = document.createElement('div');
    el.className = 'toast';
    el.textContent = msg;
    toasts.appendChild(el);
    setTimeout(() => el.remove(), ms || 1700);
  }

  // A spinner in the status line: no progress bar, no layout thrash, and it
  // reads like a terminal instead of a browser.
  const SPIN = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
  let spinTimer = 0;
  function setLoading(on) {
    S.loading = !!on;
    clearInterval(spinTimer);
    spinTimer = 0;
    if (!UI && !ensureUI()) return;
    if (!S.loading) { spinEl.textContent = ''; return; }
    let i = 0;
    spinEl.textContent = SPIN[0];
    spinTimer = setInterval(() => { spinEl.textContent = SPIN[++i % SPIN.length]; }, 90);
  }

  /* ------------------------------------------------------------ scrolling */
  function step() { return (S.cfg.settings.scroll_step | 0) || 84; }
  function scrollByY(dy) {
    const smooth = S.cfg.settings.smooth_scroll === true;
    window.scrollBy({ top: dy, left: 0, behavior: smooth ? 'smooth' : 'auto' });
  }
  function viewH() { return window.innerHeight; }

  /* ------------------------------------------------------------- zooming */
  const zoomEl = () => document.body || document.documentElement;
  function setZoom(z) {
    z = Math.max(0.3, Math.min(3, z));
    S.zoom = z;
    const el = zoomEl();
    if (el) el.style.zoom = z === 1 ? '' : String(z);
    renderBar(); showBar();
  }
  function stepZoom(dir) {
    const levels = [0.5, 0.67, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2, 2.5, 3];
    let i = levels.findIndex((v) => v >= S.zoom - 0.001);
    if (i < 0) i = 4;
    i = Math.max(0, Math.min(levels.length - 1, i + dir));
    setZoom(levels[i]);
  }

  /* -------------------------------------------------------------- actions */
  const ACT = {
    'scroll down': (n) => scrollByY(step() * n),
    'scroll up': (n) => scrollByY(-step() * n),
    'scroll left': (n) => window.scrollBy(-step() * n, 0),
    'scroll right': (n) => window.scrollBy(step() * n, 0),
    'scroll page-down': (n) => scrollByY(viewH() * 0.9 * n),
    'scroll page-up': (n) => scrollByY(-viewH() * 0.9 * n),
    'scroll half-page-down': (n) => scrollByY(viewH() * 0.45 * n),
    'scroll half-page-up': (n) => scrollByY(-viewH() * 0.45 * n),
    'scroll top': () => window.scrollTo(0, 0),
    'scroll bottom': () => window.scrollTo(0, document.documentElement.scrollHeight),
    'scroll right-edge': () => window.scrollTo(document.documentElement.scrollWidth, window.scrollY),
    'scroll left-edge': () => window.scrollTo(0, window.scrollY),

    back: () => tell('history', { op: 'back' }),
    forward: () => tell('history', { op: 'forward' }),
    reload: () => tell('history', { op: 'reload' }),
    'reload-hard': () => tell('history', { op: 'hardreload' }),
    stop: () => tell('history', { op: 'stop' }),
    home: () => tell('nav', { url: '' }),

    open: () => openPalette('open', ''),
    'open-newtab': () => openPalette('open', '', true),
    'open-edit': () => openPalette('open', location.href),
    command: () => openPalette('command', ''),
    find: () => openPalette('find', S.find.last || ''),
    help: () => toggleHelp(),
    clear: () => { clearSeq(); cancelHint(); closePalette(); toggleHelp(false); unpin(); setMode('normal'); },

    hint: () => startHint({ newTab: false, yank: false }),
    'hint-newtab': () => startHint({ newTab: true, yank: false }),
    'hint-yank': () => startHint({ newTab: false, yank: true }),
    'hint-yank-newtab': () => startHint({ newTab: true, yank: true }),

    'find-next': () => findStep(1),
    'find-prev': () => findStep(-1),

    'tab-new': () => tell('tab', { op: 'new', url: '' }),
    'tab-close': () => tell('tab', { op: 'close' }),
    'tab-next': () => tell('tab', { op: 'next' }),
    'tab-prev': () => tell('tab', { op: 'prev' }),
    'tab-first': () => tell('tab', { op: 'first' }),
    'tab-last': () => tell('tab', { op: 'last' }),
    'tab-undo-close': () => tell('tab', { op: 'undo' }),

    'yank-url': () => tell('yank', { text: location.href }),
    'yank-title': () => tell('yank', { text: document.title }),
    'paste-open': () => send('paste').then((t) => t && tell('nav', { url: t })),
    'paste-open-newtab': () => send('paste').then((t) => t && tell('nav', { url: t, newTab: true })),
    'bookmark-add': () => tell('bookmark', { op: 'toggle', url: location.href, title: document.title }),
    'bookmark-list': () => openPalette('bookmark', ''),
    'history-list': () => openPalette('history', ''),

    'zoom-in': () => stepZoom(1),
    'zoom-out': () => stepZoom(-1),
    'zoom-reset': () => setZoom(1),

    'mode insert': () => setMode('insert'),
    'mode normal': () => setMode('normal'),
    'mode passthrough': () => setMode('passthrough'),
    'focus-input': () => focusFirstInput(),
    external: () => tell('external', { url: location.href }),
    quit: () => tell('quit', {}),

    'toggle-images': () => toggleImages(),
    'session-save': () => toast('session is saved continuously'),
    private: () => tell('tab', { op: 'new', url: '', priv: true }),

    'hint-cancel': () => cancelHint(),
    'hint-enter': () => finishHint(),
    'command-cancel': () => closePalette(),
    'command-enter': () => paletteAccept(false),
    'command-enter-tab': () => paletteAccept(true),
    'command-next': () => paletteMove(1),
    'command-prev': () => paletteMove(-1),
    'command-word-delete': () => paletteEdit('worddel'),
    'command-clear': () => paletteEdit('clear'),
    'command-backspace': () => paletteEdit('backspace'),
    'command-left': () => paletteEdit('left'),
    'command-right': () => paletteEdit('right'),
    'command-home': () => paletteEdit('home'),
    'command-end': () => paletteEdit('end'),
    'command-paste': () => send('paste').then((t) => { if (t) { pinput.value += t; paletteChanged(); } }),
    'command-split': () => {},
  };

  function dispatch(action, count) {
    const fn = ACT[action];
    if (!fn) { toast('unbound action: ' + action); return; }
    try { fn(count || 1); } catch (e) { S.lastError = action + ': ' + e.message; toast('error: ' + e.message); }
  }

  /* ----------------------------------------------------------------- modes */
  function setMode(m) {
    if (S.mode === m) return;
    S.mode = m;
    S.count = '';
    S.seq = [];
    if (m !== 'normal') showBar(true);
    else unpin();
    if (m === 'insert') {
      const el = document.activeElement;
      if (el && isEditable(el)) el.focus();
    }
    renderBar();
  }

  function isEditable(el) {
    if (!el || !el.tagName) return false;
    const tag = el.tagName;
    if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true;
    if (el.isContentEditable) return true;
    return false;
  }

  function focusFirstInput() {
    const cands = Array.from(document.querySelectorAll('input:not([type=hidden]),textarea,[contenteditable=true]'));
    for (const el of cands) {
      const r = el.getBoundingClientRect();
      if (r.width > 8 && r.height > 8) { el.focus(); setMode('insert'); return; }
    }
    toast('no input on page');
  }

  let imagesOff = false;
  function applyImages(off, quiet) {
    imagesOff = off;
    let el = document.getElementById('__kite_noimg__');
    if (imagesOff) {
      if (!el) {
        el = document.createElement('style');
        el.id = '__kite_noimg__';
        el.textContent = 'img,picture,video,svg,canvas{visibility:hidden !important}';
        document.documentElement.appendChild(el);
      }
      if (!quiet) toast('images off');
    } else if (el) {
      el.remove();
      if (!quiet) toast('images on');
    }
  }
  function toggleImages() { applyImages(!imagesOff, false); }

  /* ------------------------------------------------------------ key engine */
  const SPECIAL = {
    ' ': 'Space', Enter: 'CR', Escape: 'Esc', Tab: 'Tab', Backspace: 'BS', Delete: 'Del',
    ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right',
    Home: 'Home', End: 'End', PageUp: 'PgUp', PageDown: 'PgDn',
  };

  function tok(e) {
    let k = e.key;
    if (k === undefined) return null;
    if (k === 'Dead' || k === 'Unidentified') return null;
    const named = SPECIAL[k];
    let mods = '';
    if (e.ctrlKey) mods += 'C-';
    if (e.altKey) mods += 'A-';
    if (e.metaKey) mods += 'M-';
    let body;
    if (named) body = named;
    else if (k.length === 1) {
      // Letters keep Shift's case: "G" and "g" are different bindings.
      // Symbols/digits arrive already shifted (":" for Shift+";").
      body = /[A-Za-z]/.test(k)
        ? ((e.shiftKey || k === k.toUpperCase()) ? k.toUpperCase() : k.toLowerCase())
        : k;
    } else return null;
    if (mods) return '<' + mods + body + '>';
    return body;
  }

  function map() { return (S.cfg.keymap[S.mode]) || S.cfg.keymap.normal || {}; }

  function lookup(seq) {
    const m = map();
    const joined = seq.join(' ');
    if (Object.prototype.hasOwnProperty.call(m, joined)) return { exact: m[joined] };
    const prefix = joined + ' ';
    for (const k in m) if (k.startsWith(prefix)) return { prefix: true };
    return null;
  }

  function clearSeq() {
    S.seq = [];
    S.count = '';
    clearTimeout(S.seqTimer);
  }

  function onKeyDown(e) {
    if (e.isComposing) return;
    if (!ensureUI()) return;

    // Help overlay swallows the next key.
    if (helpOpen) {
      if (e.key === 'Escape' || e.key === 'q') { toggleHelp(false); e.preventDefault(); }
      return;
    }

    // Palette has focus: its own handler owns the keyboard.
    if (S.mode === 'command') return;

    if (S.hint) {
      const handled = hintKey(e);
      if (handled) { e.preventDefault(); e.stopPropagation(); }
      return;
    }

    if (e.key === 'Escape') {
      if (S.mode === 'insert' || S.mode === 'passthrough') {
        const el = document.activeElement;
        if (el && el.blur) el.blur();
        setMode('normal');
        e.preventDefault();
        return;
      }
      if (S.seq.length || S.count) { clearSeq(); maybeHideBar(); e.preventDefault(); return; }
    }

    const editing = isEditable(e.target) || isEditable(document.activeElement);
    if (S.mode === 'insert' || S.mode === 'passthrough') return;
    if (editing) {
      // Typing in a field: stay out of the way, but remember the mode.
      if (S.mode !== 'insert') S.mode = 'insert';
      return;
    }

    if (e.ctrlKey || e.metaKey || e.altKey) {
      // Registered chords (C-d, C-u, …) are matched below; anything else is
      // left to the browser/page.
      const t = tok(e);
      if (!t) return;
      if (!lookup([t])) return;
    }

    if (e.key === 'Shift' || e.key === 'Control' || e.key === 'Alt' || e.key === 'Meta') return;

    const t = tok(e);
    if (!t) return;
    S.lastKey = { raw: e.key, tok: t, editing: editing, mode: S.mode, seq: S.seq.slice() };

    // Counts: 1-9 start one, 0 is a binding.
    if (/^[1-9]$/.test(t) && S.seq.length === 0) {
      S.count = (S.count + t).slice(0, 6);
      showBar();
      e.preventDefault();
      return;
    }

    if (S.seq.length === 0 && /^[1-9]$/.test(S.count) === false) S.count = S.count;

    const next = S.seq.concat([t]);
    const hit = lookup(next);
    if (!hit) {
      if (S.seq.length) { clearSeq(); }
      return; // let the page have the key
    }
    e.preventDefault();
    e.stopPropagation();
    showBar();

    if (hit.prefix && !hit.exact) {
      S.seq = next;
      clearTimeout(S.seqTimer);
      S.seqTimer = setTimeout(() => { clearSeq(); maybeHideBar(); }, 900);
      return;
    }
    S.seq = [];
    const n = S.count ? parseInt(S.count, 10) : 1;
    S.count = '';
    dispatch(hit.exact, n);
  }

  /* ---------------------------------------------------------------- hints */
  const HINT_SELECTOR = 'a[href],button,input:not([type=hidden]),textarea,select,summary,' +
    '[onclick],[role=button],[role=link],[role=checkbox],[role=tab],[tabindex]:not([tabindex="-1"])';

  function visibleRects(el) {
    const r = el.getBoundingClientRect();
    if (r.width < 3 || r.height < 3) return null;
    if (r.bottom < 0 || r.top > window.innerHeight) return null;
    if (r.right < 0 || r.left > window.innerWidth) return null;
    const cs = getComputedStyle(el);
    if (cs.visibility === 'hidden' || cs.display === 'none' || cs.opacity === '0') return null;
    if (parseFloat(cs.opacity) < 0.15) return null;
    return r;
  }

  function hintCandidates() {
    const seen = new Set();
    const out = [];
    const nodes = document.querySelectorAll(HINT_SELECTOR);
    for (const el of nodes) {
      if (HOST.contains(el)) continue;
      if (el.disabled) continue;
      const cs = getComputedStyle(el);
      if (cs.pointerEvents === 'none') continue;
      const r = visibleRects(el);
      if (!r) continue;
      // skip elements fully covered by a same-sized ancestor link (nested markup)
      const key = Math.round(r.left) + ':' + Math.round(r.top) + ':' + Math.round(r.width);
      if (seen.has(key)) continue;
      seen.add(key);
      out.push({ el, r });
      if (out.length >= 240) break;
    }
    return out;
  }

  function labels(n, chars) {
    const k = Math.max(2, chars.length);
    let len = 1;
    while (Math.pow(k, len) < n) len++;
    const out = [];
    for (let i = 0; i < n; i++) {
      let x = i, s = '';
      for (let j = 0; j < len; j++) { s = chars[x % k] + s; x = Math.floor(x / k); }
      out.push(s);
    }
    return out;
  }

  function startHint(opts) {
    if (!UI && !ensureUI()) return;
    cancelHint();
    const cands = hintCandidates();
    if (!cands.length) { toast('no targets'); return; }
    const chars = (S.cfg.settings.hints_chars || 'asdfghjkl;').split('');
    const labs = labels(cands.length, chars);
    hintLayer.innerHTML = '';
    S.hint = { items: [], typed: '', newTab: opts.newTab, yank: opts.yank, chars };
    cands.forEach((c, i) => {
      const d = document.createElement('div');
      d.className = 'h';
      d.textContent = labs[i];
      const w = Math.max(12, labs[i].length * 7 + 6);
      // Keep labels clear of the chrome, which overlays the bottom strip.
      const h = chromeEl && chromeEl.classList.contains('on') ? chromeEl.offsetHeight : 0;
      const top = Math.min(c.r.top + Math.min(c.r.height / 2 - 7, 20), window.innerHeight - 16 - h);
      d.style.left = Math.max(0, Math.min(window.innerWidth - w, c.r.left)) + 'px';
      d.style.top = Math.max(0, top) + 'px';
      hintLayer.appendChild(d);
      S.hint.items.push({ el: c.el, label: labs[i], node: d, href: c.el.href || '' });
    });
    setMode('hint');
    showBar(true);
    renderBar();
  }

  function cancelHint() {
    if (!S.hint) return;
    hintLayer.innerHTML = '';
    S.hint = null;
    if (S.mode === 'hint') setMode('normal');
  }

  function hintKey(e) {
    if (!S.hint) return false;
    const k = e.key;
    if (k === 'Escape') { cancelHint(); return true; }
    if (k === 'Enter') { finishHint(); return true; }
    if (k === 'Backspace') {
      S.hint.typed = S.hint.typed.slice(0, -1);
      paintHints();
      return true;
    }
    if (k.length === 1) {
      const ch = k.toLowerCase();
      if (S.hint.chars.indexOf(ch) >= 0) {
        S.hint.typed += ch;
        paintHints();
        const exact = S.hint.items.find((i) => i.label === S.hint.typed);
        if (exact) finishHint();
        return true;
      }
    }
    return false;
  }

  function paintHints() {
    if (!S.hint) return;
    const typed = S.hint.typed;
    let matches = 0;
    for (const it of S.hint.items) {
      const isMatch = it.label.startsWith(typed);
      if (isMatch && typed.length) matches++;
      it.node.classList.toggle('dim', !isMatch);
      it.node.classList.toggle('match', typed.length > 0 && isMatch);
    }
    if (typed.length && !matches) { cancelHint(); toast('no hint match'); }
  }

  function finishHint() {
    if (!S.hint) return;
    const typed = S.hint.typed;
    const it = typed ? S.hint.items.find((i) => i.label === typed) : null;
    const opts = { newTab: S.hint.newTab, yank: S.hint.yank };
    if (!it) { cancelHint(); return; }
    const el = it.el;
    const href = it.href;
    cancelHint();
    if (opts.yank) {
      const text = href || el.textContent.trim();
      tell('yank', { text });
      return;
    }
    if (opts.newTab && href) { tell('nav', { url: href, newTab: true }); return; }
    try {
      el.focus({ preventScroll: true });
    } catch (e) {}
    el.click();
  }

  /* -------------------------------------------------------------- palette */
  function paletteKindMeta(kind) {
    switch (kind) {
      case 'command': return { sig: ':', ph: 'command', meta: '' };
      case 'find': return { sig: '/', ph: 'find in page', meta: '' };
      case 'bookmark': return { sig: '#', ph: 'bookmarks', meta: '' };
      case 'history': return { sig: '>', ph: 'history', meta: '' };
      case 'ext': return { sig: '+', ph: 'extensions', meta: '' };
      default: return { sig: ':', ph: 'open url, search, or pick', meta: '' };
    }
  }

  function openPalette(kind, prefill, forceNewTab) {
    if (!UI && !ensureUI()) return;
    cancelHint();
    S.cmd = {
      open: true, kind, text: prefill || '', items: [], sel: 0,
      forceNewTab: !!forceNewTab, timer: 0, req: 0,
    };
    const meta = paletteKindMeta(kind);
    psig.textContent = meta.sig;
    pinput.placeholder = meta.ph;
    pinput.value = prefill || '';
    pal.classList.add('on');
    setMode('command');
    showBar(true);
    setTimeout(() => {
      pinput.focus();
      pinput.setSelectionRange(pinput.value.length, pinput.value.length);
    }, 0);
    paletteChanged();
  }

  function closePalette() {
    if (!S.cmd || !S.cmd.open) return;
    S.cmd.open = false;
    S.cmd = null;
    pal.classList.remove('on');
    plist.innerHTML = '';
    pmeta.textContent = '';
    pinput.value = '';
    pinput.blur();
    if (S.mode === 'command') setMode('normal');
    unpin();
  }

  function paletteChanged() {
    if (!S.cmd || !S.cmd.open) return;
    S.cmd.text = pinput.value;
    S.cmd.sel = 0;
    clearTimeout(S.cmd.timer);
    const my = ++S.cmd.req;
    S.cmd.timer = setTimeout(() => {
      const kind = S.cmd.kind === 'open' || S.cmd.kind === 'find' ? 'open' : S.cmd.kind;
      send('complete', { kind: S.cmd.kind === 'open' ? 'open' : kind, query: pinput.value, limit: 14 })
        .then((res) => {
          if (!S.cmd || !S.cmd.open || my !== S.cmd.req) return;
          S.cmd.items = (res && res.items) || [];
          if (S.cmd.kind === 'find') S.cmd.items = [];
          paletteRender();
        });
      if (S.cmd.kind === 'find') paletteRender();
    }, 45);
  }

  function paletteRender() {
    if (!S.cmd) return;
    const items = S.cmd.items;
    const showKind = S.cmd.kind === 'open';
    let h = '';
    items.forEach((it, i) => {
      const cls = i === S.cmd.sel ? ' sel' : '';
      const label = it.kind === 'command' ? it.text : (it.text || it.detail || '');
      h += '<div class="it' + cls + '" data-i="' + i + '">' +
        '<span class="mk">' + (i === S.cmd.sel ? '>' : '') + '</span>' +
        (showKind ? '<span class="k">' + esc(it.kind || '') + '</span>' : '') +
        '<span class="t">' + esc(label) + '</span>' +
        '<span class="d">' + esc(it.detail || '') + '</span></div>';
    });
    plist.innerHTML = h;
    pmeta.textContent = items.length ? (S.cmd.sel + 1) + '/' + items.length : '';
    const sel = plist.querySelector('.it.sel');
    if (sel) sel.scrollIntoView({ block: 'nearest' });
  }

  function esc(s) {
    return String(s == null ? '' : s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  }

  function paletteMove(d) {
    if (!S.cmd || !S.cmd.items.length) return;
    S.cmd.sel = (S.cmd.sel + d + S.cmd.items.length) % S.cmd.items.length;
    paletteRender();
  }

  function paletteEdit(op) {
    const el = pinput;
    let v = el.value;
    let a = el.selectionStart === null ? v.length : el.selectionStart;
    let b = el.selectionEnd === null ? v.length : el.selectionEnd;
    if (a !== b && op !== 'clear' && op !== 'backspace') { a = Math.min(a, b); b = a; }
    switch (op) {
      case 'clear': v = ''; a = b = 0; break;
      case 'backspace':
        if (a === b && a > 0) { v = v.slice(0, a - 1) + v.slice(b); a = b = a - 1; }
        else { v = v.slice(0, a) + v.slice(b); b = a; }
        break;
      case 'worddel': {
        const left = v.slice(0, a);
        const m = left.match(/(\S+\s*|\s+)$/);
        const cut = m ? a - m[0].length : 0;
        v = v.slice(0, cut) + v.slice(b); a = b = cut; break;
      }
      case 'left': a = b = Math.max(0, (a === b ? a : a) - 1); if (a !== b) b = a; else a = b = Math.max(0, el.selectionStart - 1); break;
      case 'right': a = b = Math.min(v.length, (el.selectionStart || 0) + 1); break;
      case 'home': a = b = 0; break;
      case 'end': a = b = v.length; break;
      default: return;
    }
    el.value = v;
    el.setSelectionRange(a, b);
    paletteChanged();
  }

  function paletteAccept(newTab) {
    if (!S.cmd) return;
    const cmd = S.cmd;
    const raw = pinput.value.trim();
    const picked = cmd.items[cmd.sel];
    closePalette();

    if (cmd.kind === 'find') {
      findStep(1, raw);
      return;
    }
    if (cmd.kind === 'command') {
      const name = (picked && picked.kind === 'command' ? picked.text : raw).trim();
      runCommand(name);
      return;
    }
    // open / bookmark / history
    let target = raw;
    if (picked && (picked.kind === 'bookmark' || picked.kind === 'history' || picked.kind === 'tab' || picked.kind === 'ext')) {
      target = picked.detail || picked.text;
      if (picked.kind === 'tab' && typeof picked.index === 'number') {
        tell('tab', { op: 'select', index: picked.index });
        return;
      }
      if (!raw) target = picked.detail || picked.text;
      else if (picked.kind !== 'search' && picked.kind !== 'engine') target = picked.detail || target;
    }
    if (!target) return;
    const wantNew = newTab || cmd.forceNewTab;
    tell('nav', { url: target, newTab: wantNew });
  }

  function runCommand(name) {
    if (!name) return;
    const map = {
      open: () => openPalette('open', ''),
      'open-tab': () => openPalette('open', '', true),
      'tab-new': () => tell('tab', { op: 'new', url: '' }),
      'tab-close': () => tell('tab', { op: 'close' }),
      'tab-next': () => tell('tab', { op: 'next' }),
      'tab-prev': () => tell('tab', { op: 'prev' }),
      'tab-undo': () => tell('tab', { op: 'undo' }),
      q: () => tell('quit', {}),
      qall: () => tell('quit', {}),
      reload: () => tell('history', { op: 'reload' }),
      'reload-hard': () => tell('history', { op: 'hardreload' }),
      back: () => tell('history', { op: 'back' }),
      forward: () => tell('history', { op: 'forward' }),
      home: () => tell('nav', { url: '' }),
      'zoom-in': () => stepZoom(1),
      'zoom-out': () => stepZoom(-1),
      'zoom-reset': () => setZoom(1),
      images: () => toggleImages(),
      hint: () => startHint({ newTab: false, yank: false }),
      find: () => openPalette('find', ''),
      yank: () => tell('yank', { text: location.href }),
      'yank-title': () => tell('yank', { text: document.title }),
      bookmark: () => tell('bookmark', { op: 'toggle', url: location.href, title: document.title }),
      bookmarks: () => openPalette('bookmark', ''),
      history: () => openPalette('history', ''),
      extensions: () => openPalette('ext', ''),
      'session-save': () => toast('session is saved continuously'),
      private: () => tell('nav', { url: '', newTab: true, private: true }),
      external: () => tell('external', { url: location.href }),
      help: () => toggleHelp(true),
    };
    // URL-ish input in the command line opens directly.
    if (!map[name] && /[.:/]/.test(name)) { tell('nav', { url: name }); return; }
    (map[name] || (() => toast('unknown command: ' + name)))();
  }

  // Element-bound listeners, installed once the UI exists.
  function bindUI() {
    pinput.addEventListener('keydown', (e) => {
      if (e.isComposing) return;
      const t = tok(e);
      const ck = S.cfg.keymap.command || {};
      if (t && ck[t]) {
        e.preventDefault();
        e.stopPropagation();
        dispatch(ck[t], 1);
        return;
      }
      if (e.key === 'Escape') { e.preventDefault(); closePalette(); return; }
      if (e.key === 'Enter') { e.preventDefault(); paletteAccept(false); return; }
      if (e.key === 'Tab') { e.preventDefault(); paletteMove(e.shiftKey ? -1 : 1); return; }
    }, true);
    pinput.addEventListener('input', paletteChanged);

    plist.addEventListener('mousedown', (e) => {
      const it = e.target.closest('.it');
      if (!it) return;
      e.preventDefault();
      S.cmd.sel = parseInt(it.dataset.i, 10) || 0;
      paletteAccept(false);
    });
    tabsEl.addEventListener('mousedown', (e) => {
      const t = e.target.closest('.tab');
      if (!t) return;
      e.preventDefault();
      tell('tab', { op: 'select', index: parseInt(t.dataset.i, 10) || 0 });
    });
  }

  /* ----------------------------------------------------------------- find */
  function findStep(dir, term) {
    const q = term !== undefined ? term : (S.find.last || '');
    if (!q) { openPalette('find', ''); return; }
    S.find.last = q;
    S.find.dir = dir;
    let found;
    try {
      found = window.find(q, false, dir < 0, true, false, true, false);
    } catch (e) {
      found = false;
    }
    if (!found) toast('no match: ' + q);
    else showBar();
  }

  /* ----------------------------------------------------------------- help */
  let helpOpen = false;
  function toggleHelp(force) {
    if (!UI && !ensureUI()) return;
    helpOpen = force === undefined ? !helpOpen : !!force;
    if (!helpOpen) { helpEl.classList.remove('on'); return; }
    const modes = Object.keys(S.cfg.keymap)
      .filter((m) => m === 'normal' || m === 'insert')
      .sort((a, b) => (a === 'normal' ? -1 : b === 'normal' ? 1 : 0));
    let h = '<div class="head"><span><b>kite ' + esc(VERSION) + '</b> — keys</span>' +
      '<span>esc / q close</span></div><div class="body">';
    for (const m of modes) {
      h += '<h2>' + esc(m) + '</h2><div class="grid">';
      const entries = Object.entries(S.cfg.keymap[m] || {});
      entries.sort((a, b) => a[1].localeCompare(b[1]));
      for (const [k, a] of entries) {
        h += '<div class="row"><span class="k">' +
          k.split(' ').map((x) => '<kbd>' + esc(x) + '</kbd>').join('') +
          '</span><span class="a">' + esc(a) + '</span></div>';
      }
      h += '</div>';
    }
    h += '</div>';
    helpEl.innerHTML = h;
    helpEl.classList.add('on');
  }

  // The single keyboard entry point. Capture phase, on window: we see every key
  // before the page does, and only keys we actually bind get preventDefault().
  window.addEventListener('keydown', onKeyDown, true);

  /* ---------------------------------------------------------- engine glue */
  window.__kite = {
    version: VERSION,
    boot: (cfg) => {
      if (!cfg) return;
      if (!ensureUI()) return;
      S.cfg = {
        settings: Object.assign({ bar: 'auto', status: true, hints_chars: 'asdfghjkl;', scroll_step: 84, images: true, font: '' }, cfg.settings || {}),
        keymap: cfg.keymap || {},
        search: cfg.search || {},
      };
      if (cfg.tabs) S.tabs = cfg.tabs;
      if (typeof cfg.activeIndex === 'number') S.activeIndex = cfg.activeIndex;
      if (cfg.url) S.url = cfg.url;
      if (typeof cfg.bookmarked === 'boolean') S.bookmarked = cfg.bookmarked;
      if (cfg.home) S.homeUrl = cfg.home;
      if (S.cfg.settings.font) HOST.style.setProperty('--kite-font', S.cfg.settings.font);
      if (S.cfg.settings.images === false) applyImages(true, true);
      S.zoom = cfg.settings && cfg.settings.zoom ? cfg.settings.zoom : 1;
      if (S.zoom !== 1) setZoom(S.zoom);
      setLoading(false);
      renderBar();
      if (S.cfg.settings.bar === 'always') chromeEl.classList.add('on');
      else showBar();
      S.home = location.href;
    },
    state: (st) => {
      if (!st) return;
      if (!ensureUI()) return;
      if (st.tabs) S.tabs = st.tabs;
      if (typeof st.activeIndex === 'number') S.activeIndex = st.activeIndex;
      if (st.url) S.url = st.url;
      if (typeof st.loading === 'boolean') setLoading(st.loading);
      if (typeof st.bookmarked === 'boolean') S.bookmarked = st.bookmarked;
      if (typeof st.private === 'boolean') S.private = st.private;
      renderBar();
      showBar();
    },
    toast: (msg) => toast(msg),
    recv: (id, body) => {
      const fn = pending.get(id);
      if (!fn) return;
      pending.delete(id);
      fn(body);
    },
    // test/introspection hook — not used by the UI itself
    _state: () => ({ mode: S.mode, url: S.url, tabs: S.tabs.length, active: S.activeIndex, hint: S.hint ? S.hint.typed : null, hints: (hintLayer && hintLayer.children) ? hintLayer.children.length : 0, err: S.lastError || null, key: S.lastKey || null }),
    _cfg: () => ({
      modes: Object.keys(S.cfg.keymap || {}),
      normal: Object.keys((S.cfg.keymap || {}).normal || {}).slice(0, 10),
      settings: S.cfg.settings,
    }),
    _palette: () => !!S.cmd,
    _lookup: (t) => { const h = lookup([t]); return h ? (h.exact || 'prefix') : null; },
    _nkeys: () => (Object.keys((S.cfg.keymap || {}).normal || {}).length),
  };

  /* ------------------------------------------------------------- lifecycle */
  function onLoad() {
    if (!ensureUI()) { setTimeout(onLoad, 80); return; }
    S.loaded = true;
    setLoading(false);
    tell('title', { title: document.title });
    tell('ready', {});
    showBar();
  }

  if (document.readyState === 'complete') onLoad();
  else window.addEventListener('load', onLoad, { once: true });
  document.addEventListener('DOMContentLoaded', () => {
    ensureUI();
    if (S.cfg.settings.bar === 'always' && UI) chromeEl.classList.add('on');
  }, { once: true });

  // Scroll hides hints (labels are viewport-positioned).
  window.addEventListener('scroll', () => {
    if (S.hint) cancelHint();
    if (S.mode === 'normal') maybeHideBar();
  }, { passive: true, capture: true });

  // Auto insert-mode when a field takes focus.
  document.addEventListener('focusin', (e) => {
    if (S.mode === 'command') return;
    if (isEditable(e.target) && S.mode !== 'insert') {
      S.mode = 'insert';
      renderBar();
      showBar(true);
    }
  }, true);
  document.addEventListener('focusout', () => {
    if (S.mode === 'insert' && !isEditable(document.activeElement)) setMode('normal');
  }, true);

  window.addEventListener('beforeunload', () => { tell('title', { title: document.title }); });

  // Ask the controller for config + tab state on every fresh document.
  tell('hello', {});
  setTimeout(() => { if (!S.cfg.keymap || !Object.keys(S.cfg.keymap).length) tell('hello', {}); }, 400);
})();
