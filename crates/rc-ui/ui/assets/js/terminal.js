/* terminal.js —— W3 终端远控管理器（亮色会话列表 + xterm.js 暗色交互终端，零 innerHTML）
 *
 * 架构与约束：
 *   - 视图 1（list）：展示远程设备状态、创建后台会话按钮与会话卡片列表（打开终端 / ... 重命名 & 终止会话）；
 *   - 视图 2（terminal）：基于离线内置 xterm.js（+ FitAddon + Unicode11Addon）的完整 VT100/xterm-256color/
 *     TrueColor 终端模拟器，原生兼容 agy / vim / htop / tmux 等复杂交互式 TUI、CJK 双倍字符宽、
 *     括号粘贴、中文 IME 组合输入与 100ms 防抖自适应 Resize；
 *   - 关闭窗口或返回列表仅执行 detach（暂停实时流），不会终止远端后台 PTY，仅在点击「终止会话」时关闭 PTY；
 *   - 严禁使用 innerHTML / insertAdjacentHTML：所有自定义 DOM 节点一律经 createElement / createElementNS + textContent 渲染。
 */

(function () {
  'use strict';

  var Bridge = window.RCBridge;
  var IS_TAURI = Boolean(Bridge && Bridge.isTauri);

  var MAX_SESSIONS = 8;
  var MAX_NAME_CHARS = 64;
  var RESIZE_DEBOUNCE_MS = 100;
  var SVG_NS = 'http://www.w3.org/2000/svg';
  var DEFAULT_FONT_FAMILY = '"SF Mono", Menlo, Monaco, Consolas, "PingFang SC", monospace';

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

  function errMessage(err, fallback) {
    if (!err) return fallback || '操作失败';
    if (typeof err === 'string') return err;
    if (err.message) return String(err.message);
    return String(err);
  }

  function readCssToken(name, fallback) {
    try {
      var val = window.getComputedStyle(document.documentElement).getPropertyValue(name);
      if (val && val.trim()) return val.trim();
    } catch (_e) {
      /* ignore */
    }
    return fallback;
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

    var fontBtnEl = document.getElementById('tm-font-btn');
    var fontBackdropEl = document.getElementById('tm-font-backdrop');
    var fontTabLocalEl = document.getElementById('tm-font-tab-local');
    var fontTabRemoteEl = document.getElementById('tm-font-tab-remote');
    var fontSearchEl = document.getElementById('tm-font-search');
    var fontMonoOnlyEl = document.getElementById('tm-font-mono-only');
    var fontRefreshBtn = document.getElementById('tm-font-refresh');
    var fontListEl = document.getElementById('tm-font-list');
    var fontSizeInputEl = document.getElementById('tm-font-size');
    var fontSizeValEl = document.getElementById('tm-font-size-val');
    var fontPreviewEl = document.getElementById('tm-font-preview');
    var fontStatusEl = document.getElementById('tm-font-status');
    var fontResetBtn = document.getElementById('tm-font-reset');
    var fontCancelBtn = document.getElementById('tm-font-cancel');
    var fontApplyBtn = document.getElementById('tm-font-apply');

    var lockGateEl = document.getElementById('tm-lock-gate');
    var lockUserBadgeEl = document.getElementById('tm-lock-user-badge');
    var lockUserNameEl = document.getElementById('tm-lock-user-name');
    var lockFormEl = document.getElementById('tm-lock-form');
    var lockUserRowEl = document.getElementById('tm-lock-user-row');
    var lockUsernameEl = document.getElementById('tm-lock-username');
    var lockPasswordEl = document.getElementById('tm-lock-password');
    var lockToggleVisBtn = document.getElementById('tm-lock-toggle-vis');
    var lockRememberEl = document.getElementById('tm-lock-remember');
    var lockErrorEl = document.getElementById('tm-lock-error');
    var lockClearSavedBtn = document.getElementById('tm-lock-clear-saved');
    var lockSubmitBtn = document.getElementById('tm-lock-submit');

    var toastEl = document.getElementById('tm-toast');

    if (!winEl || !listViewEl || !liveViewEl || !screenEl) return;

    /* ── 状态管理 ──────────────────────────────────────────── */
    var initialSessionLoaded = !IS_TAURI;
    var sessionState = {
      active: !IS_TAURI,
      connected: !IS_TAURI,
      watchOnly: false,
      phase: !IS_TAURI ? 'connected' : '',
      reconnectingAttempt: 0,
      reconnectingMax: 8,
      reconnectingReason: '',
      deviceName: IS_TAURI ? '' : 'yii’s MacBook Pro'
    };
    var lastEndedReason = '';
    var terminalLockState = {
      required: false,
      unlocked: false,
      pending: false,
      username: '',
      retryAfterSecs: 0,
      hasSavedPassword: false,
      deviceKey: '',
      allowTerminal: true,
      submitting: false,
      autoAttempted: false,
      autoInFlight: false,
      cooldownUntilMs: 0
    };
    var lockStateSeq = 0;
    var lockCooldownTimer = 0;
    var mgrBannerEl = null;
    var mgrBannerTextEl = null;
    var mgrBannerBtnEl = null;
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

    /* ── xterm.js 终端实例 ─────────────────────────────────── */
    var xterm = null;
    var fitAddon = null;
    var xtermMounted = false;

    function buildXtermTheme() {
      return {
        background: readCssToken('--terminal-bg', '#1A1B1F'),
        foreground: readCssToken('--terminal-fg', '#F2F3F5'),
        cursor: readCssToken('--accent', '#3370FF'),
        cursorAccent: readCssToken('--terminal-bg', '#1A1B1F'),
        selectionBackground: 'rgba(51, 112, 255, 0.35)',
        black: '#18181B',
        red: '#EF4444',
        green: '#22C55E',
        yellow: '#EAB308',
        blue: '#3B82F6',
        magenta: '#A855F7',
        cyan: '#06B6D4',
        white: '#E4E4E7',
        brightBlack: '#71717A',
        brightRed: '#F87171',
        brightGreen: '#4ADE80',
        brightYellow: '#FACC15',
        brightBlue: '#60A5FA',
        brightMagenta: '#C084FC',
        brightCyan: '#22D3EE',
        brightWhite: '#FAFAFA'
      };
    }

    function copyTextToClipboard(text) {
      if (!text) return false;
      if (Bridge && typeof Bridge.copyText === 'function') {
        Bridge.copyText(text).catch(function () {});
        return true;
      }
      if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
        navigator.clipboard.writeText(text).catch(function () {});
        return true;
      }
      return false;
    }

    function isAnyModalOpen() {
      if (renameBackdropEl && !renameBackdropEl.hidden) return true;
      if (fontBackdropEl && !fontBackdropEl.hidden) return true;
      if (lockGateEl && !lockGateEl.hidden) return true;
      return false;
    }

    function ensureXterm() {
      if (xterm) return xterm;
      if (typeof window.Terminal !== 'function') {
        return null;
      }
      xterm = new window.Terminal({
        allowProposedApi: true,
        cursorBlink: true,
        cursorStyle: 'block',
        scrollback: 5000,
        convertEol: false,
        macOptionClickForcesSelection: true,
        fontFamily: resolveActiveFontFamilyCss(),
        fontSize: clampFontSize(fontState ? fontState.size : 13),
        lineHeight: 1.32,
        theme: buildXtermTheme()
      });

      if (window.FitAddon && typeof window.FitAddon.FitAddon === 'function') {
        fitAddon = new window.FitAddon.FitAddon();
        xterm.loadAddon(fitAddon);
      }

      if (window.Unicode11Addon && typeof window.Unicode11Addon.Unicode11Addon === 'function') {
        try {
          var u11 = new window.Unicode11Addon.Unicode11Addon();
          xterm.loadAddon(u11);
          if (xterm.unicode) {
            xterm.unicode.activeVersion = '11';
          }
        } catch (_e) {
          /* ignore if unsupported */
        }
      }

      xterm.onData(function (data) {
        if (viewMode !== 'terminal' || isAnyModalOpen()) return;
        var tab = getActiveTab();
        if (!tab || !tab.connected || tab.termId == null || !data) return;
        Api.input(tab.termId, data).catch(function () {});
      });

      xterm.onBinary(function (data) {
        if (viewMode !== 'terminal' || isAnyModalOpen()) return;
        var tab = getActiveTab();
        if (!tab || !tab.connected || tab.termId == null || !data) return;
        Api.input(tab.termId, data).catch(function () {});
      });

      xterm.onTitleChange(function (title) {
        var tab = getActiveTab();
        if (tab && title) {
          tab.title = title;
          syncLiveChrome();
        }
      });

      xterm.attachCustomKeyEventHandler(function (e) {
        if (viewMode !== 'terminal' || isAnyModalOpen()) return false;
        if (e.type !== 'keydown') return true;
        var lower = (e.key || '').toLowerCase();

        // macOS Cmd+C：选中文本时复制并阻止 WKWebView 默认菜单竞态覆写剪贴板，无选中时忽略
        if (e.metaKey && !e.ctrlKey && !e.altKey && lower === 'c') {
          if (xterm.hasSelection()) {
            e.preventDefault();
            copyTextToClipboard(xterm.getSelection());
            return false;
          }
          return false;
        }
        // macOS Cmd+V：交由浏览器原生 paste 事件（xterm 内部自动处理括号粘贴）
        if (e.metaKey && !e.ctrlKey && !e.altKey && lower === 'v') {
          return false;
        }
        // macOS Cmd+A：全选终端缓冲
        if (e.metaKey && !e.ctrlKey && !e.altKey && lower === 'a') {
          e.preventDefault();
          xterm.selectAll();
          return false;
        }
        // macOS Cmd+K：清屏
        if (e.metaKey && !e.ctrlKey && !e.altKey && lower === 'k') {
          e.preventDefault();
          xterm.clear();
          return false;
        }
        // Ctrl+Shift+C / Ctrl+Shift+V 兼容 Linux/Win 习惯
        if (e.ctrlKey && e.shiftKey && !e.metaKey && !e.altKey) {
          if (lower === 'c' && xterm.hasSelection()) {
            copyTextToClipboard(xterm.getSelection());
            return false;
          }
          if (lower === 'v') {
            if (navigator.clipboard && typeof navigator.clipboard.readText === 'function') {
              navigator.clipboard
                .readText()
                .then(function (txt) {
                  if (txt && xterm) xterm.paste(txt);
                })
                .catch(function () {});
            }
            return false;
          }
        }
        return true;
      });

      return xterm;
    }

    function mountXtermIfNeeded() {
      var t = ensureXterm();
      if (!t) return null;
      if (!xtermMounted) {
        screenEl.textContent = '';
        t.open(screenEl);
        xtermMounted = true;
        applyActiveFontFromState();
      }
      return t;
    }

    function computeAndFitGrid(forceNotify) {
      var t = mountXtermIfNeeded();
      var cols = lastCols > 0 ? lastCols : 80;
      var rows = lastRows > 0 ? lastRows : 24;
      if (t && fitAddon && viewMode === 'terminal') {
        try {
          var dims = fitAddon.proposeDimensions();
          if (dims && dims.cols && dims.rows && isFinite(dims.cols) && isFinite(dims.rows)) {
            cols = Math.min(512, Math.max(40, Math.floor(dims.cols)));
            rows = Math.min(512, Math.max(12, Math.floor(dims.rows)));
            if (t.cols !== cols || t.rows !== rows) {
              t.resize(cols, rows);
            }
          }
        } catch (_e) {
          /* ignore layout-not-ready */
        }
      }
      var changed = cols !== lastCols || rows !== lastRows;
      lastCols = cols;
      lastRows = rows;
      if (changed || forceNotify) {
        var tab = getActiveTab();
        if (tab && tab.connected && tab.termId != null) {
          Api.resize(tab.termId, cols, rows).catch(function () {});
        }
      }
      return { cols: cols, rows: rows };
    }

    function resetXtermView() {
      var t = mountXtermIfNeeded();
      if (t) {
        t.reset();
      }
    }

    function writeSystemNoticeToXterm(text) {
      var t = mountXtermIfNeeded();
      if (t && text) {
        t.writeln('\r\n\x1b[90m' + String(text) + '\x1b[0m');
      }
    }

    function showToast(msg) {
      if (!toastEl || !msg) return;
      toastEl.textContent = String(msg);
      toastEl.classList.add('is-visible');
      window.clearTimeout(toastTimer);
      toastTimer = window.setTimeout(function () {
        toastEl.classList.remove('is-visible');
      }, 2000);
    }

    var lockGateWasVisible = false;
    var lockGateWasAutoInFlight = false;

    function isSessionReady() {
      return Boolean(
        sessionState.active &&
          sessionState.connected &&
          !sessionState.watchOnly &&
          terminalLockState.allowTerminal !== false &&
          !terminalLockState.pending &&
          (!terminalLockState.required || terminalLockState.unlocked)
      );
    }

    function resetLockPasswordVisibility() {
      if (lockPasswordEl) lockPasswordEl.type = 'password';
      if (lockToggleVisBtn) lockToggleVisBtn.textContent = '显示';
    }

    function focusLockGateInput() {
      if (!lockGateEl || lockGateEl.hidden || terminalLockState.autoInFlight) return;
      if (lockUserRowEl && !lockUserRowEl.hidden && lockUsernameEl && !lockUsernameEl.value) {
        lockUsernameEl.focus();
      } else if (lockPasswordEl) {
        lockPasswordEl.focus();
      }
    }

    function remainingCooldownSecs() {
      if (!terminalLockState.cooldownUntilMs) return 0;
      var diff = Math.ceil((terminalLockState.cooldownUntilMs - Date.now()) / 1000);
      return diff > 0 ? diff : 0;
    }

    function stopLockCooldownTimer() {
      if (lockCooldownTimer) {
        window.clearInterval(lockCooldownTimer);
        lockCooldownTimer = 0;
      }
      terminalLockState.cooldownUntilMs = 0;
    }

    function setLockCooldown(secs) {
      var n = Math.max(0, Math.round(Number(secs) || 0));
      if (n <= 0) {
        stopLockCooldownTimer();
        return;
      }
      terminalLockState.cooldownUntilMs = Date.now() + n * 1000;
      if (!lockCooldownTimer) {
        lockCooldownTimer = window.setInterval(function () {
          if (remainingCooldownSecs() <= 0) {
            stopLockCooldownTimer();
          }
          syncLockGateUi();
        }, 500);
      }
    }

    function setLockError(msg) {
      if (!lockErrorEl) return;
      if (msg) {
        lockErrorEl.textContent = String(msg);
        lockErrorEl.hidden = false;
      } else {
        lockErrorEl.textContent = '';
        lockErrorEl.hidden = true;
      }
    }

    function syncLockGateUi() {
      if (!lockGateEl) return;
      var blockedByPolicy =
        terminalLockState.allowTerminal === false ||
        !sessionState.active ||
        !sessionState.connected ||
        sessionState.watchOnly;
      var shouldShow = Boolean(
        !blockedByPolicy && terminalLockState.required && !terminalLockState.unlocked
      );
      lockGateEl.hidden = !shouldShow;
      if (!shouldShow) {
        lockGateWasVisible = false;
        lockGateWasAutoInFlight = false;
        stopLockCooldownTimer();
        resetLockPasswordVisibility();
        if (terminalLockState.unlocked && lockPasswordEl) {
          lockPasswordEl.value = '';
          setLockError('');
        }
        return;
      }

      var justBecameVisible = !lockGateWasVisible;
      if (justBecameVisible) {
        lockGateWasVisible = true;
        if (renameBackdropEl && !renameBackdropEl.hidden) {
          closeRenameModal();
        }
        if (fontBackdropEl && !fontBackdropEl.hidden) {
          closeFontModal();
        }
      }
      var autoJustEnded = lockGateWasAutoInFlight && !terminalLockState.autoInFlight;
      lockGateWasAutoInFlight = Boolean(terminalLockState.autoInFlight);

      if (lockUserBadgeEl && lockUserNameEl && lockUserRowEl) {
        if (terminalLockState.username) {
          lockUserNameEl.textContent = terminalLockState.username;
          lockUserBadgeEl.hidden = false;
          lockUserRowEl.hidden = true;
        } else {
          lockUserBadgeEl.hidden = true;
          lockUserRowEl.hidden = false;
        }
      }
      if (lockClearSavedBtn) {
        lockClearSavedBtn.hidden = !terminalLockState.hasSavedPassword;
        lockClearSavedBtn.disabled =
          terminalLockState.submitting || terminalLockState.autoInFlight;
      }
      if (lockSubmitBtn) {
        var rem = remainingCooldownSecs();
        if (terminalLockState.autoInFlight) {
          lockSubmitBtn.disabled = true;
          lockSubmitBtn.textContent = '正在自动验证已保存密码…';
        } else if (terminalLockState.submitting) {
          lockSubmitBtn.disabled = true;
          lockSubmitBtn.textContent = '验证中…';
        } else if (rem > 0) {
          lockSubmitBtn.disabled = true;
          lockSubmitBtn.textContent = '请等待 ' + rem + 's';
        } else {
          lockSubmitBtn.disabled = false;
          lockSubmitBtn.textContent = '验证并进入';
        }
      }
      if (justBecameVisible || autoJustEnded) {
        focusLockGateInput();
      }
    }

    function maybeTriggerAutoUnlock() {
      if (
        !IS_TAURI ||
        !sessionState.active ||
        !sessionState.connected ||
        sessionState.watchOnly ||
        terminalLockState.allowTerminal === false ||
        !terminalLockState.required ||
        terminalLockState.unlocked ||
        !terminalLockState.hasSavedPassword ||
        terminalLockState.autoAttempted ||
        terminalLockState.autoInFlight ||
        remainingCooldownSecs() > 0 ||
        typeof Bridge.terminalTryAutoUnlock !== 'function'
      ) {
        return;
      }
      terminalLockState.autoAttempted = true;
      terminalLockState.autoInFlight = true;
      var autoSeq = lockStateSeq;
      setLockError('');
      syncLockGateUi();
      Bridge.terminalTryAutoUnlock()
        .then(function (submitted) {
          if (autoSeq !== lockStateSeq) return;
          if (!submitted) {
            terminalLockState.autoInFlight = false;
            syncLockGateUi();
          }
        })
        .catch(function () {
          if (autoSeq !== lockStateSeq) return;
          terminalLockState.autoInFlight = false;
          syncLockGateUi();
        });
    }

    function resetLockGateSessionState(resetAllowTerminal) {
      lockStateSeq += 1;
      terminalLockState.required = false;
      terminalLockState.unlocked = false;
      terminalLockState.pending = false;
      terminalLockState.username = '';
      terminalLockState.hasSavedPassword = false;
      terminalLockState.deviceKey = '';
      terminalLockState.submitting = false;
      terminalLockState.autoAttempted = false;
      terminalLockState.autoInFlight = false;
      if (resetAllowTerminal) {
        terminalLockState.allowTerminal = true;
      }
      if (lockPasswordEl) lockPasswordEl.value = '';
      if (lockUsernameEl) lockUsernameEl.value = '';
      setLockError('');
      resetLockPasswordVisibility();
      stopLockCooldownTimer();
      syncLockGateUi();
    }

    function fetchTerminalLockState() {
      var reqSeq = ++lockStateSeq;
      if (!IS_TAURI || typeof Bridge.terminalGetLockState !== 'function') {
        terminalLockState.pending = false;
        return Promise.resolve(null);
      }
      if (!terminalLockState.required && !terminalLockState.unlocked) {
        terminalLockState.pending = true;
      }
      return Bridge.terminalGetLockState()
        .then(function (snap) {
          if (reqSeq !== lockStateSeq) return null;
          terminalLockState.pending = false;
          if (!sessionState.active || !sessionState.connected) return null;
          if (!snap || typeof snap !== 'object') return null;
          if (typeof snap.allowTerminal === 'boolean') {
            terminalLockState.allowTerminal = snap.allowTerminal;
          }
          if (typeof snap.required === 'boolean') {
            terminalLockState.required = snap.required;
          }
          if (typeof snap.unlocked === 'boolean') {
            terminalLockState.unlocked = snap.unlocked;
          }
          terminalLockState.username = snap.username ? String(snap.username) : '';
          terminalLockState.hasSavedPassword = Boolean(snap.hasSavedPassword);
          terminalLockState.deviceKey = snap.deviceKey ? String(snap.deviceKey) : '';
          var retrySecs = Number(snap.retryAfterSecs || 0);
          if (retrySecs > 0) {
            setLockCooldown(retrySecs);
          }
          syncLockGateUi();
          if (viewMode === 'list') renderManagerView();
          else syncLiveChrome();
          maybeTriggerAutoUnlock();
          return snap;
        })
        .catch(function () {
          if (reqSeq === lockStateSeq) {
            terminalLockState.pending = false;
          }
          return null;
        });
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
        cwd: sess ? sess.cwd : ''
      };
      tabByTermId[key] = tab;
      return tab;
    }

    function getActiveTab() {
      if (activeTermId == null) return null;
      return tabByTermId[String(activeTermId)] || null;
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
        mountXtermIfNeeded();
        syncLiveChrome();
        window.requestAnimationFrame(function () {
          computeAndFitGrid(false);
        });
      } else {
        liveViewEl.hidden = true;
        listViewEl.hidden = false;
        if (backBtn) backBtn.hidden = true;
        syncTimerRunning();
        renderManagerView();
      }
    }

    function triggerSessionReconnect() {
      if (!IS_TAURI) {
        showToast('正在尝试重新连接…（预览）');
        return;
      }
      var fn =
        typeof Bridge.triggerReconnectNow === 'function'
          ? Bridge.triggerReconnectNow
          : typeof Bridge.reconnectLastSession === 'function'
            ? Bridge.reconnectLastSession
            : null;
      if (!fn) {
        showToast('当前版本暂不支持自动重连，请从主窗口重新连接');
        return;
      }
      sessionState.active = true;
      sessionState.connected = false;
      sessionState.phase = 'connecting';
      lastEndedReason = '';
      if (viewMode === 'list') {
        renderManagerView();
      } else {
        hideNotice();
        syncLiveChrome();
      }
      showToast('正在重新连接…');
      fn().catch(function (err) {
        var msg = errMessage(err, '重新连接失败');
        sessionState.active = false;
        sessionState.connected = false;
        sessionState.phase = 'ended';
        lastEndedReason = msg;
        handleDisconnectState(msg);
        showToast(msg);
      });
    }

    function ensureMgrBanner() {
      if (mgrBannerEl || !listViewEl) return mgrBannerEl;
      var box = document.createElement('div');
      box.className = 'tm-mgr-banner';
      box.hidden = true;

      var txt = document.createElement('span');
      txt.className = 'tm-mgr-banner__text';
      box.appendChild(txt);

      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'tm-btn-light tm-btn-light--primary';
      btn.textContent = '重新连接';
      btn.addEventListener('click', function () {
        triggerSessionReconnect();
      });
      box.appendChild(btn);

      if (sessionListEl && sessionListEl.parentNode === listViewEl) {
        listViewEl.insertBefore(box, sessionListEl);
      } else {
        listViewEl.appendChild(box);
      }
      mgrBannerEl = box;
      mgrBannerTextEl = txt;
      mgrBannerBtnEl = btn;
      return mgrBannerEl;
    }

    function syncMgrBanner() {
      var banner = ensureMgrBanner();
      if (!banner || !mgrBannerTextEl || !mgrBannerBtnEl) return;
      if (!IS_TAURI || !initialSessionLoaded || isSessionReady() || sessionState.watchOnly) {
        banner.hidden = true;
        return;
      }
      if (sessionState.phase === 'reconnecting') {
        var attempt = sessionState.reconnectingAttempt || 1;
        var max = sessionState.reconnectingMax || 8;
        var detail = sessionState.reconnectingReason ? '（' + sessionState.reconnectingReason + '）' : '';
        mgrBannerTextEl.textContent =
          '连接中断，正在自动重连 (' + attempt + '/' + max + ')…' + detail;
        mgrBannerBtnEl.textContent = '立即重试';
        banner.hidden = false;
        return;
      }
      if (sessionState.active && !sessionState.connected) {
        banner.hidden = true;
        return;
      }
      if (!sessionState.active) {
        mgrBannerTextEl.textContent = lastEndedReason || '远控连接已断开';
        mgrBannerBtnEl.textContent = '重新连接';
        banner.hidden = false;
        return;
      }
      banner.hidden = true;
    }

    function renderManagerView() {
      if (deviceNameEl) {
        deviceNameEl.textContent = sessionState.deviceName || '远程设备';
      }

      var ready = isSessionReady();
      if (mgrDotEl && mgrStatusTextEl) {
        mgrDotEl.classList.remove('is-online', 'is-warning');
        if (terminalLockState.allowTerminal === false && sessionState.active) {
          mgrDotEl.classList.add('is-warning');
          mgrStatusTextEl.textContent = '仅限本人使用';
        } else if (ready) {
          mgrDotEl.classList.add('is-online');
          mgrStatusTextEl.textContent = '已连接';
        } else if (sessionState.watchOnly) {
          mgrDotEl.classList.add('is-warning');
          mgrStatusTextEl.textContent = '只读观看模式';
        } else if (sessionState.phase === 'reconnecting') {
          mgrDotEl.classList.add('is-warning');
          mgrStatusTextEl.textContent =
            '自动重连中 (' +
            (sessionState.reconnectingAttempt || 1) +
            '/' +
            (sessionState.reconnectingMax || 8) +
            ')…';
        } else if (sessionState.active && !sessionState.connected) {
          mgrDotEl.classList.add('is-warning');
          mgrStatusTextEl.textContent = '正在建立安全连接…';
        } else if (!initialSessionLoaded) {
          mgrStatusTextEl.textContent = '连接中…';
        } else {
          mgrStatusTextEl.textContent = '未连接';
        }
      }

      syncMgrBanner();

      if (titleEl && viewMode === 'list') {
        titleEl.textContent = '终端远控';
        document.title = '终端远控 — RabbitControl';
      }

      if (createBtn) {
        createBtn.disabled =
          !ready ||
          bgCreating ||
          sessions.length >= MAX_SESSIONS ||
          (terminalLockState.required && !terminalLockState.unlocked);
        createBtn.textContent = bgCreating ? '创建中…' : '创建会话';
      }

      if (!sessionListEl || !emptyStateEl) return;
      sessionListEl.textContent = '';

      if (!sessions.length) {
        sessionListEl.hidden = true;
        emptyStateEl.hidden = false;
        if (emptyTextEl) {
          if (terminalLockState.allowTerminal === false && sessionState.active) {
            emptyTextEl.textContent = '远程终端仅限本人账号使用，当前为他人协助会话';
          } else if (sessionState.watchOnly) {
            emptyTextEl.textContent = '当前为只读观看模式，无法管理或打开远程终端';
          } else if (sessionState.phase === 'reconnecting') {
            emptyTextEl.textContent =
              '连接中断，正在自动重连 (' +
              (sessionState.reconnectingAttempt || 1) +
              '/' +
              (sessionState.reconnectingMax || 8) +
              ')…';
          } else if (sessionState.active && !sessionState.connected) {
            emptyTextEl.textContent = '正在建立安全连接，请稍候…';
          } else if (!initialSessionLoaded) {
            emptyTextEl.textContent = '正在检查远控连接状态…';
          } else if (!sessionState.active) {
            emptyTextEl.textContent = lastEndedReason || '远控会话未连接，可点击上方「重新连接」恢复';
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

    /* ── 终端字体管理（本机 / 远程机器字体 + 字号） ───────────── */
    var FONT_STORAGE_KEY = 'rc.terminal.fontPrefs.v1';

    function clampFontSize(raw) {
      var n = Math.round(Number(raw) || 13);
      if (!isFinite(n)) return 13;
      return Math.max(10, Math.min(28, n));
    }

    function readCachedFontPrefs() {
      try {
        var raw = window.localStorage ? window.localStorage.getItem(FONT_STORAGE_KEY) : null;
        if (!raw) return null;
        var parsed = JSON.parse(raw);
        if (!parsed || typeof parsed !== 'object') return null;
        var src = String(parsed.source || 'default');
        if (src !== 'local' && src !== 'remote' && src !== 'default') src = 'default';
        return {
          source: src,
          family: src === 'default' || !parsed.family ? '' : String(parsed.family),
          fontId: src === 'default' || !parsed.fontId ? '' : String(parsed.fontId),
          size: clampFontSize(parsed.size)
        };
      } catch (_e) {
        return null;
      }
    }

    var fontState = {
      source: 'default', /* 'default' | 'local' | 'remote' */
      family: '',
      fontId: '',
      size: 13,
      modalTab: 'local',
      draftSource: 'default',
      draftFamily: '',
      draftFontId: '',
      draftSize: 13,
      localFonts: null,
      remoteFonts: null,
      localLoading: false,
      remoteLoading: false,
      loadedAliases: Object.create(null),
      loadingLocalFaces: Object.create(null),
      pendingFetch: null,
      autoRestoredRemote: false
    };

    var cachedFontInit = readCachedFontPrefs();
    if (cachedFontInit) {
      fontState.source = cachedFontInit.source;
      fontState.family = cachedFontInit.family;
      fontState.fontId = cachedFontInit.fontId;
      fontState.size = cachedFontInit.size;
    }

    function writeCachedFontPrefs() {
      try {
        if (!window.localStorage) return;
        window.localStorage.setItem(
          FONT_STORAGE_KEY,
          JSON.stringify({
            source: fontState.source,
            family: fontState.source === 'default' ? '' : fontState.family || '',
            fontId: fontState.source === 'default' ? '' : fontState.fontId || '',
            size: clampFontSize(fontState.size)
          })
        );
      } catch (_e) {
        /* ignore storage restrictions */
      }
    }

    function sanitizeFontFamilyCss(name) {
      return String(name || '').replace(/['"\\;\r\n{}]/g, '').trim();
    }

    function remoteFontAlias(fontId) {
      return 'rc-remote-' + String(fontId || '').replace(/[^a-zA-Z0-9_-]/g, '').slice(0, 32);
    }

    function localAliasKey(fontId, family) {
      if (fontId) return 'local:' + String(fontId);
      if (family) return 'local-fam:' + String(family).toLowerCase();
      return '';
    }

    function localFontAlias(fontId, family) {
      var raw = String(fontId || family || 'font').replace(/[^a-zA-Z0-9_-]/g, '');
      return 'rc-local-' + (raw || 'font').slice(0, 32);
    }

    function toArrayBuffer(raw) {
      if (raw instanceof ArrayBuffer) return raw;
      if (ArrayBuffer.isView(raw)) {
        return raw.buffer.slice(raw.byteOffset, raw.byteOffset + raw.byteLength);
      }
      if (Array.isArray(raw)) {
        return new Uint8Array(raw).buffer;
      }
      return null;
    }

    function setFontStatus(msg) {
      if (fontStatusEl) fontStatusEl.textContent = msg ? String(msg) : '';
    }

    function resolveActiveFontFamilyCss() {
      if (fontState.source === 'local' && (fontState.family || fontState.fontId)) {
        var locKey = localAliasKey(fontState.fontId, fontState.family);
        var locAlias = locKey ? fontState.loadedAliases[locKey] : '';
        var safeLoc = sanitizeFontFamilyCss(fontState.family);
        var parts = [];
        if (locAlias) parts.push('"' + locAlias + '"');
        if (safeLoc) parts.push('"' + safeLoc + '"');
        if (parts.length > 0) {
          return parts.join(', ') + ', ' + DEFAULT_FONT_FAMILY;
        }
      }
      if (fontState.source === 'remote' && fontState.fontId) {
        var alias = fontState.loadedAliases[fontState.fontId];
        if (alias) {
          return '"' + alias + '", ' + DEFAULT_FONT_FAMILY;
        }
        var safeRem = sanitizeFontFamilyCss(fontState.family);
        if (safeRem) {
          return '"' + safeRem + '", ' + DEFAULT_FONT_FAMILY;
        }
      }
      return DEFAULT_FONT_FAMILY;
    }

    function applyFontToXterm(fontFamilyCss, sizePx) {
      var sz = clampFontSize(sizePx);
      var fam = fontFamilyCss || DEFAULT_FONT_FAMILY;
      var t = ensureXterm();
      if (t) {
        t.options.fontFamily = fam;
        t.options.fontSize = sz;
        if (xtermMounted && viewMode === 'terminal') {
          window.requestAnimationFrame(function () {
            computeAndFitGrid(true);
            if (xterm && typeof xterm.refresh === 'function') {
              xterm.refresh(0, Math.max(0, xterm.rows - 1));
            }
          });
          if (
            document.fonts &&
            document.fonts.ready &&
            typeof document.fonts.ready.then === 'function'
          ) {
            document.fonts.ready.then(function () {
              if (xterm && xtermMounted && viewMode === 'terminal') {
                computeAndFitGrid(true);
                if (typeof xterm.refresh === 'function') {
                  xterm.refresh(0, Math.max(0, xterm.rows - 1));
                }
              }
            });
          }
        }
      }
    }

    function applyActiveFontFromState() {
      applyFontToXterm(resolveActiveFontFamilyCss(), fontState.size);
    }

    function ensureLocalFontFaceLoaded(fontId, family) {
      var key = localAliasKey(fontId, family);
      if (!key || fontState.loadedAliases[key] || fontState.loadingLocalFaces[key]) {
        return Promise.resolve();
      }
      if (
        !IS_TAURI ||
        typeof Bridge.terminalGetLocalFontBytes !== 'function' ||
        typeof window.FontFace !== 'function'
      ) {
        return Promise.resolve();
      }
      fontState.loadingLocalFaces[key] = true;
      return Bridge.terminalGetLocalFontBytes(fontId || '', family || null)
        .then(function (raw) {
          var buf = toArrayBuffer(raw);
          if (!buf || buf.byteLength === 0) return;
          var targetAlias = localFontAlias(fontId, family);
          var face = new window.FontFace(targetAlias, buf);
          return face.load().then(function (loadedFace) {
            if (document.fonts && typeof document.fonts.add === 'function') {
              document.fonts.add(loadedFace);
            }
            fontState.loadedAliases[key] = targetAlias;
            if (
              fontState.source === 'local' &&
              localAliasKey(fontState.fontId, fontState.family) === key
            ) {
              applyActiveFontFromState();
            }
            if (fontBackdropEl && !fontBackdropEl.hidden) {
              updateFontPreview();
            }
          });
        })
        .catch(function () {
          /* 本机系统字体名称 fallback 仍可用，静默降级 */
        })
        .then(function () {
          delete fontState.loadingLocalFaces[key];
        });
    }

    function persistFontPrefs() {
      writeCachedFontPrefs();
      if (!IS_TAURI || typeof Bridge.terminalSetFontPrefs !== 'function') {
        return Promise.resolve();
      }
      return Bridge.terminalSetFontPrefs({
        terminalFontSource: fontState.source,
        terminalFontFamily: fontState.family || '',
        terminalFontId: fontState.fontId || '',
        clearFont: fontState.source === 'default',
        terminalFontSize: clampFontSize(fontState.size)
      })
        .then(function (saved) {
          if (saved && typeof saved === 'object') {
            var src = String(saved.terminalFontSource || fontState.source);
            if (src === 'local' || src === 'remote' || src === 'default') {
              fontState.source = src;
            }
            fontState.family = saved.terminalFontFamily ? String(saved.terminalFontFamily) : '';
            fontState.fontId = saved.terminalFontId ? String(saved.terminalFontId) : '';
            fontState.size = clampFontSize(saved.terminalFontSize);
            writeCachedFontPrefs();
          }
          return saved;
        })
        .catch(function (err) {
          showToast('保存终端字体设置失败：' + errMessage(err, '写入配置失败'));
          throw err;
        });
    }

    function updateFontPreview() {
      if (!fontPreviewEl) return;
      var sz = clampFontSize(fontState.draftSize);
      if (fontSizeValEl) fontSizeValEl.textContent = sz + ' px';
      fontPreviewEl.style.setProperty('--tm-preview-font-size', sz + 'px');
      var cssFam = 'var(--font-mono)';
      if (fontState.draftSource === 'local' && (fontState.draftFamily || fontState.draftFontId)) {
        var locAlias = fontState.loadedAliases[localAliasKey(fontState.draftFontId, fontState.draftFamily)];
        var safeLoc = sanitizeFontFamilyCss(fontState.draftFamily);
        var parts = [];
        if (locAlias) parts.push("'" + locAlias + "'");
        if (safeLoc) parts.push("'" + safeLoc + "'");
        if (parts.length > 0) cssFam = parts.join(', ') + ', var(--font-mono)';
      } else if (fontState.draftSource === 'remote' && fontState.draftFontId) {
        var alias = fontState.loadedAliases[fontState.draftFontId];
        if (alias) cssFam = "'" + alias + "', var(--font-mono)";
        else if (fontState.draftFamily) {
          cssFam = "'" + sanitizeFontFamilyCss(fontState.draftFamily) + "', var(--font-mono)";
        }
      }
      fontPreviewEl.style.setProperty('--tm-preview-font-family', cssFam);
    }

    function renderFontList() {
      if (!fontListEl) return;
      while (fontListEl.firstChild) fontListEl.removeChild(fontListEl.firstChild);

      var tab = fontState.modalTab;
      var loading = tab === 'local' ? fontState.localLoading : fontState.remoteLoading;
      var rawList = tab === 'local' ? fontState.localFonts : fontState.remoteFonts;

      if (loading) {
        var loadingEl = document.createElement('div');
        loadingEl.className = 'tm-font-empty';
        loadingEl.textContent = tab === 'local' ? '正在扫描本机字体…' : '正在向远端请求字体列表…';
        fontListEl.appendChild(loadingEl);
        return;
      }

      if (tab === 'remote' && !isSessionReady()) {
        var discEl = document.createElement('div');
        discEl.className = 'tm-font-empty';
        discEl.textContent = '请先连接远端设备后再加载远程机器字体';
        fontListEl.appendChild(discEl);
        return;
      }

      var items = Array.isArray(rawList) ? rawList : [];
      var hasExactDraftId = Boolean(
        fontState.draftFontId &&
          items.some(function (f) {
            return f && f.id === fontState.draftFontId;
          })
      );
      var q = fontSearchEl ? fontSearchEl.value.trim().toLowerCase() : '';
      var monoOnly = fontMonoOnlyEl ? Boolean(fontMonoOnlyEl.checked) : true;
      var filtered = items.filter(function (f) {
        if (!f || !f.family) return false;
        if (monoOnly && !f.monospace) return false;
        if (q && String(f.family).toLowerCase().indexOf(q) === -1) return false;
        return true;
      });

      if (!filtered.length) {
        var emptyEl = document.createElement('div');
        emptyEl.className = 'tm-font-empty';
        emptyEl.textContent = items.length ? '无匹配的字体（可取消“仅等宽字体”或清空搜索）' : '未找到可用字体';
        fontListEl.appendChild(emptyEl);
        return;
      }

      filtered.forEach(function (item) {
        var btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'tm-font-item';
        btn.setAttribute('role', 'option');
        var selected =
          fontState.draftSource === tab &&
          ((hasExactDraftId && fontState.draftFontId === item.id) ||
            (!hasExactDraftId && fontState.draftFamily === item.family));
        btn.classList.toggle('is-selected', Boolean(selected));
        btn.setAttribute('aria-selected', selected ? 'true' : 'false');

        var nameSpan = document.createElement('span');
        nameSpan.textContent = item.family;
        btn.appendChild(nameSpan);

        var badge = document.createElement('span');
        badge.className = 'tm-font-item__badge';
        badge.textContent = item.monospace ? '等宽' : '比例';
        btn.appendChild(badge);

        btn.addEventListener('click', function () {
          fontState.draftSource = tab;
          fontState.draftFamily = String(item.family || '');
          fontState.draftFontId = String(item.id || '');
          renderFontList();
          updateFontPreview();
          setFontStatus('已选择：' + fontState.draftFamily);
          if (tab === 'local') {
            ensureLocalFontFaceLoaded(fontState.draftFontId, fontState.draftFamily);
          }
        });
        fontListEl.appendChild(btn);
      });
    }

    function loadLocalFonts(forceRefresh) {
      if (fontState.localLoading) return;
      if (Array.isArray(fontState.localFonts) && !forceRefresh) {
        renderFontList();
        return;
      }
      if (!IS_TAURI || typeof Bridge.terminalListLocalFonts !== 'function') {
        fontState.localFonts = [
          { id: 'mock-local-sf', family: 'SF Mono', monospace: true, sizeBytes: 0 },
          { id: 'mock-local-menlo', family: 'Menlo', monospace: true, sizeBytes: 0 }
        ];
        renderFontList();
        return;
      }
      fontState.localLoading = true;
      renderFontList();
      Bridge.terminalListLocalFonts()
        .then(function (list) {
          fontState.localLoading = false;
          fontState.localFonts = Array.isArray(list) ? list : [];
          if (fontState.modalTab === 'local') renderFontList();
        })
        .catch(function (err) {
          fontState.localLoading = false;
          setFontStatus('加载本机字体失败：' + String(err || '未知错误'));
          if (fontState.modalTab === 'local') renderFontList();
        });
    }

    function loadRemoteFonts(forceRefresh) {
      if (fontState.remoteLoading) return;
      if (Array.isArray(fontState.remoteFonts) && !forceRefresh) {
        renderFontList();
        return;
      }
      if (!isSessionReady()) {
        renderFontList();
        return;
      }
      if (!IS_TAURI || typeof Bridge.terminalListRemoteFonts !== 'function') {
        fontState.remoteFonts = [
          { id: 'mock-remote-jb', family: 'JetBrains Mono', monospace: true, sizeBytes: 0 },
          { id: 'mock-remote-fira', family: 'Fira Code', monospace: true, sizeBytes: 0 }
        ];
        renderFontList();
        return;
      }
      fontState.remoteLoading = true;
      renderFontList();
      Bridge.terminalListRemoteFonts().catch(function (err) {
        fontState.remoteLoading = false;
        setFontStatus('请求远程字体列表失败：' + String(err || '未知错误'));
        if (fontState.modalTab === 'remote') renderFontList();
      });
    }

    function installAndUseRemoteFont(fontId, family, size, showSuccessToast, shouldPersist) {
      var doPersist = shouldPersist !== false;
      var alias = fontState.loadedAliases[fontId];
      if (alias) {
        fontState.source = 'remote';
        fontState.fontId = fontId;
        fontState.family = family || fontState.family;
        fontState.size = clampFontSize(size);
        applyActiveFontFromState();
        var finishCached = function () {
          if (showSuccessToast) showToast('已应用远程字体：' + (fontState.family || fontId));
        };
        if (doPersist) {
          return persistFontPrefs().then(finishCached);
        }
        finishCached();
        return Promise.resolve();
      }
      if (!IS_TAURI || typeof Bridge.terminalGetRemoteFontBytes !== 'function') {
        fontState.loadedAliases[fontId] = sanitizeFontFamilyCss(family) || 'monospace';
        fontState.source = 'remote';
        fontState.fontId = fontId;
        fontState.family = family || fontState.family;
        fontState.size = clampFontSize(size);
        applyActiveFontFromState();
        return Promise.resolve();
      }
      return Bridge.terminalGetRemoteFontBytes(fontId).then(function (raw) {
        var buf = toArrayBuffer(raw);
        if (!buf || buf.byteLength === 0) {
          throw new Error('远程字体数据为空');
        }
        var targetAlias = remoteFontAlias(fontId);
        var face = new window.FontFace(targetAlias, buf);
        return face.load().then(function (loadedFace) {
          if (document.fonts && typeof document.fonts.add === 'function') {
            document.fonts.add(loadedFace);
          }
          fontState.loadedAliases[fontId] = targetAlias;
          fontState.source = 'remote';
          fontState.fontId = fontId;
          fontState.family = family || loadedFace.family || fontState.family;
          fontState.size = clampFontSize(size);
          applyActiveFontFromState();
          var finishLoaded = function () {
            if (showSuccessToast) showToast('已加载并应用远程字体：' + fontState.family);
          };
          if (doPersist) {
            return persistFontPrefs().then(finishLoaded);
          }
          finishLoaded();
        });
      });
    }

    function fetchAndApplyRemoteFont(fontId, family, size, userInitiated) {
      if (!fontId) return;
      if (fontState.loadedAliases[fontId]) {
        installAndUseRemoteFont(fontId, family, size, userInitiated, true)
          .then(function () {
            if (userInitiated && fontBackdropEl && !fontBackdropEl.hidden) {
              closeFontModal();
            }
          })
          .catch(function () {
            if (userInitiated && fontBackdropEl && !fontBackdropEl.hidden) {
              closeFontModal();
            }
          });
        return;
      }
      if (!IS_TAURI || typeof Bridge.terminalFetchRemoteFont !== 'function') {
        installAndUseRemoteFont(fontId, family, size, userInitiated, true).then(function () {
          if (userInitiated && fontBackdropEl && !fontBackdropEl.hidden) {
            closeFontModal();
          }
        });
        return;
      }
      if (!isSessionReady()) {
        if (userInitiated) setFontStatus('远控会话未就绪，无法下载远程字体');
        return;
      }
      if (userInitiated) {
        setFontStatus('正在从远程机器下载字体…');
        if (fontApplyBtn) fontApplyBtn.disabled = true;
      }
      Bridge.terminalFetchRemoteFont(fontId)
        .then(function (reqId) {
          fontState.pendingFetch = {
            reqId: Number(reqId),
            fontId: fontId,
            family: family || '',
            size: size,
            userInitiated: Boolean(userInitiated)
          };
        })
        .catch(function (err) {
          if (!userInitiated) fontState.autoRestoredRemote = false;
          if (fontApplyBtn) fontApplyBtn.disabled = false;
          if (userInitiated) setFontStatus('下载远程字体失败：' + String(err || '未知错误'));
        });
    }

    function openFontModal() {
      if (!fontBackdropEl) return;
      closeMoreMenu();
      fontState.draftSource = fontState.source;
      fontState.draftFamily = fontState.family;
      fontState.draftFontId = fontState.fontId;
      fontState.draftSize = fontState.size;
      fontState.modalTab = fontState.source === 'remote' ? 'remote' : 'local';
      if (fontTabLocalEl) {
        var isLoc = fontState.modalTab === 'local';
        fontTabLocalEl.classList.toggle('is-active', isLoc);
        fontTabLocalEl.setAttribute('aria-selected', isLoc ? 'true' : 'false');
      }
      if (fontTabRemoteEl) {
        var isRem = fontState.modalTab === 'remote';
        fontTabRemoteEl.classList.toggle('is-active', isRem);
        fontTabRemoteEl.setAttribute('aria-selected', isRem ? 'true' : 'false');
      }
      if (fontSizeInputEl) fontSizeInputEl.value = String(fontState.draftSize);
      if (fontApplyBtn) fontApplyBtn.disabled = false;
      setFontStatus('');
      updateFontPreview();
      fontBackdropEl.hidden = false;
      if (fontState.modalTab === 'local') loadLocalFonts(false);
      else loadRemoteFonts(false);
    }

    function closeFontModal() {
      if (fontBackdropEl) fontBackdropEl.hidden = true;
      if (fontApplyBtn) fontApplyBtn.disabled = false;
      setFontStatus('');
      if (viewMode === 'terminal') focusTerminal();
    }

    if (fontBtnEl) {
      fontBtnEl.addEventListener('click', function () {
        openFontModal();
      });
    }
    [fontTabLocalEl, fontTabRemoteEl].forEach(function (tabBtn) {
      if (!tabBtn) return;
      tabBtn.addEventListener('click', function () {
        var nextTab = tabBtn.getAttribute('data-font-source') === 'remote' ? 'remote' : 'local';
        fontState.modalTab = nextTab;
        if (fontTabLocalEl) {
          fontTabLocalEl.classList.toggle('is-active', nextTab === 'local');
          fontTabLocalEl.setAttribute('aria-selected', nextTab === 'local' ? 'true' : 'false');
        }
        if (fontTabRemoteEl) {
          fontTabRemoteEl.classList.toggle('is-active', nextTab === 'remote');
          fontTabRemoteEl.setAttribute('aria-selected', nextTab === 'remote' ? 'true' : 'false');
        }
        setFontStatus('');
        if (nextTab === 'local') loadLocalFonts(false);
        else loadRemoteFonts(false);
      });
    });
    if (fontSearchEl) {
      fontSearchEl.addEventListener('input', function () {
        renderFontList();
      });
    }
    if (fontMonoOnlyEl) {
      fontMonoOnlyEl.addEventListener('change', function () {
        renderFontList();
      });
    }
    if (fontRefreshBtn) {
      fontRefreshBtn.addEventListener('click', function () {
        if (fontState.modalTab === 'local') loadLocalFonts(true);
        else loadRemoteFonts(true);
      });
    }
    if (fontSizeInputEl) {
      fontSizeInputEl.addEventListener('input', function () {
        fontState.draftSize = Math.max(10, Math.min(28, Math.round(Number(fontSizeInputEl.value) || 13)));
        updateFontPreview();
      });
    }
    if (fontResetBtn) {
      fontResetBtn.addEventListener('click', function () {
        fontState.source = 'default';
        fontState.family = '';
        fontState.fontId = '';
        fontState.size = 13;
        fontState.draftSource = 'default';
        fontState.draftFamily = '';
        fontState.draftFontId = '';
        fontState.draftSize = 13;
        applyActiveFontFromState();
        closeFontModal();
        persistFontPrefs()
          .then(function () {
            showToast('已恢复默认终端字体');
          })
          .catch(function () {});
      });
    }
    if (fontCancelBtn) {
      fontCancelBtn.addEventListener('click', closeFontModal);
    }
    if (fontBackdropEl) {
      fontBackdropEl.addEventListener('click', function (e) {
        if (e.target === fontBackdropEl) closeFontModal();
      });
      document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && !fontBackdropEl.hidden) {
          e.preventDefault();
          closeFontModal();
        }
      });
    }
    if (fontApplyBtn) {
      fontApplyBtn.addEventListener('click', function () {
        var nextSize = clampFontSize(fontState.draftSize);
        if (fontState.draftSource === 'local' && fontState.draftFamily) {
          fontState.source = 'local';
          fontState.family = fontState.draftFamily;
          fontState.fontId = fontState.draftFontId;
          fontState.size = nextSize;
          applyActiveFontFromState();
          ensureLocalFontFaceLoaded(fontState.fontId, fontState.family);
          closeFontModal();
          var appliedFamily = fontState.family;
          persistFontPrefs()
            .then(function () {
              showToast('已应用本机字体：' + appliedFamily);
            })
            .catch(function () {});
          return;
        }
        if (fontState.draftSource === 'remote' && fontState.draftFontId) {
          fetchAndApplyRemoteFont(
            fontState.draftFontId,
            fontState.draftFamily,
            nextSize,
            true
          );
          return;
        }
        fontState.size = nextSize;
        applyActiveFontFromState();
        closeFontModal();
        persistFontPrefs()
          .then(function () {
            showToast('已更新终端字号：' + nextSize + 'px');
          })
          .catch(function () {});
      });
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
        newTabBtn.disabled =
          !isSessionReady() ||
          attachCreating ||
          sessions.length >= MAX_SESSIONS ||
          (terminalLockState.required && !terminalLockState.unlocked);
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
      } else if (sessionState.phase === 'reconnecting') {
        setStatus(
          'connecting',
          '自动重连中 (' +
            (sessionState.reconnectingAttempt || 1) +
            '/' +
            (sessionState.reconnectingMax || 8) +
            ')…'
        );
        if (reconnectBtn) {
          reconnectBtn.textContent = '立即重试';
          reconnectBtn.hidden = false;
        }
      } else if (sessionState.active && !sessionState.connected) {
        setStatus('connecting', '正在建立安全连接…' + devSuffix);
        if (reconnectBtn) reconnectBtn.hidden = true;
      } else {
        setStatus('error', '未连接' + devSuffix);
        var noticeBlocksReconnect = Boolean(
          noticeEl && !noticeEl.hidden && noticeReconnectBtn && noticeReconnectBtn.hidden
        );
        if (reconnectBtn) {
          reconnectBtn.textContent = isSessionReady() ? '重新附着' : '重新连接';
          reconnectBtn.hidden = sessionState.watchOnly || noticeBlocksReconnect;
        }
      }

      renderLiveTabs();
      syncTimerRunning();
    }

    function focusTerminal() {
      if (viewMode !== 'terminal' || isAnyModalOpen()) return;
      var t = mountXtermIfNeeded();
      if (t) {
        t.focus();
      }
    }

    screenEl.addEventListener('mousedown', function () {
      if (viewMode === 'terminal' && !isAnyModalOpen()) {
        window.setTimeout(focusTerminal, 0);
      }
    });

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

      hideNotice();
      setView('terminal');
      resetXtermView();
      var grid = computeAndFitGrid(false);

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

      var targetId = Number(termId);
      var existingTab = tabByTermId[String(targetId)];
      if (
        activeTermId === targetId &&
        viewMode === 'terminal' &&
        existingTab &&
        (existingTab.connected || existingTab.opening)
      ) {
        focusTerminal();
        return;
      }

      pendingAttachViewTarget = false;

      var prevId = activeTermId;
      if (prevId != null && prevId !== targetId) {
        var prevTab = tabByTermId[String(prevId)];
        if (prevTab) {
          prevTab.connected = false;
          prevTab.opening = false;
        }
        Api.detach(prevId).catch(function () {});
      }

      activeTermId = targetId;
      var tab = ensureTab(activeTermId);
      tab.opening = true;
      tab.connected = false;
      tab.disconnectNoticed = false;
      hideNotice();

      setView('terminal');
      resetXtermView();
      var grid = computeAndFitGrid(false);

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
      if (!isSessionReady()) {
        triggerSessionReconnect();
        return;
      }
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

    if (lockToggleVisBtn && lockPasswordEl) {
      lockToggleVisBtn.addEventListener('click', function () {
        var showing = lockPasswordEl.type === 'text';
        lockPasswordEl.type = showing ? 'password' : 'text';
        lockToggleVisBtn.textContent = showing ? '显示' : '隐藏';
        lockPasswordEl.focus();
      });
    }

    if (lockClearSavedBtn) {
      lockClearSavedBtn.addEventListener('click', function () {
        if (!IS_TAURI || typeof Bridge.terminalClearSavedPassword !== 'function') return;
        Bridge.terminalClearSavedPassword(terminalLockState.deviceKey || null)
          .then(function () {
            terminalLockState.hasSavedPassword = false;
            syncLockGateUi();
            showToast('已清除本机保存的设备系统密码');
          })
          .catch(function (err) {
            showToast(errMessage(err, '清除已保存密码失败'));
          });
      });
    }

    if (lockGateEl) {
      document.addEventListener('keydown', function (ev) {
        if (ev.key !== 'Tab' || !lockGateEl || lockGateEl.hidden) return;
        var candidates = [
          lockUsernameEl,
          lockPasswordEl,
          lockToggleVisBtn,
          lockRememberEl,
          lockClearSavedBtn,
          lockSubmitBtn
        ].filter(function (item) {
          return item && !item.disabled && !item.hidden && item.offsetParent !== null;
        });
        if (!candidates.length) return;
        var first = candidates[0];
        var last = candidates[candidates.length - 1];
        var cur = document.activeElement;
        if (!lockGateEl.contains(cur)) {
          ev.preventDefault();
          (ev.shiftKey ? last : first).focus();
          return;
        }
        if (ev.shiftKey && cur === first) {
          ev.preventDefault();
          last.focus();
        } else if (!ev.shiftKey && cur === last) {
          ev.preventDefault();
          first.focus();
        }
      });
    }

    if (lockFormEl) {
      lockFormEl.addEventListener('submit', function (ev) {
        ev.preventDefault();
        if (terminalLockState.submitting || terminalLockState.autoInFlight) return;
        var rem = remainingCooldownSecs();
        if (rem > 0) {
          setLockError('验证过于频繁，请 ' + rem + ' 秒后再试');
          syncLockGateUi();
          return;
        }
        var pw = lockPasswordEl ? lockPasswordEl.value : '';
        if (!pw) {
          setLockError('请输入被控端系统登录密码');
          if (lockPasswordEl) lockPasswordEl.focus();
          return;
        }
        var userArg = null;
        if (lockUserRowEl && !lockUserRowEl.hidden && lockUsernameEl) {
          var uTrim = lockUsernameEl.value.trim();
          if (!uTrim) {
            setLockError('当前处于登录界面，请输入系统用户名');
            lockUsernameEl.focus();
            return;
          }
          userArg = uTrim;
        }
        var remember = Boolean(lockRememberEl && lockRememberEl.checked);
        terminalLockState.submitting = true;
        var submitSeq = lockStateSeq;
        setLockError('');
        syncLockGateUi();
        if (!IS_TAURI || typeof Bridge.terminalUnlock !== 'function') {
          terminalLockState.submitting = false;
          terminalLockState.required = false;
          terminalLockState.unlocked = true;
          syncLockGateUi();
          return;
        }
        Bridge.terminalUnlock(pw, userArg, remember).catch(function (err) {
          if (submitSeq !== lockStateSeq) return;
          terminalLockState.submitting = false;
          setLockError(errMessage(err, '发送密码验证请求失败'));
          syncLockGateUi();
        });
      });
    }

    /* ── 远端事件与会话状态处理 ─────────────────────────────── */

    function handleTerminalEvent(ev) {
      if (!ev || !ev.type) return;

      if (ev.type === 'fontList') {
        fontState.remoteLoading = false;
        fontState.remoteFonts = Array.isArray(ev.fonts) ? ev.fonts : [];
        if (fontState.modalTab === 'remote' && fontBackdropEl && !fontBackdropEl.hidden) {
          renderFontList();
        }
        return;
      }

      if (ev.type === 'fontReady') {
        var pf = fontState.pendingFetch;
        if (pf && pf.reqId === ev.reqId) {
          fontState.pendingFetch = null;
        }
        if (!ev.ok) {
          if (!pf || !pf.userInitiated) {
            fontState.autoRestoredRemote = false;
          }
          if (fontApplyBtn) fontApplyBtn.disabled = false;
          setFontStatus('远程字体下载失败：' + String(ev.error || '未知错误'));
          if (pf && pf.userInitiated) {
            showToast('远程字体下载失败：' + String(ev.error || '未知错误'));
          }
          return;
        }
        var targetSize = pf ? pf.size : fontState.size;
        var targetFamily = ev.family || (pf ? pf.family : fontState.family);
        var userInitiated = pf ? pf.userInitiated : false;
        installAndUseRemoteFont(ev.fontId, targetFamily, targetSize, userInitiated, true)
          .then(function () {
            if (userInitiated && fontBackdropEl && !fontBackdropEl.hidden) {
              closeFontModal();
            }
          })
          .catch(function (err) {
            if (fontApplyBtn) fontApplyBtn.disabled = false;
            setFontStatus('应用远程字体失败：' + String(err || '未知错误'));
          });
        return;
      }

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
          hideNotice();
          setView('terminal');
          resetXtermView();
          computeAndFitGrid(true);
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
        resetXtermView();
        hideNotice();
        syncLiveChrome();
        computeAndFitGrid(true);
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
        if (!outTab || !outTab.connected || ev.termId !== activeTermId) return;
        var t = mountXtermIfNeeded();
        if (!t) return;
        if (Array.isArray(ev.bytes) && ev.bytes.length > 0) {
          t.write(new Uint8Array(ev.bytes));
        } else if (ev.bytes instanceof Uint8Array && ev.bytes.length > 0) {
          t.write(ev.bytes);
        } else if (ev.textUtf8Lossy) {
          t.write(ev.textUtf8Lossy);
        }
        return;
      }

      if (ev.type === 'exited') {
        var wasActive = activeTermId === ev.termId;
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

    function handleDisconnectState(reasonText, opts) {
      var options = opts || {};
      fontState.autoRestoredRemote = false;
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
          writeSystemNoticeToXterm('[' + (reasonText || '远控会话已断开') + ']');
        }
        if (options.showNotice !== false && !sessionState.watchOnly) {
          if (noticeReconnectBtn) {
            noticeReconnectBtn.textContent = options.reconnectLabel || '重新连接';
          }
          showNotice(reasonText || '远控会话已断开', true);
        }
        syncLiveChrome();
      } else {
        renderManagerView();
      }
    }

    function maybeAutoRestoreRemoteFont() {
      if (
        !fontState.autoRestoredRemote &&
        fontState.source === 'remote' &&
        fontState.fontId &&
        !fontState.loadedAliases[fontState.fontId] &&
        isSessionReady()
      ) {
        fontState.autoRestoredRemote = true;
        fetchAndApplyRemoteFont(fontState.fontId, fontState.family, fontState.size, false);
      }
    }

    function applySessionInfo(raw) {
      initialSessionLoaded = true;
      var s = raw && typeof raw === 'object' ? raw : {};
      var wasReady = isSessionReady();
      sessionState.active = Boolean(s.active);
      sessionState.connected = Boolean(s.connected);
      sessionState.watchOnly = Boolean(
        s.watchOnly !== undefined ? s.watchOnly : s.watch_only
      );
      var rawPhase = s.phase ? String(s.phase) : '';
      sessionState.phase =
        rawPhase || (sessionState.connected ? 'connected' : sessionState.active ? 'connecting' : '');
      if (s.deviceName || s.device_name || s.target) {
        sessionState.deviceName = String(s.deviceName || s.device_name || s.target);
      }
      var permsRaw = s.toolPermissions || s.tool_permissions || null;
      if (permsRaw && typeof permsRaw === 'object') {
        if (typeof permsRaw.allowTerminal === 'boolean') {
          terminalLockState.allowTerminal = permsRaw.allowTerminal;
        } else if (typeof permsRaw.allow_terminal === 'boolean') {
          terminalLockState.allowTerminal = permsRaw.allow_terminal;
        }
      }

      if (!sessionState.active) {
        resetLockGateSessionState(true);
        handleDisconnectState(lastEndedReason || '远控会话已断开', { reconnectLabel: '重新连接' });
        return;
      }

      if (sessionState.phase === 'reconnecting') {
        resetLockGateSessionState(false);
        var recMsg =
          '连接中断，正在自动重连 (' +
          (sessionState.reconnectingAttempt || 1) +
          '/' +
          (sessionState.reconnectingMax || 8) +
          ')…';
        handleDisconnectState(recMsg, { reconnectLabel: '立即重试' });
        return;
      }

      lastEndedReason = '';

      if (sessionState.watchOnly) {
        syncLockGateUi();
        handleDisconnectState('当前为只读观看模式，无法使用远程终端', { showNotice: false });
        return;
      }

      if (terminalLockState.allowTerminal === false) {
        syncLockGateUi();
        handleDisconnectState('远程终端仅限本人账号使用，当前为他人协助会话', {
          showNotice: viewMode === 'terminal'
        });
        return;
      }

      if (!sessionState.connected) {
        syncLockGateUi();
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
        return;
      }

      hideNotice();
      fetchTerminalLockState().then(function () {
        if (!wasReady && isSessionReady()) {
          resetSessionTimer();
          refreshSessionList();
          if (viewMode === 'terminal' && activeTermId != null) {
            openSessionTerminal(activeTermId);
          }
          maybeAutoRestoreRemoteFont();
        }
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
      });
    }

    function handleSessionUpdate(upd) {
      if (!upd || !upd.kind) return;
      initialSessionLoaded = true;
      var data = upd.data || {};
      if (upd.kind === 'tool_permissions_updated') {
        if (typeof data.allow_terminal === 'boolean') {
          terminalLockState.allowTerminal = data.allow_terminal;
        } else if (typeof data.allowTerminal === 'boolean') {
          terminalLockState.allowTerminal = data.allowTerminal;
        }
        if (terminalLockState.allowTerminal === false) {
          sessions = [];
          handleDisconnectState('远程终端仅限本人账号使用，当前为他人协助会话', {
            showNotice: viewMode === 'terminal'
          });
        }
        syncLockGateUi();
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
        return;
      }
      if (upd.kind === 'tool_permission_denied') {
        if (data && data.channel === 2) {
          terminalLockState.allowTerminal = false;
          syncLockGateUi();
          showToast('远程终端仅限本人账号使用');
          if (viewMode === 'list') renderManagerView();
          else syncLiveChrome();
        }
        return;
      }
      if (upd.kind === 'terminal_lock_state') {
        var prevUnlocked = terminalLockState.unlocked;
        lockStateSeq += 1;
        terminalLockState.pending = false;
        terminalLockState.required = Boolean(data.required);
        terminalLockState.unlocked = Boolean(data.unlocked);
        terminalLockState.username = data.username ? String(data.username) : '';
        var retryLockSecs = Number(data.retry_after_secs || 0);
        if (retryLockSecs > 0) setLockCooldown(retryLockSecs);
        if (!terminalLockState.required || terminalLockState.unlocked) {
          terminalLockState.submitting = false;
          terminalLockState.autoInFlight = false;
          setLockError('');
          if (lockPasswordEl) lockPasswordEl.value = '';
        }
        syncLockGateUi();
        if (!terminalLockState.required || terminalLockState.unlocked) {
          if (!prevUnlocked && isSessionReady()) {
            refreshSessionList();
            if (viewMode === 'terminal' && activeTermId != null) {
              var curTab = getActiveTab();
              if (!curTab || (!curTab.connected && !curTab.opening)) {
                openSessionTerminal(activeTermId);
              }
            }
            maybeAutoRestoreRemoteFont();
          }
        } else {
          Object.keys(tabByTermId).forEach(function (k) {
            if (tabByTermId[k]) tabByTermId[k].opening = false;
          });
          fetchTerminalLockState();
        }
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
        return;
      }
      if (upd.kind === 'terminal_unlock_result') {
        var wasAuto = terminalLockState.autoInFlight;
        lockStateSeq += 1;
        terminalLockState.submitting = false;
        terminalLockState.autoInFlight = false;
        var resRetrySecs = Number(data.retry_after_secs || 0);
        if (resRetrySecs > 0) setLockCooldown(resRetrySecs);
        if (data.ok) {
          terminalLockState.pending = false;
          terminalLockState.required = false;
          terminalLockState.unlocked = true;
          terminalLockState.autoAttempted = false;
          if (lockPasswordEl) lockPasswordEl.value = '';
          setLockError('');
          syncLockGateUi();
          showToast('系统密码验证通过');
          fetchTerminalLockState();
          refreshSessionList();
          maybeAutoRestoreRemoteFont();
          if (viewMode === 'terminal' && activeTermId != null) {
            var activeTab = getActiveTab();
            if (!activeTab || (!activeTab.connected && !activeTab.opening)) {
              openSessionTerminal(activeTermId);
            } else {
              focusTerminal();
            }
          }
        } else {
          if (wasAuto) {
            terminalLockState.hasSavedPassword = false;
            setLockError('已保存的系统密码已失效，请重新输入');
          } else {
            setLockError(data.error ? String(data.error) : '系统密码错误，请重试');
          }
          syncLockGateUi();
          if (lockPasswordEl) {
            lockPasswordEl.focus();
            lockPasswordEl.select();
          }
        }
        return;
      }
      if (upd.kind === 'connecting') {
        sessionState.active = true;
        sessionState.connected = false;
        sessionState.phase = 'connecting';
        lastEndedReason = '';
        if (data.device_name || data.device) {
          sessionState.deviceName = String(data.device_name || data.device);
        }
        if (viewMode === 'list') renderManagerView();
        else syncLiveChrome();
        return;
      }
      if (upd.kind === 'reconnecting') {
        sessionState.active = true;
        sessionState.connected = false;
        sessionState.phase = 'reconnecting';
        resetLockGateSessionState(false);
        sessionState.reconnectingAttempt = Number(data.attempt) || 1;
        sessionState.reconnectingMax = Number(data.max_attempts || data.maxAttempts) || 8;
        sessionState.reconnectingReason = data.reason ? String(data.reason) : '';
        var recMsg =
          '连接中断，正在自动重连 (' +
          sessionState.reconnectingAttempt +
          '/' +
          sessionState.reconnectingMax +
          ')…';
        handleDisconnectState(recMsg, { reconnectLabel: '立即重试' });
        return;
      }
      if (upd.kind === 'ended') {
        var reason = (data && data.reason) || '远控会话已断开';
        lastEndedReason = reason;
        sessionState.active = false;
        sessionState.connected = false;
        sessionState.watchOnly = false;
        sessionState.phase = 'ended';
        resetLockGateSessionState(true);
        handleDisconnectState(reason, { reconnectLabel: '重新连接' });
        return;
      }
      if (upd.kind === 'negotiated') {
        var wasReady = isSessionReady();
        lastEndedReason = '';
        sessionState.active = true;
        sessionState.connected = true;
        sessionState.phase = 'connected';
        if (typeof data.watch_only === 'boolean') {
          sessionState.watchOnly = data.watch_only;
        }
        if (data.device_name || data.device) {
          sessionState.deviceName = String(data.device_name || data.device);
        }
        if (sessionState.watchOnly) {
          syncLockGateUi();
          handleDisconnectState('当前为只读观看模式，无法使用远程终端', { showNotice: false });
          return;
        }
        hideNotice();
        fetchTerminalLockState().then(function () {
          if (!wasReady && isSessionReady()) {
            resetSessionTimer();
            refreshSessionList();
            if (viewMode === 'terminal' && activeTermId != null) {
              openSessionTerminal(activeTermId);
            }
            maybeAutoRestoreRemoteFont();
          }
          if (viewMode === 'list') renderManagerView();
          else syncLiveChrome();
        });
      }
    }

    /* ── ResizeObserver（100ms 防抖） ───────────────────────── */

    function scheduleFitResize() {
      if (viewMode !== 'terminal') return;
      window.clearTimeout(resizeTimer);
      resizeTimer = window.setTimeout(function () {
        computeAndFitGrid(false);
      }, RESIZE_DEBOUNCE_MS);
    }

    if (typeof window.ResizeObserver === 'function') {
      var ro = new window.ResizeObserver(scheduleFitResize);
      ro.observe(screenEl);
    } else {
      window.addEventListener('resize', scheduleFitResize);
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

    if (typeof Bridge.getAppPrefs === 'function') {
      Bridge.getAppPrefs()
        .then(function (prefs) {
          if (!prefs || typeof prefs !== 'object') return;
          var src = String(prefs.terminalFontSource || 'default');
          fontState.source =
            src === 'local' || src === 'remote' || src === 'default' ? src : 'default';
          fontState.family =
            fontState.source !== 'default' && prefs.terminalFontFamily
              ? String(prefs.terminalFontFamily)
              : '';
          fontState.fontId =
            fontState.source !== 'default' && prefs.terminalFontId
              ? String(prefs.terminalFontId)
              : '';
          var sz = Number(prefs.terminalFontSize);
          fontState.size = isFinite(sz) && sz >= 10 && sz <= 28 ? Math.round(sz) : 13;
          writeCachedFontPrefs();
          applyActiveFontFromState();
          if (fontState.source === 'local' && (fontState.fontId || fontState.family)) {
            ensureLocalFontFaceLoaded(fontState.fontId, fontState.family);
          } else if (fontState.source === 'remote' && fontState.fontId) {
            if (!fontState.loadedAliases[fontState.fontId]) {
              installAndUseRemoteFont(
                fontState.fontId,
                fontState.family,
                fontState.size,
                false,
                false
              ).catch(function () {
                maybeAutoRestoreRemoteFont();
              });
            }
          }
        })
        .catch(function () {});
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
            initialSessionLoaded = true;
            renderManagerView();
          });
      } else {
        initialSessionLoaded = true;
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
