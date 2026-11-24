/* terminal.js —— W3 终端远控管理器（亮色会话列表 + 暗色 VT100 交互终端，零 innerHTML，零第三方依赖）
 *
 * 架构与约束：
 *   - 视图 1（list）：展示远程设备状态、创建后台会话按钮与会话卡片列表（打开终端 / ... 重命名 & 终止会话）；
 *   - 视图 2（terminal）：完整 VT100/ANSI 终端模拟器（SGR 256/24bit 真彩色、?1049h/l 备选屏、CJK 双倍字符宽、
 *     括号粘贴 ?2004h/l、中文 IME 组合输入、100ms 防抖 Resize），支持返回会话列表或直接切换其他后台会话；
 *   - 关闭窗口或返回列表仅执行 detach（暂停实时流），不会终止远端后台 PTY，仅在点击「终止会话」时关闭 PTY；
 *   - 严禁使用 innerHTML / insertAdjacentHTML：所有节点一律经 createElement / createElementNS + textContent 渲染。
 */

(function () {
  'use strict';

  var Bridge = window.RCBridge;
  var IS_TAURI = Boolean(Bridge && Bridge.isTauri);

  var MAX_LINES = 2000;
  var MAX_SESSIONS = 8;
  var MAX_NAME_CHARS = 64;
  var RESIZE_DEBOUNCE_MS = 100;
  var SVG_NS = 'http://www.w3.org/2000/svg';

  function pad2(n) {
    return n < 10 ? '0' + n : String(n);
  }

  function formatHms(totalSec) {
    var s = Math.max(0, Math.floor(totalSec));
    var h = Math.floor(s / 3600);
    var m = Math.floor((s % 3600) / 60);
    var rem = s % 60;
    return pad2(h) + ':' + pad2(m) + ':' + pad2(rem);
  }

  function formatSessionTime(unixSec) {
    var sec = Number(unixSec);
    if (!sec || !isFinite(sec) || sec <= 0) {
      return '';
    }
    var d = new Date(sec * 1000);
    return (
      d.getFullYear() +
      '-' +
      pad2(d.getMonth() + 1) +
      '-' +
      pad2(d.getDate()) +
      ' ' +
      pad2(d.getHours()) +
      ':' +
      pad2(d.getMinutes())
    );
  }

  function validateSessionName(raw) {
    var trimmed = String(raw || '').trim();
    if (!trimmed) {
      return { ok: false, error: '会话名称不能为空' };
    }
    if (Array.from(trimmed).length > MAX_NAME_CHARS) {
      return { ok: false, error: '会话名称不能超过 ' + MAX_NAME_CHARS + ' 个字符' };
    }
    for (var i = 0; i < trimmed.length; i += 1) {
      var code = trimmed.charCodeAt(i);
      if (code < 0x20 || (code >= 0x7f && code <= 0x9f)) {
        return { ok: false, error: '会话名称不能包含控制字符' };
      }
    }
    return { ok: true, name: trimmed };
  }

  function createMoreIconSvg() {
    var svg = document.createElementNS(SVG_NS, 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('width', '16');
    svg.setAttribute('height', '16');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('fill', 'currentColor');
    [3, 8, 13].forEach(function (cx) {
      var dot = document.createElementNS(SVG_NS, 'circle');
      dot.setAttribute('cx', String(cx));
      dot.setAttribute('cy', '8');
      dot.setAttribute('r', '1.35');
      svg.appendChild(dot);
    });
    return svg;
  }

  var FG_CLASS_BY_CODE = {
    30: 'tm-ansi-fg-muted',
    31: 'tm-ansi-fg-red',
    32: 'tm-ansi-fg-green',
    33: 'tm-ansi-fg-yellow',
    34: 'tm-ansi-fg-blue',
    35: 'tm-ansi-fg-magenta',
    36: 'tm-ansi-fg-cyan',
    37: 'tm-ansi-fg-white',
    90: 'tm-ansi-fg-muted',
    91: 'tm-ansi-fg-red',
    92: 'tm-ansi-fg-green',
    93: 'tm-ansi-fg-yellow',
    94: 'tm-ansi-fg-blue',
    95: 'tm-ansi-fg-magenta',
    96: 'tm-ansi-fg-cyan',
    97: 'tm-ansi-fg-white'
  };

  var BG_CLASS_BY_CODE = {
    41: 'tm-ansi-bg-red',
    42: 'tm-ansi-bg-green',
    43: 'tm-ansi-bg-yellow',
    44: 'tm-ansi-bg-blue',
    101: 'tm-ansi-bg-red',
    102: 'tm-ansi-bg-green',
    103: 'tm-ansi-bg-yellow',
    104: 'tm-ansi-bg-blue'
  };

  var FKEY_SEQ = {
    F1: '\x1bOP',
    F2: '\x1bOQ',
    F3: '\x1bOR',
    F4: '\x1bOS',
    F5: '\x1b[15~',
    F6: '\x1b[17~',
    F7: '\x1b[18~',
    F8: '\x1b[19~',
    F9: '\x1b[20~',
    F10: '\x1b[21~',
    F11: '\x1b[23~',
    F12: '\x1b[24~'
  };

  var ANSI16_COLORS = [
    '#18181b', '#ef4444', '#22c55e', '#eab308', '#3b82f6', '#a855f7', '#06b6d4', '#e4e4e7',
    '#71717a', '#f87171', '#4ade80', '#facc15', '#60a5fa', '#c084fc', '#22d3ee', '#fafafa'
  ];

  function clampByte(v) {
    var n = parseInt(v, 10);
    if (isNaN(n) || n < 0) return 0;
    if (n > 255) return 255;
    return n;
  }

  function ansi256ToCss(idx) {
    var n = clampByte(idx);
    if (n < 16) return ANSI16_COLORS[n];
    if (n >= 232) {
      var gray = 8 + (n - 232) * 10;
      return 'rgb(' + gray + ',' + gray + ',' + gray + ')';
    }
    var c = n - 16;
    var r = Math.floor(c / 36);
    var g = Math.floor((c % 36) / 6);
    var b = c % 6;
    var steps = [0, 95, 135, 175, 215, 255];
    return 'rgb(' + steps[r] + ',' + steps[g] + ',' + steps[b] + ')';
  }

  function emptyStyle() {
    return {
      fg: '',
      bg: '',
      fgCss: '',
      bgCss: '',
      bold: false,
      dim: false,
      italic: false,
      underline: false,
      inverse: false
    };
  }

  function styleClassName(st) {
    var cls = [];
    if (st.bold) cls.push('tm-ansi-bold');
    if (st.dim) cls.push('tm-ansi-dim');
    if (st.italic) cls.push('tm-ansi-italic');
    if (st.underline) cls.push('tm-ansi-underline');
    if (st.inverse) cls.push('tm-ansi-inverse');
    if (!st.fgCss && st.fg) cls.push(st.fg);
    if (!st.bgCss && st.bg) cls.push(st.bg);
    return cls.join(' ');
  }

  function makeStyleToken(st) {
    var cls = styleClassName(st);
    if (!cls && !st.fgCss && !st.bgCss) return '';
    return cls + '|' + (st.fgCss || '') + '|' + (st.bgCss || '');
  }

  function parseStyleToken(token) {
    if (!token) return null;
    var parts = token.split('|');
    return {
      cls: parts[0] || '',
      fgCss: parts[1] || '',
      bgCss: parts[2] || ''
    };
  }

  function charDisplayWidth(ch) {
    if (!ch) return 0;
    var cp = ch.codePointAt ? ch.codePointAt(0) : ch.charCodeAt(0);
    if (cp == null || cp < 0x20 || (cp >= 0x7f && cp < 0xa0)) return 0;
    if (
      (cp >= 0x0300 && cp <= 0x036f) ||
      (cp >= 0x1ab0 && cp <= 0x1aff) ||
      (cp >= 0x20d0 && cp <= 0x20ff) ||
      (cp >= 0xfe20 && cp <= 0xfe2f)
    ) {
      return 0;
    }
    if (
      cp >= 0x1100 &&
      (cp <= 0x115f ||
        cp === 0x2329 ||
        cp === 0x232a ||
        (cp >= 0x2e80 && cp <= 0x303e) ||
        (cp >= 0x3040 && cp <= 0x3247) ||
        (cp >= 0x3250 && cp <= 0x4dbf) ||
        (cp >= 0x4e00 && cp <= 0xa4cf) ||
        (cp >= 0xa960 && cp <= 0xa97f) ||
        (cp >= 0xac00 && cp <= 0xd7a3) ||
        (cp >= 0xf900 && cp <= 0xfaff) ||
        (cp >= 0xfe10 && cp <= 0xfe19) ||
        (cp >= 0xfe30 && cp <= 0xfe6f) ||
        (cp >= 0xff00 && cp <= 0xff60) ||
        (cp >= 0xffe0 && cp <= 0xffe6) ||
        (cp >= 0x1f300 && cp <= 0x1faff) ||
        (cp >= 0x20000 && cp <= 0x3fffd))
    ) {
      return 2;
    }
    return 1;
  }

  function makeLine(system) {
    return {
      chars: [],
      styles: [],
      dirty: true,
      system: Boolean(system),
      node: null
    };
  }

  function errMessage(err, fallback) {
    if (!err) return fallback || '操作失败';
    if (typeof err === 'string') return err;
    if (err.message) return String(err.message);
    return String(err);
  }

  /* ── 浏览器静态预览模拟传输层（!IS_TAURI） ───────────────── */
  function createBrowserMockTransport(emitEvent) {
    var nextId = 2;
    var nextSeq = 2;
    var mockList = [
      {
        termId: 1,
        name: 'session1',
        title: 'zsh',
        cwd: '/Users/yii',
        createdAtUnixSec: Math.floor(new Date(2026, 10, 21, 16, 45, 0).getTime() / 1000),
        cols: 80,
        rows: 24,
        attached: false,
        scrollback: 'yii@MacBook-Pro /Users/yii ~ > ',
        lineBuf: ''
      }
    ];

    function clonePublic(s) {
      return {
        termId: s.termId,
        name: s.name,
        title: s.title,
        cwd: s.cwd,
        createdAtUnixSec: s.createdAtUnixSec,
        cols: s.cols,
        rows: s.rows,
        attached: s.attached
      };
    }

    function findById(id) {
      for (var i = 0; i < mockList.length; i += 1) {
        if (mockList[i].termId === id) return mockList[i];
      }
      return null;
    }

    return {
      list: function () {
        window.setTimeout(function () {
          emitEvent({
            type: 'sessionList',
            sessions: mockList.map(clonePublic)
          });
        }, 0);
        return Promise.resolve();
      },
      open: function (cols, rows, name, attach) {
        if (mockList.length >= MAX_SESSIONS) {
          window.setTimeout(function () {
            emitEvent({
              type: 'opened',
              termId: 0,
              name: '',
              createdAtUnixSec: 0,
              title: '',
              cwd: '',
              attached: false,
              ok: false,
              error: '最多同时开启 ' + MAX_SESSIONS + ' 个终端会话'
            });
          }, 0);
          return Promise.resolve(0);
        }
        var termId = nextId++;
        var assignedName = name || 'session' + nextSeq++;
        var nowSec = Math.floor(Date.now() / 1000);
        var rec = {
          termId: termId,
          name: assignedName,
          title: 'zsh',
          cwd: '/Users/yii',
          createdAtUnixSec: nowSec,
          cols: cols,
          rows: rows,
          attached: Boolean(attach),
          scrollback: 'yii@MacBook-Pro /Users/yii ~ > ',
          lineBuf: ''
        };
        mockList.push(rec);
        window.setTimeout(function () {
          emitEvent({
            type: 'opened',
            termId: rec.termId,
            name: rec.name,
            createdAtUnixSec: rec.createdAtUnixSec,
            title: rec.title,
            cwd: rec.cwd,
            attached: rec.attached,
            ok: true,
            error: null
          });
          if (attach) {
            emitEvent({
              type: 'output',
              termId: rec.termId,
              textUtf8Lossy: rec.scrollback,
              bytes: []
            });
          }
        }, 0);
        return Promise.resolve(attach ? termId : 0);
      },
      attach: function (termId, cols, rows) {
        var rec = findById(termId);
        window.setTimeout(function () {
          if (!rec) {
            emitEvent({
              type: 'attached',
              termId: termId,
              session: null,
              ok: false,
              error: '终端会话不存在或已终止'
            });
            return;
          }
          rec.cols = cols;
          rec.rows = rows;
          rec.attached = true;
          emitEvent({
            type: 'attached',
            termId: termId,
            session: clonePublic(rec),
            ok: true,
            error: null
          });
          if (rec.scrollback) {
            emitEvent({
              type: 'output',
              termId: termId,
              textUtf8Lossy: rec.scrollback,
              bytes: []
            });
          }
        }, 0);
        return Promise.resolve();
      },
      detach: function (termId) {
        var rec = findById(termId);
        if (rec) rec.attached = false;
        return Promise.resolve();
      },
      rename: function (termId, name) {
        var check = validateSessionName(name);
        var rec = findById(termId);
        window.setTimeout(function () {
          if (!check.ok || !rec) {
            emitEvent({
              type: 'renamed',
              termId: termId,
              name: '',
              ok: false,
              error: !check.ok ? check.error : '终端会话不存在或已终止'
            });
            return;
          }
          rec.name = check.name;
          emitEvent({
            type: 'renamed',
            termId: termId,
            name: check.name,
            ok: true,
            error: null
          });
        }, 0);
        return Promise.resolve();
      },
      input: function (termId, data) {
        var rec = findById(termId);
        if (!rec) return Promise.resolve();
        var echo = '';
        for (var i = 0; i < data.length; i += 1) {
          var ch = data.charAt(i);
          if (ch === '\r' || ch === '\n') {
            var cmd = rec.lineBuf.trim();
            rec.lineBuf = '';
            echo += '\r\n' + (cmd ? cmd + '\r\n' : '') + 'yii@MacBook-Pro /Users/yii ~ > ';
          } else if (ch === '\x7f') {
            if (rec.lineBuf.length > 0) {
              rec.lineBuf = rec.lineBuf.slice(0, -1);
              echo += '\b \b';
            }
          } else if (ch >= ' ') {
            rec.lineBuf += ch;
            echo += ch;
          }
        }
        if (echo) {
          rec.scrollback += echo;
          if (rec.attached) {
            emitEvent({
              type: 'output',
              termId: termId,
              textUtf8Lossy: echo,
              bytes: []
            });
          }
        }
        return Promise.resolve();
      },
      resize: function (termId, cols, rows) {
        var rec = findById(termId);
        if (rec) {
          rec.cols = cols;
          rec.rows = rows;
        }
        return Promise.resolve();
      },
      close: function (termId) {
        mockList = mockList.filter(function (item) {
          return item.termId !== termId;
        });
        window.setTimeout(function () {
          emitEvent({
            type: 'exited',
            termId: termId,
            code: 0
          });
        }, 0);
        return Promise.resolve();
      }
    };
  }

  function init() {
    var winEl = document.getElementById('terminal-window');
    var titleEl = document.getElementById('terminal-title');
    var backBtn = document.getElementById('tm-back-to-list');

    var listViewEl = document.getElementById('terminal-list-view');
    var deviceNameEl = document.getElementById('tm-device-name');
    var mgrDotEl = document.getElementById('tm-mgr-dot');
    var mgrStatusTextEl = document.getElementById('tm-mgr-status-text');
    var createBtn = document.getElementById('tm-create-session');
    var sessionListEl = document.getElementById('tm-session-list');
    var emptyStateEl = document.getElementById('tm-empty-state');
    var emptyTextEl = document.getElementById('tm-empty-text');

    var liveViewEl = document.getElementById('terminal-live-view');
    var tabsWrapEl = document.getElementById('terminal-tabs');
    var tabListEl = document.getElementById('terminal-tab-list');
    var newTabBtn = document.getElementById('terminal-new-tab');
    var timerEl = document.getElementById('terminal-timer');
    var statusDotEl = document.getElementById('terminal-status-dot');
    var statusTextEl = document.getElementById('terminal-status-text');
    var cwdEl = document.getElementById('terminal-cwd');
    var liveMoreBtn = document.getElementById('tm-live-more');
    var reconnectBtn = document.getElementById('terminal-reconnect');
    var screenEl = document.getElementById('terminal-screen');
    var imeEl = document.getElementById('terminal-ime');
    var noticeEl = document.getElementById('terminal-notice');
    var noticeTextEl = document.getElementById('terminal-notice-text');
    var noticeBackBtn = document.getElementById('terminal-notice-back');
    var noticeReconnectBtn = document.getElementById('terminal-notice-reconnect');

    var moreMenuEl = document.getElementById('tm-more-menu');
    var menuRenameBtn = document.getElementById('tm-menu-rename');
    var menuTerminateBtn = document.getElementById('tm-menu-terminate');

    var renameBackdropEl = document.getElementById('tm-rename-backdrop');
    var renameFormEl = document.getElementById('tm-rename-form');
    var renameInputEl = document.getElementById('tm-rename-input');
    var renameErrorEl = document.getElementById('tm-rename-error');
    var renameCancelBtn = document.getElementById('tm-rename-cancel');
    var renameConfirmBtn = document.getElementById('tm-rename-confirm');

    var toastEl = document.getElementById('tm-toast');

    if (!winEl || !listViewEl || !liveViewEl || !screenEl) return;

    /* ── 状态管理 ──────────────────────────────────────────── */
    var sessionState = {
      active: !IS_TAURI,
      connected: !IS_TAURI,
      watchOnly: false,
      deviceName: IS_TAURI ? '' : 'yii’s MacBook Pro'
    };
    var lastEndedReason = '';
    var viewMode = 'list'; // 'list' | 'terminal'
    var sessions = []; // [{ termId, name, title, cwd, createdAtUnixSec, cols, rows, attached }]
    var tabByTermId = Object.create(null);
    var activeTermId = null;

    var bgCreating = false;
    var bgCreateTimer = 0;
    var attachCreating = false;
    var attachCreateTimer = 0;
    var pendingAttachViewTarget = false;
    var pendingOpenKinds = [];

    function removePendingOpenKind(kind) {
      var idx = pendingOpenKinds.indexOf(kind);
      if (idx !== -1) pendingOpenKinds.splice(idx, 1);
    }

    var attachSeq = 0;
    var menuTargetTermId = null;
    var menuAnchorBtn = null;
    var renameTargetTermId = null;
    var renameInFlight = false;

    var elapsedSec = 0;
    var timerHandle = 0;
    var toastTimer = 0;

    var lastCols = 80;
    var lastRows = 24;
    var resizeTimer = 0;
    var rafId = 0;
    var renderPending = false;
    var fullRebuild = true;
    var removedNodes = [];
    var prevCursorLineIdx = -1;
    var prevCursorCol = -1;
    var prevCursorVisible = false;
    var isComposing = false;

    var Api = IS_TAURI
      ? {
          list: function () {
            return Bridge.terminalList();
          },
          open: function (cols, rows, name, attach) {
            return Bridge.terminalOpen(cols, rows, name, attach);
          },
          attach: function (termId, cols, rows) {
            return Bridge.terminalAttach(termId, cols, rows);
          },
          detach: function (termId) {
            return Bridge.terminalDetach(termId);
          },
          rename: function (termId, name) {
            return Bridge.terminalRename(termId, name);
          },
          input: function (termId, data) {
            return Bridge.terminalInput(termId, data);
          },
          resize: function (termId, cols, rows) {
            return Bridge.terminalResize(termId, cols, rows);
          },
          close: function (termId) {
            return Bridge.terminalClose(termId);
          }
        }
      : createBrowserMockTransport(function (ev) {
          handleTerminalEvent(ev);
        });

    function showToast(msg) {
      if (!toastEl || !msg) return;
      toastEl.textContent = String(msg);
      toastEl.classList.add('is-visible');
      window.clearTimeout(toastTimer);
      toastTimer = window.setTimeout(function () {
        toastEl.classList.remove('is-visible');
      }, 2000);
    }

    function isSessionReady() {
      return Boolean(sessionState.active && sessionState.connected && !sessionState.watchOnly);
    }

    function findSession(termId) {
      for (var i = 0; i < sessions.length; i += 1) {
        if (sessions[i].termId === termId) return sessions[i];
      }
      return null;
    }

    function upsertSession(rec) {
      if (!rec || rec.termId == null) return null;
      var existing = findSession(rec.termId);
      if (existing) {
        if (rec.name) existing.name = String(rec.name);
        if (rec.title) existing.title = String(rec.title);
        if (rec.cwd) existing.cwd = String(rec.cwd);
        if (rec.createdAtUnixSec) existing.createdAtUnixSec = Number(rec.createdAtUnixSec);
        if (rec.cols) existing.cols = Number(rec.cols);
        if (rec.rows) existing.rows = Number(rec.rows);
        if (typeof rec.attached === 'boolean') existing.attached = rec.attached;
        return existing;
      }
      var item = {
        termId: Number(rec.termId),
        name: String(rec.name || 'session' + rec.termId),
        title: String(rec.title || ''),
        cwd: String(rec.cwd || ''),
        createdAtUnixSec: Number(rec.createdAtUnixSec || 0),
        cols: Number(rec.cols || 80),
        rows: Number(rec.rows || 24),
        attached: Boolean(rec.attached)
      };
      sessions.push(item);
      sessions.sort(function (a, b) {
        if (a.createdAtUnixSec !== b.createdAtUnixSec) {
          return a.createdAtUnixSec - b.createdAtUnixSec;
        }
        return a.termId - b.termId;
      });
      return item;
    }

    function ensureTab(termId) {
      var key = String(termId);
      var existing = tabByTermId[key];
      if (existing) return existing;
      var sess = findSession(termId);
      var tab = {
        termId: Number(termId),
        name: sess ? sess.name : 'session' + termId,
        opening: false,
        connected: false,
        disconnectNoticed: false,
        title: sess ? sess.title : '',
        cwd: sess ? sess.cwd : '',
        lines: [makeLine(false)],
        altLines: null,
        savedMainState: null,
        inAltScreen: false,
        scrollTop: 0,
        scrollBottom: -1,
        curLineIdx: 0,
        curCol: 0,
        savedCursor: null,
        cursorVisible: true,
        bracketedPaste: false,
        lastBsCol: -1,
        lastBsSpaceCol: -1,
        curStyle: emptyStyle(),
        curStyleToken: '',
        parseState: 'normal',
        csiBuf: '',
        oscBuf: '',
        pendingSurrogate: '',
        utf8Decoder: typeof TextDecoder === 'function' ? new TextDecoder('utf-8', { fatal: false }) : null
      };
      tabByTermId[key] = tab;
      return tab;
    }

    function getActiveTab() {
      if (activeTermId == null) return null;
      return tabByTermId[String(activeTermId)] || null;
    }

    function cancelPendingRender() {
      renderPending = false;
      if (rafId) {
        window.cancelAnimationFrame(rafId);
        rafId = 0;
      }
    }

    function resetTabBuffer(tab, skipRender) {
      tab.lines = [makeLine(false)];
      tab.altLines = null;
      tab.savedMainState = null;
      tab.inAltScreen = false;
      tab.scrollTop = 0;
      tab.scrollBottom = -1;
      tab.curLineIdx = 0;
      tab.curCol = 0;
      tab.savedCursor = null;
      tab.cursorVisible = true;
      tab.bracketedPaste = false;
      tab.lastBsCol = -1;
      tab.lastBsSpaceCol = -1;
      tab.curStyle = emptyStyle();
      tab.curStyleToken = '';
      tab.parseState = 'normal';
      tab.csiBuf = '';
      tab.oscBuf = '';
      tab.pendingSurrogate = '';
      if (typeof TextDecoder === 'function') {
        tab.utf8Decoder = new TextDecoder('utf-8', { fatal: false });
      }
      if (tab.termId === activeTermId) {
        removedNodes = [];
        fullRebuild = true;
        prevCursorLineIdx = -1;
        prevCursorCol = -1;
        prevCursorVisible = false;
        if (!skipRender) {
          scheduleRender();
        }
      }
    }

    /* ── 独立计时器 ─────────────────────────────────────────── */

    function syncTimerRunning() {
      var tab = getActiveTab();
      var running = Boolean(viewMode === 'terminal' && tab && tab.connected);
      if (running && !timerHandle) {
        timerHandle = window.setInterval(function () {
          elapsedSec += 1;
          if (timerEl) timerEl.textContent = formatHms(elapsedSec);
        }, 1000);
      } else if (!running && timerHandle) {
        window.clearInterval(timerHandle);
        timerHandle = 0;
      }
    }

    function resetSessionTimer() {
      if (timerHandle) {
        window.clearInterval(timerHandle);
        timerHandle = 0;
      }
      elapsedSec = 0;
      if (timerEl) timerEl.textContent = '00:00:00';
    }

    /* ── 视图切换与会话管理列表渲染 ─────────────────────────── */

    function setView(nextView) {
      viewMode = nextView === 'terminal' ? 'terminal' : 'list';
      winEl.setAttribute('data-view', viewMode);
      if (viewMode === 'terminal') {
        listViewEl.hidden = true;
        liveViewEl.hidden = false;
        if (backBtn) backBtn.hidden = false;
        syncLiveChrome();
      } else {
        liveViewEl.hidden = true;
        listViewEl.hidden = false;
        if (backBtn) backBtn.hidden = true;
        syncTimerRunning();
        renderManagerView();
      }
    }

    function renderManagerView() {
      if (deviceNameEl) {
        deviceNameEl.textContent = sessionState.deviceName || '远程设备';
      }

      var ready = isSessionReady();
      if (mgrDotEl && mgrStatusTextEl) {
        mgrDotEl.classList.remove('is-online', 'is-warning');
        if (ready) {
          mgrDotEl.classList.add('is-online');
          mgrStatusTextEl.textContent = '已连接';
        } else if (sessionState.watchOnly) {
          mgrDotEl.classList.add('is-warning');
          mgrStatusTextEl.textContent = '只读观看模式';
        } else if (sessionState.active && !sessionState.connected) {
          mgrDotEl.classList.add('is-warning');
          mgrStatusTextEl.textContent = '正在连接…';
        } else {
          mgrStatusTextEl.textContent = '未连接';
        }
      }

      if (titleEl && viewMode === 'list') {
        titleEl.textContent = '终端远控';
        document.title = '终端远控 — RabbitControl';
      }

      if (createBtn) {
        createBtn.disabled = !ready || bgCreating || sessions.length >= MAX_SESSIONS;
        createBtn.textContent = bgCreating ? '创建中…' : '创建会话';
      }

      if (!sessionListEl || !emptyStateEl) return;
      sessionListEl.textContent = '';

      if (!sessions.length) {
        sessionListEl.hidden = true;
        emptyStateEl.hidden = false;
        if (emptyTextEl) {
          if (sessionState.watchOnly) {
            emptyTextEl.textContent = '当前为只读观看模式，无法管理或打开远程终端';
          } else if (sessionState.active && !sessionState.connected) {
            emptyTextEl.textContent = '正在建立远控连接，请稍候…';
          } else if (!sessionState.active) {
            emptyTextEl.textContent = lastEndedReason || '请先在主窗口或远程桌面窗口建立远控会话';
          } else {
            emptyTextEl.textContent = '暂无运行中的终端会话，点击右上角「创建会话」开始';
          }
        }
        return;
      }

      emptyStateEl.hidden = true;
      sessionListEl.hidden = false;

      var frag = document.createDocumentFragment();
      sessions.forEach(function (sess) {
        var card = document.createElement('div');
        card.className = 'tm-session-card';
        card.setAttribute('role', 'listitem');
        card.setAttribute('data-term-id', String(sess.termId));

        var meta = document.createElement('div');
        meta.className = 'tm-session-card__meta';

        var nameRow = document.createElement('div');
        nameRow.className = 'tm-session-card__name-row';

        var nameEl = document.createElement('span');
        nameEl.className = 'tm-session-card__name';
        nameEl.textContent = sess.name || 'session' + sess.termId;
        nameRow.appendChild(nameEl);
        meta.appendChild(nameRow);

        var timeText = formatSessionTime(sess.createdAtUnixSec);
        if (timeText) {
          var timeEl = document.createElement('span');
          timeEl.className = 'tm-session-card__time';
          timeEl.textContent = timeText;
          meta.appendChild(timeEl);
        }

        var actions = document.createElement('div');
        actions.className = 'tm-session-card__actions';

        var openBtn = document.createElement('button');
        openBtn.type = 'button';
        openBtn.className = 'tm-card-open-btn';
        openBtn.textContent = '打开终端';
        openBtn.disabled = !ready;
        openBtn.addEventListener('click', function () {
          openSessionTerminal(sess.termId);
        });
        actions.appendChild(openBtn);

        var moreBtn = document.createElement('button');
        moreBtn.type = 'button';
        moreBtn.className = 'tm-card-more-btn';
        moreBtn.setAttribute('aria-label', '会话操作');
        moreBtn.setAttribute('aria-haspopup', 'menu');
        moreBtn.setAttribute('aria-expanded', 'false');
        moreBtn.disabled = !ready;
        moreBtn.appendChild(createMoreIconSvg());
        moreBtn.addEventListener('click', function (ev) {
          ev.stopPropagation();
          toggleMoreMenu(moreBtn, sess.termId);
        });
        actions.appendChild(moreBtn);

        card.appendChild(meta);
        card.appendChild(actions);
        frag.appendChild(card);
      });
      sessionListEl.appendChild(frag);
    }

    /* ── 「...」浮层菜单与重命名弹窗 ─────────────────────────── */

    function closeMoreMenu() {
      if (moreMenuEl) moreMenuEl.hidden = true;
      if (menuAnchorBtn) {
        menuAnchorBtn.setAttribute('aria-expanded', 'false');
        menuAnchorBtn = null;
      }
      menuTargetTermId = null;
    }

    function toggleMoreMenu(anchorBtn, termId) {
      if (!moreMenuEl) return;
      if (!moreMenuEl.hidden && menuTargetTermId === termId && menuAnchorBtn === anchorBtn) {
        closeMoreMenu();
        return;
      }
      closeMoreMenu();
      menuTargetTermId = termId;
      menuAnchorBtn = anchorBtn;
      anchorBtn.setAttribute('aria-expanded', 'true');

      moreMenuEl.hidden = false;
      var anchorRect = anchorBtn.getBoundingClientRect();
      var menuW = moreMenuEl.offsetWidth || 128;
      var menuH = moreMenuEl.offsetHeight || 72;
      var left = Math.max(8, Math.min(window.innerWidth - menuW - 8, anchorRect.right - menuW));
      var top = anchorRect.bottom + 6;
      if (top + menuH > window.innerHeight - 8) {
        top = Math.max(8, anchorRect.top - menuH - 6);
      }
      moreMenuEl.style.left = Math.round(left) + 'px';
      moreMenuEl.style.top = Math.round(top) + 'px';
    }

    function openRenameModal(termId) {
      var sess = findSession(termId);
      if (!sess || !renameBackdropEl || !renameInputEl) return;
      closeMoreMenu();
      renameTargetTermId = termId;
      renameInFlight = false;
      if (renameErrorEl) {
        renameErrorEl.textContent = '';
        renameErrorEl.hidden = true;
      }
      if (renameConfirmBtn) renameConfirmBtn.disabled = false;
      renameInputEl.value = sess.name || '';
      renameBackdropEl.hidden = false;
      window.setTimeout(function () {
        renameInputEl.focus();
        renameInputEl.select();
      }, 0);
    }

    function closeRenameModal() {
      renameTargetTermId = null;
      renameInFlight = false;
      if (renameBackdropEl) renameBackdropEl.hidden = true;
      if (renameErrorEl) {
        renameErrorEl.textContent = '';
        renameErrorEl.hidden = true;
      }
      if (viewMode === 'terminal') {
        focusTerminal();
      }
    }

    function terminateSession(termId) {
      closeMoreMenu();
      if (!isSessionReady()) {
        showToast('当前会话未连接，无法终止远端终端');
        return;
      }
      Api.close(termId).catch(function (err) {
        showToast(errMessage(err, '终止会话失败'));
      });
    }

    if (menuRenameBtn) {
      menuRenameBtn.addEventListener('click', function () {
        if (menuTargetTermId != null) {
          openRenameModal(menuTargetTermId);
        }
      });
    }

    if (menuTerminateBtn) {
      menuTerminateBtn.addEventListener('click', function () {
        if (menuTargetTermId != null) {
          terminateSession(menuTargetTermId);
        }
      });
    }

    if (renameCancelBtn) {
      renameCancelBtn.addEventListener('click', closeRenameModal);
    }

    if (renameBackdropEl) {
      renameBackdropEl.addEventListener('mousedown', function (ev) {
        if (ev.target === renameBackdropEl) {
          closeRenameModal();
        }
      });
      renameBackdropEl.addEventListener('keydown', function (ev) {
        ev.stopPropagation();
        if (ev.key === 'Escape') {
          ev.preventDefault();
          closeRenameModal();
        }
      });
    }

    if (renameFormEl) {
      renameFormEl.addEventListener('submit', function (ev) {
        ev.preventDefault();
        if (renameTargetTermId == null || renameInFlight) return;
        var check = validateSessionName(renameInputEl ? renameInputEl.value : '');
        if (!check.ok) {
          if (renameErrorEl) {
            renameErrorEl.textContent = check.error;
            renameErrorEl.hidden = false;
          }
          return;
        }
        var targetId = renameTargetTermId;
        renameInFlight = true;
        if (renameConfirmBtn) renameConfirmBtn.disabled = true;
        if (renameErrorEl) {
          renameErrorEl.textContent = '';
          renameErrorEl.hidden = true;
        }
        Api.rename(targetId, check.name).catch(function (err) {
          renameInFlight = false;
          if (renameConfirmBtn) renameConfirmBtn.disabled = false;
          if (renameErrorEl) {
            renameErrorEl.textContent = errMessage(err, '重命名失败');
            renameErrorEl.hidden = false;
          }
        });
      });
    }

    document.addEventListener('mousedown', function (ev) {
      if (moreMenuEl && !moreMenuEl.hidden) {
        if (
          !moreMenuEl.contains(ev.target) &&
          !(menuAnchorBtn && menuAnchorBtn.contains(ev.target))
        ) {
          closeMoreMenu();
        }
      }
    });

    document.addEventListener('keydown', function (ev) {
      if (ev.key === 'Escape' && moreMenuEl && !moreMenuEl.hidden) {
        closeMoreMenu();
      }
    });

    listViewEl.addEventListener('scroll', closeMoreMenu);
    window.addEventListener('resize', closeMoreMenu);

    /* ── 视图 2（交互终端）UI 同步 ─────────────────────────── */

    function setStatus(kind, text) {
      if (statusTextEl) statusTextEl.textContent = text;
      if (statusDotEl) {
        statusDotEl.classList.toggle('is-online', kind === 'online');
        statusDotEl.classList.toggle('is-error', kind === 'error');
      }
    }

    function showNotice(message, showReconnect) {
      if (noticeTextEl) noticeTextEl.textContent = message;
      if (noticeEl) noticeEl.hidden = false;
      if (noticeReconnectBtn) noticeReconnectBtn.hidden = showReconnect === false;
      if (reconnectBtn) reconnectBtn.hidden = showReconnect === false;
    }

    function hideNotice() {
      if (noticeEl) noticeEl.hidden = true;
    }

    function renderLiveTabs() {
      if (tabsWrapEl) {
        tabsWrapEl.hidden = false;
      }
      if (tabListEl) {
        tabListEl.textContent = '';
        sessions.forEach(function (sess) {
          var isActive = sess.termId === activeTermId;
          var btn = document.createElement('button');
          btn.type = 'button';
          btn.className = 'tm-tab' + (isActive ? ' is-active' : '');
          btn.setAttribute('role', 'tab');
          btn.setAttribute('aria-selected', isActive ? 'true' : 'false');

          var labelSpan = document.createElement('span');
          labelSpan.className = 'tm-tab__title';
          labelSpan.textContent = sess.name || 'session' + sess.termId;
          btn.appendChild(labelSpan);

          btn.addEventListener('click', function () {
            openSessionTerminal(sess.termId);
          });
          tabListEl.appendChild(btn);
        });
      }
      if (newTabBtn) {
        newTabBtn.disabled = !isSessionReady() || attachCreating || sessions.length >= MAX_SESSIONS;
      }
    }

    function syncLiveChrome() {
      if (viewMode !== 'terminal') return;
      var sess = activeTermId != null ? findSession(activeTermId) : null;
      var tab = getActiveTab();
      var devSuffix = sessionState.deviceName ? ' · ' + sessionState.deviceName : '';
      var displayName = (sess && sess.name) || (tab && (tab.name || tab.title)) || '终端远控';

      if (titleEl) {
        titleEl.textContent = displayName + devSuffix;
        document.title = displayName + devSuffix + ' — RabbitControl';
      }

      if (liveMoreBtn) {
        liveMoreBtn.hidden = !sess || !isSessionReady();
      }

      if (!tab) {
        if (cwdEl) cwdEl.hidden = true;
        if (pendingAttachViewTarget) {
          setStatus('connecting', '创建中…' + devSuffix);
        } else {
          setStatus('error', isSessionReady() ? '未选择终端' : '未连接');
        }
        if (reconnectBtn) reconnectBtn.hidden = true;
        renderLiveTabs();
        syncTimerRunning();
        return;
      }

      if (cwdEl) {
        if (tab.cwd) {
          cwdEl.textContent = tab.cwd;
          cwdEl.hidden = false;
        } else {
          cwdEl.hidden = true;
        }
      }

      if (tab.opening) {
        setStatus('connecting', '附着中…' + devSuffix);
        if (reconnectBtn) reconnectBtn.hidden = true;
      } else if (tab.connected) {
        setStatus('online', '已连接' + devSuffix);
        if (reconnectBtn) reconnectBtn.hidden = true;
      } else {
        setStatus('error', '未连接' + devSuffix);
        var noticeBlocksReconnect = Boolean(
          noticeEl && !noticeEl.hidden && noticeReconnectBtn && noticeReconnectBtn.hidden
        );
        if (reconnectBtn) {
          reconnectBtn.hidden = !isSessionReady() || noticeBlocksReconnect;
        }
      }

      renderLiveTabs();
      syncTimerRunning();
    }

    function focusTerminal() {
      if (viewMode !== 'terminal') return;
      if (renameBackdropEl && !renameBackdropEl.hidden) return;
      if (imeEl) {
        imeEl.focus({ preventScroll: true });
      } else {
        screenEl.focus({ preventScroll: true });
      }
    }

    /* ── VT100 / ANSI 缓冲区操作（作用于指定 Tab） ──────────── */

    function resetBsTracking(tab) {
      tab.lastBsCol = -1;
      tab.lastBsSpaceCol = -1;
    }

    function viewportTopOf(tab) {
      return Math.max(0, tab.lines.length - lastRows);
    }

    function scrollBoundsOf(tab) {
      var top = viewportTopOf(tab);
      var rTop = Math.max(0, Math.min(lastRows - 1, tab.scrollTop));
      var rawBot = tab.scrollBottom >= 0 ? tab.scrollBottom : lastRows - 1;
      var rBot = Math.max(rTop, Math.min(lastRows - 1, rawBot));
      return {
        startIdx: top + rTop,
        endIdx: top + rBot
      };
    }

    function trimLinesAboveMax(tab) {
      var dropLimit = tab.inAltScreen ? lastRows : MAX_LINES;
      while (tab.lines.length > dropLimit) {
        var dropped = tab.lines.shift();
        if (tab.termId === activeTermId && dropped && dropped.node) {
          removedNodes.push(dropped.node);
        }
        if (tab.curLineIdx > 0) tab.curLineIdx -= 1;
        if (tab.termId === activeTermId && prevCursorLineIdx >= 0) prevCursorLineIdx -= 1;
      }
    }

    function truncateLinesAfter(tab, keepCount) {
      while (tab.lines.length > keepCount) {
        var popped = tab.lines.pop();
        if (tab.termId === activeTermId && popped && popped.node) {
          removedNodes.push(popped.node);
        }
      }
      if (tab.termId === activeTermId && prevCursorLineIdx >= tab.lines.length) {
        prevCursorLineIdx = -1;
      }
    }

    function ensureLineAt(tab, idx) {
      while (tab.lines.length <= idx) {
        tab.lines.push(makeLine(false));
      }
      trimLinesAboveMax(tab);
      if (idx >= tab.lines.length) idx = tab.lines.length - 1;
      return tab.lines[idx];
    }

    function ensureCurrentLine(tab) {
      if (!tab.lines.length) {
        tab.lines.push(makeLine(false));
        tab.curLineIdx = 0;
        tab.curCol = 0;
      }
      if (tab.curLineIdx >= tab.lines.length) {
        tab.curLineIdx = tab.lines.length - 1;
      }
      if (tab.curLineIdx < 0) {
        tab.curLineIdx = 0;
      }
      return tab.lines[tab.curLineIdx];
    }

    function clearWideAt(line, col) {
      if (col < 0 || col >= line.chars.length) return;
      if (line.chars[col] === '' && col > 0) {
        line.chars[col - 1] = ' ';
        line.chars[col] = ' ';
        line.dirty = true;
      } else if (col + 1 < line.chars.length && line.chars[col + 1] === '') {
        line.chars[col] = ' ';
        line.chars[col + 1] = ' ';
        line.dirty = true;
      }
    }

    function putChar(tab, ch) {
      var width = charDisplayWidth(ch);
      if (width === 0) return;

      var line = ensureCurrentLine(tab);
      while (line.chars.length < tab.curCol) {
        line.chars.push(' ');
        line.styles.push('');
      }
      if (
        ch === ' ' &&
        !tab.curStyleToken &&
        tab.curCol === tab.lastBsCol &&
        tab.curCol === line.chars.length - 1
      ) {
        tab.lastBsSpaceCol = tab.curCol;
        tab.lastBsCol = -1;
      } else {
        resetBsTracking(tab);
      }

      clearWideAt(line, tab.curCol);
      if (width === 2) {
        clearWideAt(line, tab.curCol + 1);
      }

      line.chars[tab.curCol] = ch;
      line.styles[tab.curCol] = tab.curStyleToken;
      if (width === 2) {
        line.chars[tab.curCol + 1] = '';
        line.styles[tab.curCol + 1] = tab.curStyleToken;
      }
      line.dirty = true;
      tab.curCol += width;
    }

    function scrollUpInRegion(tab, count) {
      var bounds = scrollBoundsOf(tab);
      ensureLineAt(tab, bounds.endIdx);
      var start = bounds.startIdx;
      var end = Math.min(tab.lines.length - 1, bounds.endIdx);
      for (var k = 0; k < count; k += 1) {
        if (
          tab.scrollTop === 0 &&
          (tab.scrollBottom < 0 || tab.scrollBottom >= lastRows - 1) &&
          !tab.inAltScreen
        ) {
          tab.lines.push(makeLine(false));
          trimLinesAboveMax(tab);
        } else {
          var removed = tab.lines.splice(start, 1)[0];
          if (tab.termId === activeTermId && removed && removed.node) {
            removedNodes.push(removed.node);
          }
          tab.lines.splice(end, 0, makeLine(false));
          if (tab.termId === activeTermId) fullRebuild = true;
        }
      }
    }

    function scrollDownInRegion(tab, count) {
      var bounds = scrollBoundsOf(tab);
      ensureLineAt(tab, bounds.endIdx);
      var start = bounds.startIdx;
      var end = Math.min(tab.lines.length - 1, bounds.endIdx);
      for (var k = 0; k < count; k += 1) {
        var removed = tab.lines.splice(end, 1)[0];
        if (tab.termId === activeTermId && removed && removed.node) {
          removedNodes.push(removed.node);
        }
        tab.lines.splice(start, 0, makeLine(false));
      }
      if (tab.termId === activeTermId) fullRebuild = true;
    }

    function newLine(tab) {
      resetBsTracking(tab);
      var bounds = scrollBoundsOf(tab);
      if (tab.curLineIdx === bounds.endIdx) {
        scrollUpInRegion(tab, 1);
        tab.curLineIdx = Math.min(tab.lines.length - 1, bounds.endIdx);
      } else if (tab.curLineIdx < tab.lines.length - 1) {
        tab.curLineIdx += 1;
      } else {
        tab.lines.push(makeLine(false));
        trimLinesAboveMax(tab);
        tab.curLineIdx = tab.lines.length - 1;
      }
      tab.curCol = 0;
    }

    function lineFeedOnly(tab) {
      resetBsTracking(tab);
      var bounds = scrollBoundsOf(tab);
      if (tab.curLineIdx === bounds.endIdx) {
        scrollUpInRegion(tab, 1);
        tab.curLineIdx = Math.min(tab.lines.length - 1, bounds.endIdx);
      } else {
        ensureLineAt(tab, tab.curLineIdx + 1);
        tab.curLineIdx = Math.min(tab.lines.length - 1, tab.curLineIdx + 1);
      }
    }

    function appendSystemLine(tab, text) {
      resetBsTracking(tab);
      var sysLine = makeLine(true);
      sysLine.chars = String(text).split('');
      tab.lines.push(sysLine);
      trimLinesAboveMax(tab);
      tab.curLineIdx = tab.lines.length - 1;
      tab.curCol = tab.lines[tab.curLineIdx].chars.length;
      if (tab.termId === activeTermId) scheduleRender();
    }

    function applySgr(tab, paramsStr) {
      var raw = paramsStr === '' ? ['0'] : paramsStr.split(';');
      for (var i = 0; i < raw.length; i += 1) {
        var code = parseInt(raw[i], 10);
        if (isNaN(code)) code = 0;
        if (code === 0) {
          tab.curStyle = emptyStyle();
        } else if (code === 1) {
          tab.curStyle.bold = true;
        } else if (code === 2) {
          tab.curStyle.dim = true;
        } else if (code === 3) {
          tab.curStyle.italic = true;
        } else if (code === 4) {
          tab.curStyle.underline = true;
        } else if (code === 7) {
          tab.curStyle.inverse = true;
        } else if (code === 22) {
          tab.curStyle.bold = false;
          tab.curStyle.dim = false;
        } else if (code === 23) {
          tab.curStyle.italic = false;
        } else if (code === 24) {
          tab.curStyle.underline = false;
        } else if (code === 27) {
          tab.curStyle.inverse = false;
        } else if (code === 39) {
          tab.curStyle.fg = '';
          tab.curStyle.fgCss = '';
        } else if (code === 49) {
          tab.curStyle.bg = '';
          tab.curStyle.bgCss = '';
        } else if (FG_CLASS_BY_CODE[code]) {
          tab.curStyle.fg = FG_CLASS_BY_CODE[code];
          tab.curStyle.fgCss = '';
        } else if (BG_CLASS_BY_CODE[code]) {
          tab.curStyle.bg = BG_CLASS_BY_CODE[code];
          tab.curStyle.bgCss = '';
        } else if (code === 38 || code === 48) {
          var mode = parseInt(raw[i + 1], 10);
          var cssVal = '';
          if (mode === 5 && i + 2 < raw.length) {
            cssVal = ansi256ToCss(raw[i + 2]);
            i += 2;
          } else if (mode === 2 && i + 4 < raw.length) {
            cssVal =
              'rgb(' +
              clampByte(raw[i + 2]) +
              ',' +
              clampByte(raw[i + 3]) +
              ',' +
              clampByte(raw[i + 4]) +
              ')';
            i += 4;
          }
          if (cssVal) {
            if (code === 38) {
              tab.curStyle.fgCss = cssVal;
              tab.curStyle.fg = '';
            } else {
              tab.curStyle.bgCss = cssVal;
              tab.curStyle.bg = '';
            }
          }
        }
      }
      tab.curStyleToken = makeStyleToken(tab.curStyle);
    }

    function enterAltScreen(tab, saveCursorFirst) {
      if (saveCursorFirst) {
        tab.savedCursor = { lineIdx: tab.curLineIdx, col: tab.curCol };
      }
      if (!tab.inAltScreen) {
        tab.savedMainState = {
          lines: tab.lines,
          curLineIdx: tab.curLineIdx,
          curCol: tab.curCol,
          scrollTop: tab.scrollTop,
          scrollBottom: tab.scrollBottom
        };
        tab.inAltScreen = true;
      }
      tab.lines = [];
      for (var i = 0; i < lastRows; i += 1) {
        tab.lines.push(makeLine(false));
      }
      tab.curLineIdx = 0;
      tab.curCol = 0;
      tab.scrollTop = 0;
      tab.scrollBottom = -1;
      if (tab.termId === activeTermId) fullRebuild = true;
    }

    function leaveAltScreen(tab, restoreCursorAfter) {
      if (tab.inAltScreen && tab.savedMainState) {
        tab.lines = tab.savedMainState.lines;
        tab.curLineIdx = tab.savedMainState.curLineIdx;
        tab.curCol = tab.savedMainState.curCol;
        tab.scrollTop = tab.savedMainState.scrollTop;
        tab.scrollBottom = tab.savedMainState.scrollBottom;
        tab.savedMainState = null;
        tab.inAltScreen = false;
        tab.lines.forEach(function (ln) {
          ln.node = null;
          ln.dirty = true;
        });
        if (tab.termId === activeTermId) fullRebuild = true;
      }
      if (restoreCursorAfter && tab.savedCursor) {
        tab.curLineIdx = Math.max(0, Math.min(tab.lines.length - 1, tab.savedCursor.lineIdx));
        tab.curCol = Math.max(0, tab.savedCursor.col);
      }
    }

    function handleDecPrivateMode(tab, paramsStr, enable) {
      var items = paramsStr.split(';');
      items.forEach(function (item) {
        var mode = parseInt(item, 10);
        if (mode === 1049) {
          if (enable) enterAltScreen(tab, true);
          else leaveAltScreen(tab, true);
        } else if (mode === 47 || mode === 1047) {
          if (enable) enterAltScreen(tab, false);
          else leaveAltScreen(tab, false);
        } else if (mode === 25) {
          tab.cursorVisible = enable;
        } else if (mode === 2004) {
          tab.bracketedPaste = enable;
        }
      });
    }

    function handleCsi(tab, paramsRaw, finalChar) {
      if (paramsRaw.charAt(0) === '?') {
        if (finalChar === 'h' || finalChar === 'l') {
          handleDecPrivateMode(tab, paramsRaw.slice(1), finalChar === 'h');
        }
        return;
      }
      if (paramsRaw.charAt(0) === '>') {
        return;
      }
      if (finalChar === 'm') {
        applySgr(tab, paramsRaw);
        return;
      }
      resetBsTracking(tab);
      var n = Math.max(1, parseInt(paramsRaw || '1', 10) || 1);

      if (finalChar === 'A') {
        var boundsUp = scrollBoundsOf(tab);
        var minLine = Math.max(viewportTopOf(tab), boundsUp.startIdx);
        tab.curLineIdx = Math.max(minLine, tab.curLineIdx - n);
        return;
      }
      if (finalChar === 'B') {
        var maxLineB = viewportTopOf(tab) + Math.max(0, lastRows - 1);
        var nextLineB = Math.min(maxLineB, tab.curLineIdx + n);
        ensureLineAt(tab, nextLineB);
        tab.curLineIdx = Math.min(nextLineB, tab.lines.length - 1);
        return;
      }
      if (finalChar === 'C') {
        tab.curCol = Math.min(lastCols - 1, tab.curCol + n);
        return;
      }
      if (finalChar === 'D') {
        tab.curCol = Math.max(0, tab.curCol - n);
        return;
      }
      if (finalChar === 'E') {
        var nextE = Math.min(viewportTopOf(tab) + Math.max(0, lastRows - 1), tab.curLineIdx + n);
        ensureLineAt(tab, nextE);
        tab.curLineIdx = Math.min(nextE, tab.lines.length - 1);
        tab.curCol = 0;
        return;
      }
      if (finalChar === 'F') {
        tab.curLineIdx = Math.max(viewportTopOf(tab), tab.curLineIdx - n);
        tab.curCol = 0;
        return;
      }
      if (finalChar === 'G') {
        var colAbs = parseInt(paramsRaw || '1', 10);
        tab.curCol = Math.max(0, Math.min(lastCols - 1, (isNaN(colAbs) ? 1 : colAbs) - 1));
        return;
      }
      if (finalChar === 'H' || finalChar === 'f') {
        var parts = paramsRaw.split(';');
        var row = parseInt(parts[0] || '1', 10);
        var col = parseInt(parts[1] || '1', 10);
        if (isNaN(row) || row < 1) row = 1;
        if (isNaN(col) || col < 1) col = 1;
        var vpTop = viewportTopOf(tab);
        var targetLine = Math.min(vpTop + Math.max(0, lastRows - 1), vpTop + (row - 1));
        ensureLineAt(tab, targetLine);
        tab.curLineIdx = Math.min(targetLine, tab.lines.length - 1);
        tab.curCol = Math.max(0, Math.min(lastCols - 1, col - 1));
        return;
      }
      if (finalChar === 'J') {
        var jMode = parseInt(paramsRaw || '0', 10);
        if (jMode === 2 || jMode === 3) {
          tab.lines = [makeLine(false)];
          if (tab.termId === activeTermId) {
            removedNodes = [];
            fullRebuild = true;
            prevCursorLineIdx = -1;
          }
          tab.curLineIdx = 0;
          tab.curCol = 0;
        } else if (jMode === 1) {
          var vpStart = viewportTopOf(tab);
          for (var i1 = vpStart; i1 < tab.curLineIdx && i1 < tab.lines.length; i1 += 1) {
            tab.lines[i1].chars = [];
            tab.lines[i1].styles = [];
            tab.lines[i1].dirty = true;
          }
          var ln1 = ensureCurrentLine(tab);
          for (var c1 = 0; c1 <= tab.curCol && c1 < ln1.chars.length; c1 += 1) {
            ln1.chars[c1] = ' ';
            ln1.styles[c1] = '';
          }
          ln1.dirty = true;
        } else {
          var ln0 = ensureCurrentLine(tab);
          ln0.chars.length = Math.min(ln0.chars.length, tab.curCol);
          ln0.styles.length = ln0.chars.length;
          ln0.dirty = true;
          truncateLinesAfter(tab, tab.curLineIdx + 1);
        }
        return;
      }
      if (finalChar === 'K') {
        var kMode = parseInt(paramsRaw || '0', 10);
        var lineK = ensureCurrentLine(tab);
        if (kMode === 2) {
          lineK.chars = [];
          lineK.styles = [];
          lineK.dirty = true;
        } else if (kMode === 1) {
          for (var ck = 0; ck < tab.curCol && ck < lineK.chars.length; ck += 1) {
            lineK.chars[ck] = ' ';
            lineK.styles[ck] = '';
          }
          lineK.dirty = true;
        } else {
          if (tab.curCol > 0 && lineK.chars[tab.curCol] === '') {
            clearWideAt(lineK, tab.curCol);
          }
          lineK.chars.length = Math.min(lineK.chars.length, tab.curCol);
          lineK.styles.length = lineK.chars.length;
          lineK.dirty = true;
        }
        return;
      }
      if (finalChar === 'P') {
        var lineP = ensureCurrentLine(tab);
        clearWideAt(lineP, tab.curCol);
        if (tab.curCol < lineP.chars.length) {
          lineP.chars.splice(tab.curCol, n);
          lineP.styles.splice(tab.curCol, n);
          lineP.dirty = true;
        }
        return;
      }
      if (finalChar === '@') {
        var lineAt = ensureCurrentLine(tab);
        clearWideAt(lineAt, tab.curCol);
        while (lineAt.chars.length < tab.curCol) {
          lineAt.chars.push(' ');
          lineAt.styles.push('');
        }
        for (var a = 0; a < n; a += 1) {
          lineAt.chars.splice(tab.curCol, 0, ' ');
          lineAt.styles.splice(tab.curCol, 0, '');
        }
        if (lineAt.chars.length > lastCols) {
          lineAt.chars.length = lastCols;
          lineAt.styles.length = lastCols;
        }
        lineAt.dirty = true;
        return;
      }
      if (finalChar === 'X') {
        var lineX = ensureCurrentLine(tab);
        while (lineX.chars.length < tab.curCol + n) {
          lineX.chars.push(' ');
          lineX.styles.push('');
        }
        for (var cx = 0; cx < n; cx += 1) {
          clearWideAt(lineX, tab.curCol + cx);
          lineX.chars[tab.curCol + cx] = ' ';
          lineX.styles[tab.curCol + cx] = '';
        }
        lineX.dirty = true;
        return;
      }
      if (finalChar === 'L') {
        var boundsL = scrollBoundsOf(tab);
        if (tab.curLineIdx >= boundsL.startIdx && tab.curLineIdx <= boundsL.endIdx) {
          ensureLineAt(tab, boundsL.endIdx);
          for (var il = 0; il < n; il += 1) {
            var rmL = tab.lines.splice(Math.min(tab.lines.length - 1, boundsL.endIdx), 1)[0];
            if (tab.termId === activeTermId && rmL && rmL.node) removedNodes.push(rmL.node);
            tab.lines.splice(tab.curLineIdx, 0, makeLine(false));
          }
          if (tab.termId === activeTermId) fullRebuild = true;
        }
        return;
      }
      if (finalChar === 'M') {
        var boundsM = scrollBoundsOf(tab);
        if (tab.curLineIdx >= boundsM.startIdx && tab.curLineIdx <= boundsM.endIdx) {
          ensureLineAt(tab, boundsM.endIdx);
          for (var dl = 0; dl < n; dl += 1) {
            var rmM = tab.lines.splice(tab.curLineIdx, 1)[0];
            if (tab.termId === activeTermId && rmM && rmM.node) removedNodes.push(rmM.node);
            tab.lines.splice(Math.min(tab.lines.length, boundsM.endIdx), 0, makeLine(false));
          }
          if (tab.termId === activeTermId) fullRebuild = true;
        }
        return;
      }
      if (finalChar === 'r') {
        var rParts = paramsRaw.split(';');
        var topR = parseInt(rParts[0] || '1', 10);
        var botR = parseInt(rParts[1] || String(lastRows), 10);
        if (isNaN(topR) || topR < 1) topR = 1;
        if (isNaN(botR) || botR > lastRows) botR = lastRows;
        if (topR < botR) {
          tab.scrollTop = topR - 1;
          tab.scrollBottom = botR - 1;
        } else {
          tab.scrollTop = 0;
          tab.scrollBottom = -1;
        }
        tab.curLineIdx = viewportTopOf(tab);
        tab.curCol = 0;
        return;
      }
      if (finalChar === 's') {
        tab.savedCursor = { lineIdx: tab.curLineIdx, col: tab.curCol };
        return;
      }
      if (finalChar === 'u' && tab.savedCursor) {
        tab.curLineIdx = Math.max(0, Math.min(tab.lines.length - 1, tab.savedCursor.lineIdx));
        tab.curCol = Math.max(0, tab.savedCursor.col);
      }
    }

    function handleOsc(tab, content) {
      var sep = content.indexOf(';');
      if (sep < 0) return;
      var code = content.slice(0, sep);
      var payload = content.slice(sep + 1);
      if ((code === '0' || code === '2') && payload) {
        tab.title = payload;
        if (tab.termId === activeTermId) {
          syncLiveChrome();
        }
      }
    }

    function feedText(tab, text) {
      if (!text) return;
      var stream = tab.pendingSurrogate ? tab.pendingSurrogate + text : text;
      tab.pendingSurrogate = '';
      if (stream.length > 0) {
        var tailCode = stream.charCodeAt(stream.length - 1);
        if (tailCode >= 0xd800 && tailCode <= 0xdbff) {
          tab.pendingSurrogate = stream.charAt(stream.length - 1);
          stream = stream.slice(0, -1);
        }
      }

      for (var i = 0; i < stream.length; i += 1) {
        var code = stream.charCodeAt(i);
        var ch = stream.charAt(i);
        if (code >= 0xd800 && code <= 0xdbff && i + 1 < stream.length) {
          var low = stream.charCodeAt(i + 1);
          if (low >= 0xdc00 && low <= 0xdfff) {
            ch = stream.slice(i, i + 2);
            i += 1;
          }
        }

        if (tab.parseState === 'normal') {
          if (code === 0x1b) {
            tab.parseState = 'esc';
          } else if (ch === '\r') {
            resetBsTracking(tab);
            tab.curCol = 0;
          } else if (ch === '\n') {
            newLine(tab);
          } else if (code === 0x0b || code === 0x0c) {
            lineFeedOnly(tab);
          } else if (code === 0x08) {
            var prevCol = tab.curCol;
            var bsLine = ensureCurrentLine(tab);
            if (prevCol >= 2 && bsLine.chars[prevCol - 1] === '') {
              tab.curCol = prevCol - 2;
            } else {
              tab.curCol = Math.max(0, prevCol - 1);
            }
            if (
              tab.curCol === tab.lastBsSpaceCol &&
              tab.curCol === bsLine.chars.length - 1 &&
              bsLine.chars[tab.curCol] === ' ' &&
              !bsLine.styles[tab.curCol]
            ) {
              bsLine.chars.pop();
              bsLine.styles.pop();
              bsLine.dirty = true;
              resetBsTracking(tab);
            } else {
              tab.lastBsCol = tab.curCol;
              tab.lastBsSpaceCol = -1;
            }
          } else if (ch === '\t') {
            resetBsTracking(tab);
            var nextStop = tab.curCol + (8 - (tab.curCol % 8));
            while (tab.curCol < nextStop) {
              putChar(tab, ' ');
            }
          } else if (code === 0x07) {
            /* BEL 忽略 */
          } else if (code >= 0x20) {
            putChar(tab, ch);
          }
        } else if (tab.parseState === 'esc') {
          if (ch === '[') {
            tab.parseState = 'csi';
            tab.csiBuf = '';
          } else if (ch === ']') {
            tab.parseState = 'osc';
            tab.oscBuf = '';
          } else if (ch === '(' || ch === ')' || ch === '*' || ch === '+') {
            tab.parseState = 'charset';
          } else if (ch === '7') {
            tab.savedCursor = { lineIdx: tab.curLineIdx, col: tab.curCol };
            tab.parseState = 'normal';
          } else if (ch === '8') {
            if (tab.savedCursor) {
              tab.curLineIdx = Math.max(0, Math.min(tab.lines.length - 1, tab.savedCursor.lineIdx));
              tab.curCol = Math.max(0, tab.savedCursor.col);
            }
            tab.parseState = 'normal';
          } else if (ch === 'M') {
            var boundsRv = scrollBoundsOf(tab);
            if (tab.curLineIdx === boundsRv.startIdx) {
              scrollDownInRegion(tab, 1);
            } else if (tab.curLineIdx > 0) {
              tab.curLineIdx -= 1;
            }
            tab.parseState = 'normal';
          } else if (ch === 'c') {
            resetTabBuffer(tab);
            tab.parseState = 'normal';
          } else {
            tab.parseState = 'normal';
          }
        } else if (tab.parseState === 'charset') {
          tab.parseState = 'normal';
        } else if (tab.parseState === 'csi') {
          if (code >= 0x40 && code <= 0x7e) {
            handleCsi(tab, tab.csiBuf, ch);
            tab.parseState = 'normal';
            tab.csiBuf = '';
          } else if (tab.csiBuf.length < 64) {
            tab.csiBuf += ch;
          }
        } else if (tab.parseState === 'osc') {
          if (code === 0x07) {
            handleOsc(tab, tab.oscBuf);
            tab.parseState = 'normal';
            tab.oscBuf = '';
          } else if (code === 0x1b) {
            tab.parseState = 'oscEsc';
          } else if (tab.oscBuf.length < 256) {
            tab.oscBuf += ch;
          }
        } else if (tab.parseState === 'oscEsc') {
          if (ch === '\\') {
            handleOsc(tab, tab.oscBuf);
          }
          tab.parseState = 'normal';
          tab.oscBuf = '';
        }
      }
      if (tab.termId === activeTermId) {
        scheduleRender();
      }
    }

    /* ── 安全 DOM 渲染（零 innerHTML） ──────────────────────── */

    function appendStyledRange(parent, chars, styles, start, end) {
      if (start >= end) return;
      var segStart = start;
      var segToken = styles[start] || '';
      for (var i = start + 1; i <= end; i += 1) {
        var nextToken = i < end ? styles[i] || '' : null;
        if (nextToken !== segToken) {
          var chunk = chars.slice(segStart, i).join('');
          if (chunk) {
            if (segToken) {
              var parsed = parseStyleToken(segToken);
              var span = document.createElement('span');
              if (parsed.cls) span.className = parsed.cls;
              if (parsed.fgCss) span.style.color = parsed.fgCss;
              if (parsed.bgCss) span.style.backgroundColor = parsed.bgCss;
              span.textContent = chunk;
              parent.appendChild(span);
            } else {
              parent.appendChild(document.createTextNode(chunk));
            }
          }
          segStart = i;
          segToken = nextToken;
        }
      }
    }

    function populateLineNode(div, line, hasCursor, cursorCol) {
      div.textContent = '';
      div.className = line.system ? 'tm-line tm-line--system' : 'tm-line';

      if (line.system) {
        div.textContent = line.chars.join('');
        return;
      }

      var chars = line.chars;
      var styles = line.styles;

      if (!hasCursor) {
        appendStyledRange(div, chars, styles, 0, chars.length);
        return;
      }

      var col = Math.max(0, cursorCol);
      if (col > 0 && col < chars.length && chars[col] === '') {
        col -= 1;
      }
      var cursor = document.createElement('span');
      cursor.className = 'tm-live-cursor';
      cursor.setAttribute('aria-hidden', 'true');

      if (col < chars.length) {
        appendStyledRange(div, chars, styles, 0, col);
        div.appendChild(cursor);
        appendStyledRange(div, chars, styles, col, chars.length);
      } else {
        appendStyledRange(div, chars, styles, 0, chars.length);
        div.appendChild(cursor);
      }
    }

    function renderLineNode(line, hasCursor, cursorCol) {
      var div = document.createElement('div');
      populateLineNode(div, line, hasCursor, cursorCol);
      return div;
    }

    function flushRender() {
      rafId = 0;
      if (!renderPending) return;
      renderPending = false;

      if (viewMode !== 'terminal') return;
      var tab = getActiveTab();
      if (!tab) {
        screenEl.textContent = '';
        return;
      }

      var wasNearBottom =
        screenEl.scrollHeight - screenEl.scrollTop - screenEl.clientHeight <= 48;

      var cursorVisible = Boolean(tab.connected && tab.cursorVisible);
      var cursorLineIdx = cursorVisible ? tab.curLineIdx : -1;
      var lines = tab.lines;

      if (fullRebuild) {
        fullRebuild = false;
        removedNodes.length = 0;
        var frag = document.createDocumentFragment();
        for (var i = 0; i < lines.length; i += 1) {
          var ln = lines[i];
          ln.node = renderLineNode(ln, i === cursorLineIdx, tab.curCol);
          ln.dirty = false;
          frag.appendChild(ln.node);
        }
        screenEl.textContent = '';
        screenEl.appendChild(frag);
      } else {
        for (var r = 0; r < removedNodes.length; r += 1) {
          var rn = removedNodes[r];
          if (rn && rn.parentNode === screenEl) {
            screenEl.removeChild(rn);
          }
        }
        removedNodes.length = 0;

        if (
          prevCursorVisible !== cursorVisible ||
          prevCursorLineIdx !== cursorLineIdx ||
          prevCursorCol !== tab.curCol
        ) {
          if (prevCursorLineIdx >= 0 && prevCursorLineIdx < lines.length) {
            lines[prevCursorLineIdx].dirty = true;
          }
          if (cursorLineIdx >= 0 && cursorLineIdx < lines.length) {
            lines[cursorLineIdx].dirty = true;
          }
        }

        for (var j = 0; j < lines.length; j += 1) {
          var line = lines[j];
          var isCur = j === cursorLineIdx;
          if (!line.node) {
            line.node = renderLineNode(line, isCur, tab.curCol);
            line.dirty = false;
            if (j === 0) {
              screenEl.insertBefore(line.node, screenEl.firstChild);
            } else {
              var prevNode = lines[j - 1].node;
              screenEl.insertBefore(line.node, prevNode ? prevNode.nextSibling : null);
            }
          } else if (line.dirty) {
            populateLineNode(line.node, line, isCur, tab.curCol);
            line.dirty = false;
          }
        }
      }

      prevCursorVisible = cursorVisible;
      prevCursorLineIdx = cursorLineIdx;
      prevCursorCol = tab.curCol;

      if (wasNearBottom) {
        screenEl.scrollTop = screenEl.scrollHeight;
      }
    }

    function scheduleRender() {
      renderPending = true;
      if (!rafId) {
        rafId = window.requestAnimationFrame(flushRender);
      }
    }

    /* ── 网格列/行尺寸测量 ──────────────────────────────────── */

    function measureCell() {
      var probe = document.createElement('span');
      probe.style.position = 'absolute';
      probe.style.visibility = 'hidden';
      probe.style.whiteSpace = 'pre';
      probe.textContent = 'MMMMMMMMMM';
      screenEl.appendChild(probe);
      var rect = probe.getBoundingClientRect();
      screenEl.removeChild(probe);
      var charW = rect.width > 0 ? rect.width / 10 : 7.8;
      var lineH = rect.height > 0 ? rect.height : 19;
      return { charW: charW, lineH: lineH };
    }

    function computeGrid() {
      var cell = measureCell();
      var rect = screenEl.getBoundingClientRect();
      var style = window.getComputedStyle(screenEl);
      var padX = (parseFloat(style.paddingLeft) || 0) + (parseFloat(style.paddingRight) || 0);
      var padY = (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
      var w = Math.max(0, rect.width - padX);
      var h = Math.max(0, rect.height - padY);
      return {
        cols: Math.min(512, Math.max(40, Math.floor(w / cell.charW))),
        rows: Math.min(512, Math.max(12, Math.floor(h / cell.lineH)))
      };
    }

    /* ── 键盘与剪贴板输入 ───────────────────────────────────── */

    function sendInputToTab(tab, data) {
      if (!tab || !tab.connected || tab.termId == null || !data) return;
      Api.input(tab.termId, data).catch(function () {});
    }

    function sendPasteText(tab, text) {
      if (!text) return;
      var payload = tab && tab.bracketedPaste ? '\x1b[200~' + text + '\x1b[201~' : text;
      sendInputToTab(tab, payload);
    }

    function copyTerminalSelection() {
      var sel = window.getSelection ? window.getSelection().toString() : '';
      if (!sel) return false;
      if (Bridge && typeof Bridge.copyText === 'function') {
        Bridge.copyText(sel).catch(function () {});
        return true;
      }
      if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
        navigator.clipboard.writeText(sel).catch(function () {});
        return true;
      }
      return false;
    }

    function mapKeyEvent(e) {
      if (e.metaKey) return null;

      if (FKEY_SEQ[e.key]) {
        return FKEY_SEQ[e.key];
      }

      if (e.shiftKey && e.key === 'Tab') {
        return '\x1b[Z';
      }

      if (e.ctrlKey && !e.altKey && e.key && e.key.length === 1) {
        var upper = e.key.toUpperCase();
        var cc = upper.charCodeAt(0);
        if (cc >= 65 && cc <= 90) {
          return String.fromCharCode(cc - 64);
        }
        if (e.key === '[') return '\x1b';
        if (e.key === '\\') return '\x1c';
        if (e.key === ']') return '\x1d';
        return null;
      }

      if (e.altKey && !e.ctrlKey && e.key && e.key.length === 1) {
        return '\x1b' + e.key;
      }

      if (e.key === 'Enter') return '\r';
      if (e.key === 'Backspace') return '\x7f';
      if (e.key === 'Tab') return '\t';
      if (e.key === 'Escape') return '\x1b';
      if (e.key === 'ArrowUp') return '\x1b[A';
      if (e.key === 'ArrowDown') return '\x1b[B';
      if (e.key === 'ArrowRight') return '\x1b[C';
      if (e.key === 'ArrowLeft') return '\x1b[D';
      if (e.key === 'Home') return '\x1b[H';
      if (e.key === 'End') return '\x1b[F';
      if (e.key === 'Delete') return '\x1b[3~';
      if (e.key === 'PageUp') return '\x1b[5~';
      if (e.key === 'PageDown') return '\x1b[6~';

      if (e.key && e.key.length === 1 && !e.ctrlKey && !e.altKey) {
        return e.key;
      }
      return null;
    }

    function onKeyDown(e) {
      if (viewMode !== 'terminal') return;
      if (renameBackdropEl && !renameBackdropEl.hidden) return;
      var tab = getActiveTab();
      if (e.metaKey && (e.key || '').toLowerCase() === 'a') {
        e.preventDefault();
        if (window.getSelection && document.createRange) {
          var range = document.createRange();
          range.selectNodeContents(screenEl);
          var sel = window.getSelection();
          if (sel) {
            sel.removeAllRanges();
            sel.addRange(range);
          }
        }
        return;
      }

      if (e.ctrlKey && e.shiftKey && !e.metaKey && !e.altKey) {
        var upperCtrl = (e.key || '').toUpperCase();
        if (upperCtrl === 'C') {
          if (copyTerminalSelection()) {
            e.preventDefault();
            return;
          }
        } else if (upperCtrl === 'V') {
          e.preventDefault();
          if (
            navigator.clipboard &&
            typeof navigator.clipboard.readText === 'function' &&
            tab &&
            tab.connected
          ) {
            navigator.clipboard
              .readText()
              .then(function (txt) {
                sendPasteText(tab, txt);
              })
              .catch(function () {});
          }
          return;
        }
      }

      if (!tab || !tab.connected || tab.termId == null) return;
      if (e.isComposing || isComposing || e.keyCode === 229) return;

      var seq = mapKeyEvent(e);
      if (seq != null) {
        e.preventDefault();
        sendInputToTab(tab, seq);
        if (imeEl) imeEl.value = '';
      }
    }

    function onPaste(e) {
      if (viewMode !== 'terminal') return;
      var tab = getActiveTab();
      if (!tab || !tab.connected || tab.termId == null) return;
      var clip = e.clipboardData || window.clipboardData;
      var text = clip ? clip.getData('text') : '';
      if (text) {
        e.preventDefault();
        sendPasteText(tab, text);
        if (imeEl) imeEl.value = '';
      }
    }

    screenEl.addEventListener('keydown', onKeyDown);
    screenEl.addEventListener('paste', onPaste);
    screenEl.addEventListener('mouseup', function () {
      var tab = getActiveTab();
      var sel = window.getSelection ? window.getSelection().toString() : '';
      if (!sel && tab && tab.connected) {
        focusTerminal();
      }
    });

    if (imeEl) {
      imeEl.addEventListener('keydown', onKeyDown);
      imeEl.addEventListener('paste', onPaste);
      imeEl.addEventListener('compositionstart', function () {
        isComposing = true;
      });
      imeEl.addEventListener('compositionend', function (e) {
        isComposing = false;
        var data = e.data || imeEl.value || '';
        imeEl.value = '';
        var tab = getActiveTab();
        if (data && tab) {
          sendInputToTab(tab, data);
        }
      });
      imeEl.addEventListener('input', function () {
        if (isComposing) return;
        var val = imeEl.value;
        if (val) {
          imeEl.value = '';
          var tab = getActiveTab();
          if (tab) sendInputToTab(tab, val);
        }
      });
    }

    /* ── 会话创建 / 打开终端 / 返回列表 / 切换动作 ───────────── */

    function finishBgCreate() {
      bgCreating = false;
      window.clearTimeout(bgCreateTimer);
      bgCreateTimer = 0;
    }

    function finishAttachCreate() {
      attachCreating = false;
      window.clearTimeout(attachCreateTimer);
      attachCreateTimer = 0;
    }

    function createBackgroundSession() {
      if (!isSessionReady()) {
        showToast(sessionState.watchOnly ? '只读模式下无法创建会话' : '请先建立远控连接');
        return;
      }
      if (sessions.length >= MAX_SESSIONS) {
        showToast('最多支持同时运行 ' + MAX_SESSIONS + ' 个后台终端会话');
        return;
      }
      if (bgCreating) return;

      closeMoreMenu();
      bgCreating = true;
      pendingOpenKinds.push('bg');
      window.clearTimeout(bgCreateTimer);
      bgCreateTimer = window.setTimeout(function () {
        removePendingOpenKind('bg');
        finishBgCreate();
        renderManagerView();
      }, 6000);
      renderManagerView();

      Api.open(80, 24, null, false).catch(function (err) {
        removePendingOpenKind('bg');
        finishBgCreate();
        showToast(errMessage(err, '创建终端会话失败'));
        renderManagerView();
      });
    }

    function openAndAttachNewSession() {
      if (!isSessionReady() || attachCreating || sessions.length >= MAX_SESSIONS) return;
      closeMoreMenu();

      if (activeTermId != null) {
        var prevTab = tabByTermId[String(activeTermId)];
        if (prevTab) {
          prevTab.connected = false;
          prevTab.opening = false;
        }
        var detachOldId = activeTermId;
        activeTermId = null;
        Api.detach(detachOldId).catch(function () {});
      }

      attachCreating = true;
      pendingAttachViewTarget = true;
      pendingOpenKinds.push('attach');
      window.clearTimeout(attachCreateTimer);
      attachCreateTimer = window.setTimeout(function () {
        removePendingOpenKind('attach');
        finishAttachCreate();
        pendingAttachViewTarget = false;
        syncLiveChrome();
      }, 6000);

      cancelPendingRender();
      fullRebuild = true;
      prevCursorLineIdx = -1;
      prevCursorCol = -1;
      prevCursorVisible = false;
      screenEl.textContent = '';
      hideNotice();

      setView('terminal');
      var grid = computeGrid();
      lastCols = grid.cols;
      lastRows = grid.rows;

      Api.open(grid.cols, grid.rows, null, true).catch(function (err) {
        removePendingOpenKind('attach');
        finishAttachCreate();
        pendingAttachViewTarget = false;
        showToast(errMessage(err, '新建终端失败'));
        if (viewMode === 'terminal') syncLiveChrome();
        else renderManagerView();
      });
    }

    function openSessionTerminal(termId) {
      closeMoreMenu();
      if (!isSessionReady()) {
        showToast(sessionState.watchOnly ? '只读模式下无法打开远程终端' : '请先建立远控连接');
        return;
      }

      pendingAttachViewTarget = false;

      var prevId = activeTermId;
      if (prevId != null && prevId !== termId) {
        var prevTab = tabByTermId[String(prevId)];
        if (prevTab) {
          prevTab.connected = false;
          prevTab.opening = false;
        }
        Api.detach(prevId).catch(function () {});
      }

      cancelPendingRender();
      activeTermId = Number(termId);
      var tab = ensureTab(activeTermId);
      tab.opening = true;
      tab.connected = false;
      tab.disconnectNoticed = false;
      resetTabBuffer(tab, true);
      screenEl.textContent = '';
      hideNotice();

      setView('terminal');
      var grid = computeGrid();
      lastCols = grid.cols;
      lastRows = grid.rows;

      var mySeq = ++attachSeq;
      Api.attach(activeTermId, grid.cols, grid.rows).catch(function (err) {
        if (mySeq !== attachSeq || activeTermId !== termId) return;
        tab.opening = false;
        tab.connected = false;
        showNotice(errMessage(err, '附着远程终端失败'), isSessionReady());
        syncLiveChrome();
      });
    }

    function returnToListView() {
      closeMoreMenu();
      pendingAttachViewTarget = false;
      cancelPendingRender();
      var detachId = activeTermId;
      if (detachId != null) {
        var tab = tabByTermId[String(detachId)];
        if (tab) {
          tab.opening = false;
          tab.connected = false;
        }
        activeTermId = null;
        if (isSessionReady()) {
          Api.detach(detachId).catch(function () {});
        }
      }
      setView('list');
      if (isSessionReady()) {
        Api.list().catch(function () {});
      }
    }

    function refreshSessionList() {
      if (!isSessionReady()) {
        renderManagerView();
        return;
      }
      Api.list().catch(function () {});
    }

    if (createBtn) {
      createBtn.addEventListener('click', createBackgroundSession);
    }
    if (backBtn) {
      backBtn.addEventListener('click', returnToListView);
    }
    if (noticeBackBtn) {
      noticeBackBtn.addEventListener('click', returnToListView);
    }
    if (newTabBtn) {
      newTabBtn.addEventListener('click', openAndAttachNewSession);
    }
    if (liveMoreBtn) {
      liveMoreBtn.addEventListener('click', function (ev) {
        ev.stopPropagation();
        if (activeTermId != null) {
          toggleMoreMenu(liveMoreBtn, activeTermId);
        }
      });
    }

    function reattachCurrentSession() {
      if (activeTermId != null) {
        openSessionTerminal(activeTermId);
      } else {
        returnToListView();
      }
    }

    if (reconnectBtn) {
      reconnectBtn.addEventListener('click', reattachCurrentSession);
    }
    if (noticeReconnectBtn) {
      noticeReconnectBtn.addEventListener('click', reattachCurrentSession);
    }

    /* ── 远端事件与会话状态处理 ─────────────────────────────── */

    function handleTerminalEvent(ev) {
      if (!ev || !ev.type) return;

      if (ev.type === 'sessionList') {
        var incoming = Array.isArray(ev.sessions) ? ev.sessions : [];
        sessions = [];
        var liveIds = Object.create(null);
        incoming.forEach(function (item) {
          var s = upsertSession(item);
          if (s) {
            liveIds[String(s.termId)] = true;
            var tab = tabByTermId[String(s.termId)];
            if (tab) tab.name = s.name;
          }
        });
        Object.keys(tabByTermId).forEach(function (k) {
          if (!liveIds[k]) {
            delete tabByTermId[k];
          }
        });
        if (activeTermId != null && !liveIds[String(activeTermId)]) {
          activeTermId = null;
          if (viewMode === 'terminal') {
            setView('list');
            showToast('该终端会话已结束');
          }
        } else if (viewMode === 'terminal') {
          syncLiveChrome();
        } else {
          renderManagerView();
        }
        return;
      }

      if (ev.type === 'opened') {
        var openKind = pendingOpenKinds.length > 0
          ? pendingOpenKinds.shift()
          : (ev.attached === true ? 'attach' : 'bg');
        var isAttachFlow = openKind === 'attach';
        if (isAttachFlow) {
          finishAttachCreate();
        } else {
          finishBgCreate();
        }

        if (!ev.ok) {
          if (isAttachFlow) pendingAttachViewTarget = false;
          showToast(ev.error || '创建终端会话失败');
          if (viewMode === 'list') renderManagerView();
          else syncLiveChrome();
          return;
        }

        var wantsSwitchToOpened = isAttachFlow && pendingAttachViewTarget && viewMode === 'terminal' && activeTermId == null;
        if (isAttachFlow) pendingAttachViewTarget = false;
        var openedRec = upsertSession({
          termId: ev.termId,
          name: ev.name || 'session' + ev.termId,
          title: ev.title || '',
          cwd: ev.cwd || '',
          createdAtUnixSec: ev.createdAtUnixSec || Math.floor(Date.now() / 1000),
          cols: 80,
          rows: 24,
          attached: wantsSwitchToOpened
        });
        var openedTab = ensureTab(openedRec.termId);
        openedTab.name = openedRec.name;
        openedTab.title = openedRec.title;
        openedTab.cwd = openedRec.cwd;
        openedTab.disconnectNoticed = false;

        if (wantsSwitchToOpened) {
          activeTermId = openedRec.termId;
          openedTab.opening = false;
          openedTab.connected = true;
          resetTabBuffer(openedTab);
          hideNotice();
          setView('terminal');
          var latestGrid = computeGrid();
          lastCols = latestGrid.cols;
          lastRows = latestGrid.rows;
          Api.resize(openedRec.termId, lastCols, lastRows).catch(function () {});
          scheduleRender();
          focusTerminal();
        } else {
          if (ev.attached) {
            Api.detach(openedRec.termId).catch(function () {});
          }
          if (viewMode === 'terminal') syncLiveChrome();
          else renderManagerView();
        }
        return;
      }

      if (ev.type === 'attached') {
        if (!ev.ok) {
          var failedTab = tabByTermId[String(ev.termId)];
          if (failedTab) {
            failedTab.opening = false;
            failedTab.connected = false;
          }
          if (activeTermId === ev.termId && viewMode === 'terminal') {
            showNotice(ev.error || '无法附着到终端会话', false);
            syncLiveChrome();
          } else if (findSession(ev.termId)) {
            showToast(ev.error || '无法附着到终端会话');
          }
          refreshSessionList();
          return;
        }

        if (activeTermId !== ev.termId || viewMode !== 'terminal') {
          var staleTab = tabByTermId[String(ev.termId)];
          if (staleTab) {
            staleTab.opening = false;
            staleTab.connected = false;
          }
          Api.detach(ev.termId).catch(function () {});
          return;
        }

        var attTab = ensureTab(ev.termId);
        attTab.opening = false;
        if (ev.session) {
          var synced = upsertSession(ev.session);
          if (synced) {
            attTab.name = synced.name;
            attTab.title = synced.title;
            attTab.cwd = synced.cwd;
          }
        }
        attTab.connected = true;
        attTab.disconnectNoticed = false;
        resetTabBuffer(attTab);
        hideNotice();
        syncLiveChrome();
        scheduleRender();
        focusTerminal();
        return;
      }

      if (ev.type === 'renamed') {
        if (!ev.ok) {
          if (renameTargetTermId === ev.termId) {
            renameInFlight = false;
            if (renameConfirmBtn) renameConfirmBtn.disabled = false;
            if (renameErrorEl) {
              renameErrorEl.textContent = ev.error || '重命名失败';
              renameErrorEl.hidden = false;
            }
          } else {
            showToast(ev.error || '重命名失败');
          }
          return;
        }
        var renSess = findSession(ev.termId);
        if (renSess) renSess.name = String(ev.name || renSess.name);
        var renTab = tabByTermId[String(ev.termId)];
        if (renTab) renTab.name = String(ev.name || renTab.name);
        if (renameTargetTermId === ev.termId) {
          closeRenameModal();
        }
        showToast('已重命名为 ' + ev.name);
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
        return;
      }

      if (ev.type === 'output') {
        var outTab = tabByTermId[String(ev.termId)];
        if (!outTab || !outTab.connected) return;
        var text;
        if (outTab.utf8Decoder && Array.isArray(ev.bytes) && ev.bytes.length > 0) {
          text = outTab.utf8Decoder.decode(new Uint8Array(ev.bytes), { stream: true });
        } else if (outTab.utf8Decoder && ev.bytes instanceof Uint8Array && ev.bytes.length > 0) {
          text = outTab.utf8Decoder.decode(ev.bytes, { stream: true });
        } else {
          text = ev.textUtf8Lossy || '';
        }
        feedText(outTab, text);
        return;
      }

      if (ev.type === 'exited') {
        var wasActive = activeTermId === ev.termId;
        var exTab = tabByTermId[String(ev.termId)];
        if (exTab && exTab.utf8Decoder) {
          var tail = exTab.utf8Decoder.decode();
          if (tail) feedText(exTab, tail);
        }
        delete tabByTermId[String(ev.termId)];
        sessions = sessions.filter(function (s) {
          return s.termId !== ev.termId;
        });
        if (renameTargetTermId === ev.termId) {
          closeRenameModal();
        }
        if (menuTargetTermId === ev.termId) {
          closeMoreMenu();
        }
        if (wasActive && viewMode === 'terminal') {
          activeTermId = null;
          setView('list');
          showToast('会话已终止');
        } else if (viewMode === 'terminal') {
          syncLiveChrome();
        } else {
          renderManagerView();
        }
      }
    }

    function handleDisconnectState(reasonText) {
      pendingOpenKinds = [];
      finishBgCreate();
      finishAttachCreate();
      pendingAttachViewTarget = false;
      closeMoreMenu();
      Object.keys(tabByTermId).forEach(function (k) {
        var tab = tabByTermId[k];
        tab.opening = false;
        tab.connected = false;
      });
      if (viewMode === 'terminal') {
        var activeTab = getActiveTab();
        if (activeTab && !activeTab.disconnectNoticed) {
          activeTab.disconnectNoticed = true;
          appendSystemLine(activeTab, '[' + (reasonText || '远控会话已断开') + ']');
        }
        syncLiveChrome();
      } else {
        renderManagerView();
      }
    }

    function applySessionInfo(raw) {
      var s = raw && typeof raw === 'object' ? raw : {};
      var wasReady = isSessionReady();
      sessionState.active = Boolean(s.active);
      sessionState.connected = Boolean(s.connected);
      sessionState.watchOnly = Boolean(
        s.watchOnly !== undefined ? s.watchOnly : s.watch_only
      );
      if (s.deviceName || s.device_name || s.target) {
        sessionState.deviceName = String(s.deviceName || s.device_name || s.target);
      }

      if (!sessionState.active) {
        handleDisconnectState(lastEndedReason || '远控会话已断开');
        return;
      }

      lastEndedReason = '';

      if (sessionState.watchOnly) {
        handleDisconnectState('当前为只读观看模式，无法使用远程终端');
        return;
      }

      if (!sessionState.connected) {
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
        return;
      }

      if (!wasReady) {
        resetSessionTimer();
        refreshSessionList();
        if (viewMode === 'terminal' && activeTermId != null) {
          openSessionTerminal(activeTermId);
        }
      }
      if (viewMode === 'list') renderManagerView();
      else syncLiveChrome();
    }

    function handleSessionUpdate(upd) {
      if (!upd || !upd.kind) return;
      if (upd.kind === 'ended') {
        var reason = (upd.data && upd.data.reason) || '远控会话已断开';
        lastEndedReason = reason;
        sessionState.active = false;
        sessionState.connected = false;
        sessionState.watchOnly = false;
        handleDisconnectState(reason);
        return;
      }
      if (upd.kind === 'negotiated') {
        var data = upd.data || {};
        var wasReady = isSessionReady();
        lastEndedReason = '';
        sessionState.active = true;
        sessionState.connected = true;
        if (typeof data.watch_only === 'boolean') {
          sessionState.watchOnly = data.watch_only;
        }
        if (data.device_name || data.device) {
          sessionState.deviceName = String(data.device_name || data.device);
        }
        if (sessionState.watchOnly) {
          handleDisconnectState('当前为只读观看模式，无法使用远程终端');
          return;
        }
        if (!wasReady) {
          resetSessionTimer();
          refreshSessionList();
          if (viewMode === 'terminal' && activeTermId != null) {
            openSessionTerminal(activeTermId);
          }
        }
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
      }
    }

    /* ── ResizeObserver（100ms 防抖） ───────────────────────── */

    function handleResize() {
      if (viewMode !== 'terminal') return;
      var grid = computeGrid();
      if (grid.cols === lastCols && grid.rows === lastRows) return;
      lastCols = grid.cols;
      lastRows = grid.rows;
      var tab = getActiveTab();
      if (!tab) return;
      if (tab.inAltScreen) {
        while (tab.lines.length > lastRows) {
          var popped = tab.lines.pop();
          if (popped && popped.node) {
            removedNodes.push(popped.node);
          }
        }
        while (tab.lines.length < lastRows) {
          tab.lines.push(makeLine(false));
        }
        tab.curLineIdx = Math.max(0, Math.min(lastRows - 1, tab.curLineIdx));
        tab.curCol = Math.max(0, Math.min(lastCols - 1, tab.curCol));
        if (tab.scrollTop >= lastRows) {
          tab.scrollTop = 0;
          tab.scrollBottom = -1;
        }
        fullRebuild = true;
        scheduleRender();
      } else {
        tab.curCol = Math.max(0, Math.min(lastCols - 1, tab.curCol));
        if (tab.scrollTop >= lastRows) {
          tab.scrollTop = 0;
          tab.scrollBottom = -1;
        }
      }
      if (tab.connected && tab.termId != null) {
        Api.resize(tab.termId, grid.cols, grid.rows).catch(function () {});
      }
    }

    if (typeof window.ResizeObserver === 'function') {
      var ro = new window.ResizeObserver(function () {
        window.clearTimeout(resizeTimer);
        resizeTimer = window.setTimeout(handleResize, RESIZE_DEBOUNCE_MS);
      });
      ro.observe(screenEl);
    } else {
      window.addEventListener('resize', function () {
        window.clearTimeout(resizeTimer);
        resizeTimer = window.setTimeout(handleResize, RESIZE_DEBOUNCE_MS);
      });
    }

    /* ── 窗口关闭仅脱离 UI（保持被控端后台 PTY 存活） ─────────── */

    window.addEventListener('beforeunload', function () {
      if (IS_TAURI && activeTermId != null) {
        Bridge.terminalDetach(activeTermId).catch(function () {});
      }
    });

    /* ── 初始化监听与首屏拉取 ──────────────────────────────── */

    setView('list');

    if (!IS_TAURI) {
      refreshSessionList();
      return;
    }

    Promise.all([
      Promise.resolve(Bridge.onTerminalEvent(handleTerminalEvent)).catch(function () {}),
      Promise.resolve(
        typeof Bridge.onSessionUpdate === 'function'
          ? Bridge.onSessionUpdate(handleSessionUpdate)
          : null
      ).catch(function () {}),
      Promise.resolve(
        typeof Bridge.onActiveSessionState === 'function'
          ? Bridge.onActiveSessionState(applySessionInfo)
          : null
      ).catch(function () {})
    ]).then(function () {
      if (typeof Bridge.getActiveSessionInfo === 'function') {
        Bridge.getActiveSessionInfo()
          .then(applySessionInfo)
          .catch(function () {
            renderManagerView();
          });
      } else {
        sessionState.active = true;
        sessionState.connected = true;
        refreshSessionList();
      }
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
