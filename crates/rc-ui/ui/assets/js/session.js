/* session.js —— W3 终端窗口 + S1 会话窗口 / S2 安全状态 / S3 控制中心 / S4 HUD
 *
 * 唯一依据：docs/reference/design/08-ui-design.md
 *   §6 W3 终端：prompt 两枚 12px 灰描边标签 + ~（ACCENT）+ >（ONLINE）+ 10×18
 *      ACCENT 光标块（800ms 闪烁）；右上 TERMINAL_MUTED mono 14px 计时，
 *      注入时钟，从 00:00:00 起每秒 +1
 *   §6 S1：计时从 00:00:00 起（注入时钟）；观看模式变体 = 只读态 + 左上灰 Pill
 *   §6 S2：需关注判定（防窥=关闭 或 自动锁屏=关）；说明文字随状态切换
 *   §6 S3：控制中心 NativeMenu；显示屏分辨率（含「其他分辨率」二级菜单）、画质两单选组（默认 超清 14M / 144 帧）、
 *      声音/安全（观看模式/立即锁屏/退出时锁屏）/外设（macOS 组合键）/窗口/工具/更多；进入全屏幕；退出远控回主窗口 P1
 *   §6 S4：HUD 首行随计时同步，其余 7 行静态 mock
 *   §7.3 快捷键：⌥⌘N 切换下一个屏幕、F 进入/退出全屏幕、Esc 关闭当前浮层
 *   §7.4 浮层：点锚点开合、点外部关闭、同时只允许一个
 *   §8.6.2 时钟注入：本文件的时间一律由可注入的时钟源提供（一期逻辑时钟）
 *
 * 同一文件服务两个页面：
 *   terminal.html —— W3（计时 only）
 *   session.html  —— S1 + S2 + S3 + S4
 */
(function () {
  'use strict';

  /* ── §8.6.2 时钟注入 ─────────────────────────────────────────────
   * 时间只从 clock.nowMs() 取；后续接线 Rust 侧会话时间时替换此函数即可。
   * 暴露 window.RCSession.setClock(fn) 作为注入点（返回毫秒）。
   */
  var clock = {
    nowMs: function () {
      return Date.now();
    }
  };

  function pad2(n) {
    return n < 10 ? '0' + n : String(n);
  }

  /* HH:MM:SS（<24h；与 T21 契约 HudSnapshot.elapsed 同格式） */
  function formatHms(totalSeconds) {
    var s = Math.max(0, Math.floor(totalSeconds));
    var h = Math.floor(s / 3600);
    var m = Math.floor((s % 3600) / 60);
    var sec = s % 60;
    return pad2(h) + ':' + pad2(m) + ':' + pad2(sec);
  }

  /* 统一走时器：250ms 轮询 + 仅在秒变化时写 DOM（避免每帧 reflow） */
  function startTicker(onSecond) {
    var startMs = clock.nowMs();
    var lastShown = -1;

    function tick() {
      var elapsed = Math.floor((clock.nowMs() - startMs) / 1000);
      if (elapsed === lastShown) return;
      lastShown = elapsed;
      onSecond(elapsed, formatHms(elapsed));
    }

    tick();
    var timerId = window.setInterval(tick, 250);
    return {
      stop: function () {
        window.clearInterval(timerId);
      }
    };
  }

  /* ── Toast（§4 / §3.6：HUD_BG 底白字 13px、高 36、距底 24 居中、1.8s 单条）─ */
  var toastEl = null;
  var toastTimer = 0;

  function toast(text) {
    if (!toastEl) {
      toastEl = document.getElementById('session-toast');
      if (!toastEl) return;
    }
    toastEl.textContent = text;
    toastEl.classList.remove('is-visible');
    void toastEl.offsetWidth; /* 重启过渡：同一时刻仅一条，新的顶替旧的 */
    toastEl.classList.add('is-visible');
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(function () {
      toastEl.classList.remove('is-visible');
    }, 1800);
  }

  /* ════════════════════════════════════════════════════════════════
   * W3 终端（terminal.html）
   * ════════════════════════════════════════════════════════════════ */
  function initTerminal() {
    var timerEl = document.getElementById('terminal-timer');
    if (!timerEl) return;
    startTicker(function (elapsed, text) {
      timerEl.textContent = text;
    });
  }

  /* ════════════════════════════════════════════════════════════════
   * S1–S4 会话窗口（session.html）
   * ════════════════════════════════════════════════════════════════ */

  /* §6.0 mock（逐字取自设计文档，禁止自编）：
   *   S1 会话：设备=yii's MacBook Pro；屏幕 tab=显示屏1 (Built-in Retina Display)
   *   S4 HUD 静态 7 行：UDP LAN / 2 fps / 227.2 Kbps / 110 ms / 20 ms frm. / 0.0% loss / 2K
   * 计时器与 HUD 首行同源（从 00:00:00 起每秒 +1）。
   */
  var MOCK = {
    screenName: '显示屏1 (Built-in Retina Display)'
  };

  function initSession() {
    var win = document.getElementById('session-window');
    var stage = document.getElementById('session-stage');
    if (!win || !stage) return;

    /* Item #3：全局捕获阶段无条件拦截 contextmenu，彻底消除 WKWebView 原生「重新加载」菜单；
     * 不调用 stopPropagation，以确保 #screen-tab 自定义右键分辨率菜单正常触发。 */
    document.addEventListener(
      'contextmenu',
      function (e) {
        e.preventDefault();
      },
      true
    );

    /* 在舞台区提前阻断鼠标右键默认行为，防止 macOS WebKit 启动原生右键跟踪 */
    stage.addEventListener('mousedown', function (e) {
      if (e.button === 2) {
        e.preventDefault();
      }
    });
    stage.addEventListener('pointerdown', function (e) {
      if (e.button === 2) {
        e.preventDefault();
      }
    });

    var timerEl = document.getElementById('session-timer');
    var hudElapsedEl = document.getElementById('hud-elapsed');
    var hudEl = document.getElementById('session-hud');
    var shieldBtn = document.getElementById('btn-shield');
    var ctrlBtn = document.getElementById('btn-control-center');
    var screenTabEl = document.getElementById('screen-tab');
    var screenTabsEl = document.getElementById('screen-tabs');
    var addScreenBtn = document.getElementById('btn-add-screen');
    var nextScreenMenuBtn = document.getElementById('menu-next-screen');
    var screenResMenuEl = document.getElementById('screen-res-menu');
    var screenResListEl = document.getElementById('screen-res-list');
    var tabFitWindowBtn = document.getElementById('btn-tab-fit-window');
    var securityEl = document.getElementById('security-popover');
    var menuEl = document.getElementById('control-menu');
    var screenEl = document.getElementById('screen-menu');
    var screenItem = document.getElementById('menu-screen');
    var screenMainResListEl = document.getElementById('screen-main-res-list');
    var screenMoreEl = document.getElementById('screen-more-menu');
    var screenMoreResListEl = document.getElementById('screen-more-res-list');
    var qualityEl = document.getElementById('quality-menu');
    var qualityItem = document.getElementById('menu-quality');
    var codecHevcBtn = document.getElementById('opt-codec-hevc');
    var codecH264Btn = document.getElementById('opt-codec-h264');
    var qualityAdvisoryHintEl = document.getElementById('quality-advisory-hint');
    var screenAdvisoryHintEl = document.getElementById('screen-advisory-hint');
    var screenResAdvisoryHintEl = document.getElementById('screen-res-advisory-hint');
    var windowEl = document.getElementById('window-menu');
    var windowItem = document.getElementById('menu-window');
    var soundEl = document.getElementById('sound-menu');
    var soundItem = document.getElementById('menu-sound');
    var soundPlayBtn = document.getElementById('opt-sound-play');
    var securitySubEl = document.getElementById('security-menu');
    var securityItem = document.getElementById('menu-security');
    var watchOnlyMenuBtn = document.getElementById('opt-watch-only');
    var lockNowBtn = document.getElementById('btn-lock-remote-now');
    var autoLockExitBtn = document.getElementById('opt-auto-lock-exit');
    var hostMuteBtn = document.getElementById('opt-host-mute');
    var hostRestoreAudioBtn = document.getElementById('opt-host-restore-audio');
    var privacyModeMenuItem = document.getElementById('menu-privacy-mode');
    var privacyModeValEl = document.getElementById('menu-privacy-mode-val');
    var privacyModeMenuEl = document.getElementById('privacy-mode-menu');
    var openPrivacyModalBtn = document.getElementById('btn-open-privacy-modal');
    var privacyModalBackdrop = document.getElementById('privacy-modal-backdrop');
    var privacyModalCloseBtn = document.getElementById('btn-privacy-modal-close');
    var privacyUploadBtn = document.getElementById('btn-privacy-upload');
    var privacyFileInput = document.getElementById('privacy-file-input');
    var privacyCustomCard = document.getElementById('privacy-custom-card');
    var privacyCustomImg = document.getElementById('privacy-custom-img');
    var privacyCustomEmpty = document.getElementById('privacy-custom-empty');
    var privacyLastUsedBadge = document.getElementById('privacy-last-used-badge');
    var privacyPresetsGrid = document.getElementById('privacy-presets-grid');
    var privacyPreviewBtn = document.getElementById('btn-privacy-preview');
    var privacyCancelBtn = document.getElementById('btn-privacy-cancel');
    var privacyConfirmBtn = document.getElementById('btn-privacy-confirm');
    var privacyPreviewOverlay = document.getElementById('privacy-preview-overlay');
    var privacyPreviewSurface = document.getElementById('privacy-preview-surface');
    var privacyPreviewImg = document.getElementById('privacy-preview-img');
    var privacyPreviewCaption = document.getElementById('privacy-preview-caption');
    var privacyPreviewCloseBtn = document.getElementById('btn-privacy-preview-close');
    var optSendSysShortcutsBtn = document.getElementById('opt-send-sys-shortcuts');
    var btnConfigShortcuts = document.getElementById('btn-config-shortcuts');
    var menuMouseModeItem = document.getElementById('menu-mouse-mode');
    var menuMouseModeValEl = document.getElementById('menu-mouse-mode-val');
    var mouseModeMenuEl = document.getElementById('mouse-mode-menu');
    var shortcutsModalBackdrop = document.getElementById('shortcuts-modal-backdrop');
    var shortcutsModalCloseBtn = document.getElementById('btn-shortcuts-modal-close');
    var shortcutsListEl = document.getElementById('shortcuts-list');
    var shortcutsResetBtn = document.getElementById('btn-shortcuts-reset');
    var shortcutsCancelBtn = document.getElementById('btn-shortcuts-cancel');
    var shortcutsSaveBtn = document.getElementById('btn-shortcuts-save');
    var mtlsSwitchEl = document.getElementById('opt-mtls');
    var secTransportModeEl = document.getElementById('sec-transport-mode');
    var secMtlsStatusEl = document.getElementById('sec-mtls-status');
    var secPeerFpEl = document.getElementById('sec-peer-fp');
    var linkSecurityState = {
      encryptConnection: true,
      transport: 'UDP LAN',
      peerFingerprint: ''
    };
    var peripheralsEl = document.getElementById('peripherals-menu');
    var peripheralsItem = document.getElementById('menu-peripherals');
    var fitWindowBtn = document.getElementById('btn-fit-window');
    var windowFullscreenBtn = document.getElementById('btn-window-fullscreen');
    var toolsEl = document.getElementById('tools-menu');
    var toolsItem = document.getElementById('menu-tools');
    var moreEl = document.getElementById('more-menu');
    var moreItem = document.getElementById('menu-more');
    var showHudBtn = document.getElementById('opt-show-hud');
    var btnRemoteUpdate = document.getElementById('btn-remote-update');
    var menuRemoteUpdateSep = document.getElementById('menu-remote-update-sep');
    var btnMenuRemoteUpdate = document.getElementById('btn-menu-remote-update');
    var menuRemoteUpdateLabel = document.getElementById('menu-remote-update-label');
    var remoteUpdateModalBackdrop = document.getElementById('remote-update-modal-backdrop');
    var remoteUpdateCloseBtn = document.getElementById('btn-remote-update-close');
    var remoteUpdateVerPill = document.getElementById('remote-update-ver-pill');
    var remoteUpdateArch = document.getElementById('remote-update-arch');
    var remoteUpdateDesc = document.getElementById('remote-update-desc');
    var remoteUpdateNotes = document.getElementById('remote-update-notes');
    var remoteUpdateProgress = document.getElementById('remote-update-progress');
    var remoteUpdateBarFill = document.getElementById('remote-update-bar-fill');
    var remoteUpdateStage = document.getElementById('remote-update-stage');
    var remoteUpdateBytes = document.getElementById('remote-update-bytes');
    var remoteUpdateError = document.getElementById('remote-update-error');
    var remoteUpdateCancelBtn = document.getElementById('btn-remote-update-cancel');
    var remoteUpdateConfirmBtn = document.getElementById('btn-remote-update-confirm');
    var remoteUpdateState = {
      hostVersion: null,
      latestRelease: null,
      inProgress: false,
      stage: 'idle',
      targetVersion: '',
      lastError: ''
    };
    var relayEl = document.getElementById('opt-relay');
    var clipboardItem = document.getElementById('menu-clipboard');
    var clipboardStateEl = document.getElementById('menu-clipboard-state');
    var clipboardEnabled = true;

    function syncClipboardMenuUi() {
      if (clipboardItem) {
        clipboardItem.setAttribute('aria-checked', clipboardEnabled ? 'true' : 'false');
      }
      if (clipboardStateEl) {
        clipboardStateEl.textContent = clipboardEnabled ? '已开启' : '已关闭';
      }
    }

    var BITRATE_BPS = {
      auto: 14000000,
      '150m': 150000000,
      '100m': 100000000,
      '60m': 60000000,
      '30m': 30000000,
      '14m': 14000000,
      '8m': 8000000
    };

    function syncBitrateMenuUi(presetOrBps) {
      if (!qualityEl || presetOrBps == null || presetOrBps === '') return;
      var bitrateOpts = Array.prototype.slice.call(
        qualityEl.querySelectorAll('.sn-submenu__item[data-group="bitrate"]')
      );
      if (!bitrateOpts.length) return;
      var activeOpt = null;
      if (typeof presetOrBps === 'string') {
        var key = presetOrBps.trim().toLowerCase();
        bitrateOpts.forEach(function (opt) {
          if (opt.getAttribute('data-value') === key) {
            activeOpt = opt;
          }
        });
      }
      if (!activeOpt) {
        var bps = Number(presetOrBps);
        if (!isFinite(bps) || bps <= 0) return;
        var closestDiff = Infinity;
        bitrateOpts.forEach(function (opt) {
          var val = opt.getAttribute('data-value');
          if (val === 'auto') return;
          var optBps = BITRATE_BPS[val];
          if (!isFinite(optBps)) return;
          var diff = Math.abs(bps - optBps);
          if (diff < closestDiff) {
            closestDiff = diff;
            activeOpt = opt;
          }
        });
      }
      if (!activeOpt) return;
      bitrateOpts.forEach(function (opt) {
        var checked = opt === activeOpt;
        opt.setAttribute('aria-checked', checked ? 'true' : 'false');
        var check = opt.querySelector('.sn-check');
        if (check) check.hidden = !checked;
      });
    }

    function syncFpsMenuUi(fps) {
      if (!qualityEl) return;
      var target = Number(fps);
      if (!isFinite(target) || target <= 0) return;
      var fpsOpts = Array.prototype.slice.call(
        qualityEl.querySelectorAll('.sn-submenu__item[data-group="fps"]')
      );
      if (!fpsOpts.length) return;
      var matchedOpt = null;
      var closestOpt = null;
      var closestDiff = Infinity;
      fpsOpts.forEach(function (opt) {
        var v = Number(opt.getAttribute('data-value'));
        if (!isFinite(v)) return;
        if (Math.round(target) === v) {
          matchedOpt = opt;
        }
        var diff = Math.abs(target - v);
        if (diff < closestDiff) {
          closestDiff = diff;
          closestOpt = opt;
        }
      });
      var activeOpt = matchedOpt || closestOpt;
      fpsOpts.forEach(function (opt) {
        var checked = opt === activeOpt;
        opt.setAttribute('aria-checked', checked ? 'true' : 'false');
        var check = opt.querySelector('.sn-check');
        if (check) check.hidden = !checked;
      });
    }

    /* 屏幕 tab 文案以 §6.0 为准（HTML 已内联，保持一致） */
    var screenNameEl = document.getElementById('screen-name');
    if (screenNameEl) {
      screenNameEl.textContent = MOCK.screenName;
      screenNameEl.parentNode.setAttribute('title', MOCK.screenName + ' — 右键切换分辨率');
    }

    var params = new URLSearchParams(window.location.search);

    /* 窗口标题 = 会话设备名（主窗口经 ?device= 传入；缺省取 §6.0 的 yii's MacBook Pro） */
    var deviceParam = params.get('device');
    if (deviceParam) document.title = deviceParam + ' — RabbitControl';

    /* 观看模式变体（§8.3 / U-M7：支持 URL 初始参数与 SessionUpdate 动态同步） */
    var hash = (window.location.hash || '').replace('#', '');
    var watchMode = params.get('mode') === 'watch' || params.get('watch') === '1' || hash === 'watch';
    var watchPill = document.getElementById('watch-pill');
    function setWatchMode(enabled) {
      watchMode = Boolean(enabled);
      if (watchMode) {
        win.setAttribute('data-watch', '');
        if (watchPill) watchPill.hidden = false;
      } else {
        win.removeAttribute('data-watch');
        if (watchPill) watchPill.hidden = true;
      }
      if (watchOnlyMenuBtn) {
        watchOnlyMenuBtn.setAttribute('aria-checked', watchMode ? 'true' : 'false');
        var wc = watchOnlyMenuBtn.querySelector('.sn-check');
        if (wc) wc.hidden = !watchMode;
      }
      if (lockNowBtn) {
        lockNowBtn.setAttribute('aria-disabled', watchMode ? 'true' : 'false');
      }
      if (peripheralsEl) {
        Array.prototype.forEach.call(
          peripheralsEl.querySelectorAll('[data-combo]'),
          function (btn) {
            btn.setAttribute('aria-disabled', watchMode ? 'true' : 'false');
          }
        );
      }
    }
    setWatchMode(watchMode);

    /* ── 计时器 + HUD 首行（同一注入时钟，§6 S1 / §6 S4）──────────── */
    var Bridge = window.RCBridge;
    var IS_TAURI = Boolean(Bridge && Bridge.isTauri);
    var stageHintEl = stage.querySelector('.sn-stage__hint');
    var hudRows = hudEl ? Array.prototype.slice.call(hudEl.querySelectorAll('span')) : [];
    var realSessionActive = false;
    var relaySwitching = false;
    var preferredResolution = null;
    var autoAppliedSavedRes = false;
    var hasDisplayConfig = false;
    var pendingUserResTarget = null;
    var prevDisplayWidth = 0;
    var prevDisplayHeight = 0;
    var seenNegotiated = false;

    function formatTransportBadge(raw) {
      var text = String(raw || '').trim();
      if (!text) return 'UDP LAN';
      if (text === 'TCP Relay' || text === 'Relay') return 'mTLS Relay';
      return text;
    }

    function formatFingerprintAbbr(fp) {
      var clean = String(fp || '').trim();
      if (!clean) return '--';
      if (clean.length <= 14) return clean;
      return clean.slice(0, 8) + '…' + clean.slice(-6);
    }

    function syncLinkSecurityUi() {
      var on = Boolean(linkSecurityState.encryptConnection);
      if (mtlsSwitchEl) mtlsSwitchEl.checked = on;
      if (secTransportModeEl) {
        secTransportModeEl.textContent = formatTransportBadge(linkSecurityState.transport);
      }
      if (secMtlsStatusEl) {
        secMtlsStatusEl.textContent = on
          ? 'mTLS 1.3 双向证书已验证'
          : '未开启端到端 mTLS（中转外层仍受保护）';
        secMtlsStatusEl.classList.toggle('is-off', !on);
      }
      if (secPeerFpEl) {
        secPeerFpEl.textContent = on
          ? '对端证书指纹：' + formatFingerprintAbbr(linkSecurityState.peerFingerprint)
          : '对端证书指纹：未验证';
      }
    }

    function syncRelaySwitchUi(transport) {
      if (typeof transport === 'string' && transport) {
        linkSecurityState.transport = transport;
        syncLinkSecurityUi();
      }
      if (relaySwitching || !relayEl) return;
      if (typeof transport !== 'string' || !transport) return;
      relayEl.checked = transport === 'TCP Relay';
    }

    function applyHudVisibility(show, persist) {
      var visible = Boolean(show);
      if (hudEl) {
        hudEl.hidden = !visible;
      }
      if (showHudBtn) {
        showHudBtn.setAttribute('aria-checked', visible ? 'true' : 'false');
        var check = showHudBtn.querySelector('.sn-check');
        if (check) check.hidden = !visible;
      }
      if (persist && IS_TAURI && typeof Bridge.saveStreamPrefs === 'function') {
        Bridge.saveStreamPrefs({ showHud: visible }).catch(function () {});
      }
    }
    var localTicker = startTicker(function (elapsed, text) {
      if (timerEl) timerEl.textContent = text;
      if (hudElapsedEl) hudElapsedEl.textContent = text;
    });

    function stopLocalTicker() {
      if (localTicker) {
        localTicker.stop();
        localTicker = null;
      }
    }

    function formatHudBitrate(kbps) {
      if (!isFinite(kbps) || kbps < 0) return '0.0 Kbps';
      if (kbps >= 1000) return (kbps / 1000).toFixed(1) + ' Mbps';
      return kbps.toFixed(1) + ' Kbps';
    }

    function applyHudSnapshot(snap) {
      if (!snap) return;
      if (snap.elapsed) {
        if (timerEl) timerEl.textContent = snap.elapsed;
        if (hudElapsedEl) hudElapsedEl.textContent = snap.elapsed;
      }
      if (snap.transport != null) {
        linkSecurityState.transport = String(snap.transport);
        syncLinkSecurityUi();
      }
      if (hudRows.length >= 8) {
        if (snap.transport != null) hudRows[1].textContent = String(snap.transport);
        hudRows[2].textContent = (isFinite(snap.fps) ? Math.round(snap.fps) : 0) + ' fps';
        hudRows[3].textContent = formatHudBitrate(snap.bitrate_kbps);
        hudRows[4].textContent =
          snap.rtt_ms != null && isFinite(snap.rtt_ms) ? snap.rtt_ms.toFixed(1) + ' ms' : '-- ms';
        hudRows[5].textContent =
          snap.frame_ms != null && isFinite(snap.frame_ms)
            ? snap.frame_ms.toFixed(1) + ' ms frm.'
            : '-- ms frm.';
        hudRows[6].textContent =
          (isFinite(snap.loss_pct) ? snap.loss_pct.toFixed(1) : '0.0') + '% loss';
        if (snap.resolution) hudRows[7].textContent = formatHudResolution(String(snap.resolution));
      }
    }

    /* ── 流参数与分辨率预设状态（Item #4 / #5 + 高刷甜点/HEVC 引导）── */
    var SVG_NS = 'http://www.w3.org/2000/svg';
    var HIGH_REFRESH_FPS_THRESHOLD = 90;
    var HEVC_HIGH_REFRESH_MAX_PIXEL_RATE = 600000000;
    var H264_HIGH_REFRESH_MAX_PIXEL_RATE = 280000000;
    var DEFAULT_MOCK_PRESETS = [
      { width: 3024, height: 1964, label: '3024 × 1964', is_native: false, is_hidpi: false },
      { width: 2560, height: 1600, label: '2560 × 1600', is_native: false, is_hidpi: false },
      { width: 1920, height: 1200, label: '1920 × 1200', is_native: false, is_hidpi: false },
      { width: 1512, height: 982, label: '1512 × 982 (HiDPI)', is_native: true, is_hidpi: true },
      { width: 1386, height: 900, label: '1386 × 900', is_native: false, is_hidpi: false },
      { width: 1110, height: 720, label: '1110 × 720', is_native: false, is_hidpi: false },
      { width: 832, height: 540, label: '832 × 540', is_native: false, is_hidpi: false }
    ];
    var streamState = {
      width: IS_TAURI ? 0 : 1512,
      height: IS_TAURI ? 0 : 982,
      fps: 144,
      nativeWidth: IS_TAURI ? 0 : 1512,
      nativeHeight: IS_TAURI ? 0 : 982,
      maxFps: 144,
      codec: IS_TAURI ? '' : 'hevc',
      supportedCodecs: IS_TAURI ? [] : ['hevc', 'h264'],
      clientSupportsHevc: !IS_TAURI,
      advisory: null,
      hasServerPresets: false,
      presets: DEFAULT_MOCK_PRESETS.slice()
    };
    var pendingUserCodecTarget = null;
    var userPinnedCodec = null;

    function normalizeCodec(raw) {
      if (typeof raw !== 'string') return '';
      var s = raw.trim().toLowerCase();
      if (s === 'hevc' || s === 'h265') return 'hevc';
      if (s === 'h264' || s === 'avc') return 'h264';
      return '';
    }

    function effectiveCodec() {
      return normalizeCodec(streamState.codec) || 'h264';
    }

    function hostSupportsHevc() {
      if (normalizeCodec(streamState.codec) === 'hevc') return true;
      return (
        Array.isArray(streamState.supportedCodecs) &&
        streamState.supportedCodecs.some(function (c) {
          return normalizeCodec(c) === 'hevc';
        })
      );
    }

    function canUseHevc() {
      return hostSupportsHevc() && Boolean(streamState.clientSupportsHevc);
    }

    function codecMaxPixelRate(codec) {
      return normalizeCodec(codec) === 'hevc'
        ? HEVC_HIGH_REFRESH_MAX_PIXEL_RATE
        : H264_HIGH_REFRESH_MAX_PIXEL_RATE;
    }

    function isWithinHighRefreshSweetSpot(w, h, fps, codec) {
      var wn = Number(w) || 0;
      var hn = Number(h) || 0;
      var fn = Number(fps) || 0;
      if (fn < HIGH_REFRESH_FPS_THRESHOLD || wn <= 0 || hn <= 0) return true;
      return wn * hn * fn <= codecMaxPixelRate(codec);
    }

    function selectRecommendedPreset(presets, fps, codec) {
      if (!Array.isArray(presets) || !presets.length) return null;
      var best = null;
      var bestPixels = -1;
      var minPreset = null;
      var minPixels = Infinity;
      presets.forEach(function (p) {
        var pw = Number(p && p.width) || 0;
        var ph = Number(p && p.height) || 0;
        if (pw <= 0 || ph <= 0) return;
        var px = pw * ph;
        var pNative = p.is_native ? 1 : 0;
        var pHidpi = p.is_hidpi ? 1 : 0;
        if (
          !minPreset ||
          px < minPixels ||
          (px === minPixels &&
            (pNative > (minPreset.is_native ? 1 : 0) ||
              (pNative === (minPreset.is_native ? 1 : 0) &&
                pHidpi > (minPreset.is_hidpi ? 1 : 0))))
        ) {
          minPreset = p;
          minPixels = px;
        }
        if (!isWithinHighRefreshSweetSpot(pw, ph, fps, codec)) return;
        var bNative = best && best.is_native ? 1 : 0;
        var bHidpi = best && best.is_hidpi ? 1 : 0;
        if (
          !best ||
          px > bestPixels ||
          (px === bestPixels &&
            (pNative > bNative || (pNative === bNative && pHidpi >= bHidpi)))
        ) {
          best = p;
          bestPixels = px;
        }
      });
      if (best) return { width: Number(best.width), height: Number(best.height) };
      if (minPreset) return { width: Number(minPreset.width), height: Number(minPreset.height) };
      return null;
    }

    function evaluateHighRefreshAdvisoryLocal(w, h, fps, codec) {
      var wn = Number(w) || 0;
      var hn = Number(h) || 0;
      var fn = Number(fps) || 0;
      var c = normalizeCodec(codec) || 'h264';
      if (fn < HIGH_REFRESH_FPS_THRESHOLD || wn <= 0 || hn <= 0) {
        return { level: 'optimal', reason: 'optimal', can_enable_hevc: false };
      }
      var rate = wn * hn * fn;
      var presets = Array.isArray(streamState.presets) ? streamState.presets : [];
      if (c === 'hevc') {
        if (rate <= HEVC_HIGH_REFRESH_MAX_PIXEL_RATE) {
          return { level: 'optimal', reason: 'optimal', can_enable_hevc: false };
        }
        var recHevc = selectRecommendedPreset(presets, fn, 'hevc');
        return {
          level: 'above_sweet_spot',
          reason: 'hevc_over_budget',
          can_enable_hevc: false,
          recommended_width: recHevc ? recHevc.width : null,
          recommended_height: recHevc ? recHevc.height : null
        };
      }
      if (rate <= H264_HIGH_REFRESH_MAX_PIXEL_RATE) {
        return { level: 'optimal', reason: 'optimal', can_enable_hevc: false };
      }
      if (canUseHevc()) {
        var recUp = selectRecommendedPreset(presets, fn, 'hevc');
        return {
          level: rate <= HEVC_HIGH_REFRESH_MAX_PIXEL_RATE ? 'suggest_hevc' : 'above_sweet_spot',
          reason: 'h264_suggest_hevc',
          can_enable_hevc: true,
          recommended_width: recUp ? recUp.width : null,
          recommended_height: recUp ? recUp.height : null
        };
      }
      var recH264 = selectRecommendedPreset(presets, fn, 'h264');
      return {
        level: 'above_sweet_spot',
        reason: 'h264_no_hevc',
        can_enable_hevc: false,
        recommended_width: recH264 ? recH264.width : null,
        recommended_height: recH264 ? recH264.height : null
      };
    }

    function formatRecommendedTarget(adv) {
      var rw = adv && Number(adv.recommended_width);
      var rh = adv && Number(adv.recommended_height);
      var targetCodec = adv && adv.can_enable_hevc ? 'hevc' : effectiveCodec();
      var nativeInSweetSpot =
        streamState.nativeWidth > 0 &&
        streamState.nativeHeight > 0 &&
        isWithinHighRefreshSweetSpot(
          streamState.nativeWidth,
          streamState.nativeHeight,
          streamState.fps,
          targetCodec
        );
      if (isFinite(rw) && rw > 0 && isFinite(rh) && rh > 0) {
        if (
          streamState.nativeWidth > 0 &&
          Math.abs(rw - streamState.nativeWidth) <= 2 &&
          Math.abs(rh - streamState.nativeHeight) <= 2
        ) {
          return '原生 ' + rw + '×' + rh;
        }
        return nativeInSweetSpot ? rw + '×' + rh + ' 或原生分辨率' : rw + '×' + rh;
      }
      if (nativeInSweetSpot) {
        return '原生 ' + streamState.nativeWidth + '×' + streamState.nativeHeight;
      }
      return '较低分辨率';
    }

    function formatHudResolution(resText) {
      var clean = String(resText || '').trim();
      if (!clean) return '';
      var c = normalizeCodec(streamState.codec);
      if (!c) return clean;
      var tag = c === 'hevc' ? 'HEVC' : 'H.264';
      return clean + ' · ' + tag;
    }

    function syncCodecMenuUi() {
      var cur = effectiveCodec();
      var hevcAllowed = !IS_TAURI || !hasDisplayConfig || canUseHevc();
      if (codecHevcBtn) {
        var isHevc = cur === 'hevc';
        codecHevcBtn.setAttribute('aria-checked', isHevc ? 'true' : 'false');
        codecHevcBtn.setAttribute('aria-disabled', hevcAllowed ? 'false' : 'true');
        codecHevcBtn.title = hevcAllowed
          ? 'HEVC (H.265) 硬件编码：高分辨率与 120Hz+ 高刷推荐'
          : '当前主机或本机解码端不支持 HEVC 硬件编解码';
        var hc = codecHevcBtn.querySelector('.sn-check');
        if (hc) hc.hidden = !isHevc;
      }
      if (codecH264Btn) {
        var isH264 = cur !== 'hevc';
        codecH264Btn.setAttribute('aria-checked', isH264 ? 'true' : 'false');
        var ac = codecH264Btn.querySelector('.sn-check');
        if (ac) ac.hidden = !isH264;
      }
    }

    function syncAdvisoryHints() {
      var msg = '';
      if (!IS_TAURI || hasDisplayConfig) {
        var adv = evaluateHighRefreshAdvisoryLocal(
          streamState.width,
          streamState.height,
          streamState.fps,
          effectiveCodec()
        );
        if (adv && adv.level !== 'optimal') {
          var recText = formatRecommendedTarget(adv);
          if (adv.reason === 'h264_suggest_hevc') {
            if (adv.level === 'suggest_hevc') {
              msg = '当前 H.264 超出高刷甜点，开启 HEVC 即可跑满高刷帧率。';
            } else {
              msg =
                '当前 H.264 在此分辨率高刷下受硬编限制（约 60fps），建议开启 HEVC，满帧推荐 ' +
                recText +
                '。';
            }
          } else if (adv.reason === 'h264_no_hevc') {
            msg =
              '当前 H.264 在此分辨率高刷下受硬编限制，追求满帧建议切至 ' + recText + '。';
          } else if (adv.reason === 'hevc_over_budget') {
            msg =
              '当前分辨率超过高刷满帧甜点，动态画面可能低于目标帧率；满帧推荐 ' + recText + '。';
          }
        }
      }
      [qualityAdvisoryHintEl, screenAdvisoryHintEl, screenResAdvisoryHintEl].forEach(function (
        el
      ) {
        if (!el) return;
        if (msg) {
          el.textContent = msg;
          el.hidden = false;
        } else {
          el.textContent = '';
          el.hidden = true;
        }
      });
    }

    function partitionPresetsForMenu(presets, primaryLimit) {
      var list = Array.isArray(presets) ? presets : [];
      var limit = Math.max(1, Number(primaryLimit) || 5);
      if (list.length <= limit + 1) {
        var smallPrimary = list.slice();
        smallPrimary.sort(function (a, b) {
          if (Boolean(a.is_native) !== Boolean(b.is_native)) {
            return a.is_native ? -1 : 1;
          }
          var dh = (Number(b.height) || 0) - (Number(a.height) || 0);
          if (dh !== 0) return dh;
          return (Number(b.width) || 0) - (Number(a.width) || 0);
        });
        return { primary: smallPrimary, rest: [] };
      }
      var chosen = new Array(list.length);
      var primary = [];
      function pick(idx) {
        if (primary.length < limit && idx >= 0 && idx < list.length && !chosen[idx]) {
          chosen[idx] = true;
          primary.push(list[idx]);
        }
      }
      var nativeIdx = -1;
      for (var i = 0; i < list.length; i += 1) {
        if (list[i] && list[i].is_native) {
          nativeIdx = i;
          break;
        }
      }
      if (nativeIdx >= 0) pick(nativeIdx);

      if (streamState.width > 0 && streamState.height > 0) {
        for (var j = 0; j < list.length; j += 1) {
          if (isPresetActive(list[j])) {
            pick(j);
            break;
          }
        }
      }

      var rec = selectRecommendedPreset(list, streamState.fps, effectiveCodec());
      if (rec) {
        for (var k = 0; k < list.length; k += 1) {
          if (Number(list[k].width) === rec.width && Number(list[k].height) === rec.height) {
            pick(k);
            break;
          }
        }
      }

      var maxIdx = -1;
      var maxW = -1;
      var maxH = -1;
      for (var m = 0; m < list.length; m += 1) {
        var pw = Number(list[m] && list[m].width) || 0;
        var ph = Number(list[m] && list[m].height) || 0;
        if (pw > maxW || (pw === maxW && ph > maxH)) {
          maxW = pw;
          maxH = ph;
          maxIdx = m;
        }
      }
      if (maxIdx >= 0) pick(maxIdx);

      for (var hIdx = 0; hIdx < list.length && primary.length < limit; hIdx += 1) {
        if (list[hIdx] && list[hIdx].is_hidpi) pick(hIdx);
      }
      for (var fIdx = 0; fIdx < list.length && primary.length < limit; fIdx += 1) {
        pick(fIdx);
      }

      primary.sort(function (a, b) {
        if (Boolean(a.is_native) !== Boolean(b.is_native)) {
          return a.is_native ? -1 : 1;
        }
        var dh = (Number(b.height) || 0) - (Number(a.height) || 0);
        if (dh !== 0) return dh;
        return (Number(b.width) || 0) - (Number(a.width) || 0);
      });

      var rest = [];
      for (var rIdx = 0; rIdx < list.length; rIdx += 1) {
        if (!chosen[rIdx]) rest.push(list[rIdx]);
      }
      return { primary: primary, rest: rest };
    }

    function createSvgIcon(symbolId, size) {
      var svg = document.createElementNS(SVG_NS, 'svg');
      svg.setAttribute('class', 'sn-ico');
      svg.setAttribute('width', String(size));
      svg.setAttribute('height', String(size));
      svg.setAttribute('aria-hidden', 'true');
      var use = document.createElementNS(SVG_NS, 'use');
      use.setAttribute('href', '#' + symbolId);
      svg.appendChild(use);
      return svg;
    }

    function buildFallbackPresets(baseW, baseH) {
      var bw = Math.round(Number(baseW) || 0);
      var bh = Math.round(Number(baseH) || 0);
      if (!(bw > 0 && bh > 0)) return DEFAULT_MOCK_PRESETS.slice();
      var scales = [1, 0.85, 0.72, 0.55];
      var seen = Object.create(null);
      var list = [];
      scales.forEach(function (scale, idx) {
        var w = idx === 0 ? bw : Math.max(2, Math.round((bw * scale) / 2) * 2);
        var h = idx === 0 ? bh : Math.max(2, Math.round((bh * scale) / 2) * 2);
        if (idx > 0 && (w < 640 || h < 360)) return;
        var key = w + 'x' + h;
        if (seen[key]) return;
        seen[key] = true;
        list.push({
          width: w,
          height: h,
          label: w + ' × ' + h,
          is_native: idx === 0
        });
      });
      return list;
    }

    function formatPresetLabel(preset, w, h) {
      var raw = preset && preset.label ? String(preset.label).trim() : '';
      if (!raw) {
        raw = w + ' × ' + h;
      }
      raw = raw.replace(/^原生\s+/, '').replace(/\s*[（(]\s*原生\s*[)）]\s*$/, '');
      if (preset && preset.is_hidpi && raw.indexOf('(HiDPI)') === -1) {
        raw += ' (HiDPI)';
      }
      return raw || w + ' × ' + h;
    }

    function isPresetActive(preset) {
      if (!preset || !(streamState.width > 0 && streamState.height > 0)) return false;
      var w = Number(preset.width) || 0;
      var h = Number(preset.height) || 0;
      return Math.abs(streamState.width - w) <= 2 && Math.abs(streamState.height - h) <= 2;
    }

    function findMatchingPreset(presets, target) {
      if (!Array.isArray(presets) || !target) return null;
      var exact = null;
      var near = null;
      presets.forEach(function (p) {
        var pw = Number(p && p.width);
        var ph = Number(p && p.height);
        if (!(pw > 0 && ph > 0)) return;
        if (pw === target.width && ph === target.height && !exact) {
          exact = { width: pw, height: ph };
        } else if (
          !near &&
          Math.abs(pw - target.width) <= 2 &&
          Math.abs(ph - target.height) <= 2
        ) {
          near = { width: pw, height: ph };
        }
      });
      return exact || near;
    }

    function maybeAutoApplySavedResolution() {
      /* 首次连接保持被控端当前原始分辨率，不自动套用历史全局分辨率偏好 */
      autoAppliedSavedRes = true;
    }

    function buildHighRefreshSwitchToast(targetW, targetH, targetFps, prevCodec, actionLabel) {
      var autoUpgradedHevc =
        prevCodec === 'h264' &&
        userPinnedCodec !== 'h264' &&
        canUseHevc() &&
        !isWithinHighRefreshSweetSpot(targetW, targetH, targetFps, 'h264');
      var nextCodec = autoUpgradedHevc ? 'hevc' : prevCodec;
      var adv = evaluateHighRefreshAdvisoryLocal(targetW, targetH, targetFps, nextCodec);
      var recText = formatRecommendedTarget(adv);
      if (autoUpgradedHevc) {
        if (adv.level === 'above_sweet_spot') {
          return {
            autoHevc: true,
            text:
              actionLabel +
              '并已自动开启 HEVC；超过高刷满帧甜点，满帧推荐 ' +
              recText
          };
        }
        return {
          autoHevc: true,
          text: actionLabel + '，已自动开启 HEVC 以保障高刷流畅度'
        };
      }
      if (adv.level === 'above_sweet_spot') {
        if (adv.reason === 'h264_no_hevc') {
          return {
            autoHevc: false,
            text:
              actionLabel +
              '；当前 H.264 受硬编限制（约 60fps），满帧建议选 ' +
              recText
          };
        }
        return {
          autoHevc: false,
          text:
            actionLabel +
            '；超过高刷满帧甜点（实测约 80fps），满帧建议选 ' +
            recText
        };
      }
      return { autoHevc: false, text: '' };
    }

    function selectResolutionPreset(w, h) {
      var targetW = Number(w);
      var targetH = Number(h);
      var targetFps = Number(streamState.fps || 144);
      var prevWidth = streamState.width;
      var prevHeight = streamState.height;
      var prevRawCodec = streamState.codec;
      var prevPreferred = preferredResolution;
      var prevCodec = effectiveCodec();
      var feedback = buildHighRefreshSwitchToast(
        targetW,
        targetH,
        targetFps,
        prevCodec,
        '已切换至 ' + targetW + '×' + targetH
      );
      streamState.width = targetW;
      streamState.height = targetH;
      if (feedback.autoHevc) {
        streamState.codec = 'hevc';
        syncCodecMenuUi();
      }
      renderAllResolutionLists();
      syncAdvisoryHints();
      if (hudRows.length >= 8) {
        hudRows[7].textContent = formatHudResolution(targetW + 'x' + targetH);
      }
      preferredResolution = { width: targetW, height: targetH };
      closePopover();
      if (IS_TAURI) {
        if (typeof Bridge.saveStreamPrefs === 'function') {
          Bridge.saveStreamPrefs({
            resolutionWidth: targetW,
            resolutionHeight: targetH
          }).catch(function () {});
        }
        pendingUserResTarget = {
          width: targetW,
          height: targetH,
          suppressConfirmToast: Boolean(feedback.text)
        };
        Bridge.sessionCommand({
          type: 'set_stream_params',
          width: targetW,
          height: targetH,
          fps: targetFps
        }).catch(function (err) {
          pendingUserResTarget = null;
          streamState.width = prevWidth;
          streamState.height = prevHeight;
          streamState.codec = prevRawCodec;
          preferredResolution = prevPreferred;
          syncCodecMenuUi();
          renderAllResolutionLists();
          if (hudRows.length >= 8 && prevWidth > 0 && prevHeight > 0) {
            hudRows[7].textContent = formatHudResolution(prevWidth + 'x' + prevHeight);
          }
          toast('切换分辨率失败：' + String(err || '未知错误'));
        });
        toast(feedback.text || '正在切换分辨率至 ' + targetW + '×' + targetH + '…');
      } else {
        toast(feedback.text || '已切换分辨率至 ' + targetW + '×' + targetH);
      }
    }

    function appendResolutionButton(container, preset, isNestedItem, recSweetSpot) {
      var w = Number(preset.width) || 0;
      var h = Number(preset.height) || 0;
      if (!(w > 0 && h > 0)) return;
      var isActive = isPresetActive(preset);

      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'sn-submenu__item';
      btn.setAttribute('role', 'menuitemradio');
      btn.setAttribute('aria-checked', isActive ? 'true' : 'false');
      btn.setAttribute('data-res-width', String(w));
      btn.setAttribute('data-res-height', String(h));

      var check = document.createElement('span');
      check.className = 'sn-check';
      check.hidden = !isActive;
      check.appendChild(createSvgIcon('i-check', 14));
      btn.appendChild(check);

      var label = document.createElement('span');
      label.className = 'sn-res-label';
      label.textContent = formatPresetLabel(preset, w, h);
      btn.appendChild(label);

      var badgeText = '';
      var badgeClass = 'sn-res-badge';
      var curFps = Number(streamState.fps) || 0;
      var curCodec = effectiveCodec();
      if (preset.is_native) {
        badgeText = '原生';
      } else if (curFps >= HIGH_REFRESH_FPS_THRESHOLD) {
        if (
          recSweetSpot &&
          recSweetSpot.width === w &&
          recSweetSpot.height === h &&
          isWithinHighRefreshSweetSpot(w, h, curFps, curCodec)
        ) {
          badgeText = '高刷推荐';
        } else if (!isWithinHighRefreshSweetSpot(w, h, curFps, curCodec)) {
          badgeText =
            curCodec === 'h264' &&
            canUseHevc() &&
            isWithinHighRefreshSweetSpot(w, h, curFps, 'hevc')
              ? '建议 HEVC'
              : '高负载';
          badgeClass = 'sn-res-badge sn-res-badge--warn';
        }
      }
      if (badgeText) {
        var badge = document.createElement('span');
        badge.className = badgeClass;
        badge.textContent = badgeText;
        btn.appendChild(badge);
      }

      if (!isNestedItem) {
        btn.addEventListener('pointerenter', function () {
          if (typeof scheduleNestedClose === 'function') scheduleNestedClose();
        });
      }
      btn.addEventListener('click', function (e) {
        e.preventDefault();
        selectResolutionPreset(w, h);
      });

      container.appendChild(btn);
    }

    function renderMoreResolutionList(presets, recSweetSpot) {
      if (!screenMoreResListEl) return;
      while (screenMoreResListEl.firstChild) {
        screenMoreResListEl.removeChild(screenMoreResListEl.firstChild);
      }
      presets.forEach(function (preset) {
        appendResolutionButton(screenMoreResListEl, preset, true, recSweetSpot);
      });
    }

    function renderResolutionList(container) {
      if (!container) return;
      while (container.firstChild) {
        container.removeChild(container.firstChild);
      }
      var presets = Array.isArray(streamState.presets) ? streamState.presets : [];
      var recSweetSpot = selectRecommendedPreset(presets, streamState.fps, effectiveCodec());
      var split = partitionPresetsForMenu(presets, 5);
      var primary = split.primary;
      var rest = split.rest;
      if (!rest.length) {
        primary.forEach(function (preset) {
          appendResolutionButton(container, preset, false, recSweetSpot);
        });
        return;
      }

      primary.forEach(function (preset) {
        appendResolutionButton(container, preset, false, recSweetSpot);
      });

      var moreActive = rest.some(isPresetActive);
      var moreBtn = document.createElement('button');
      moreBtn.type = 'button';
      moreBtn.className = 'sn-submenu__item';
      moreBtn.setAttribute('role', 'menuitem');
      moreBtn.setAttribute('aria-haspopup', 'menu');
      moreBtn.setAttribute('aria-expanded', 'false');

      var check = document.createElement('span');
      check.className = 'sn-check';
      check.hidden = !moreActive;
      check.appendChild(createSvgIcon('i-check', 14));
      moreBtn.appendChild(check);

      var label = document.createElement('span');
      label.className = 'sn-res-label';
      label.textContent = '其他分辨率';
      moreBtn.appendChild(label);

      var chevronWrap = document.createElement('span');
      chevronWrap.className = 'sn-menu__chevron';
      chevronWrap.appendChild(createSvgIcon('i-chevron-right', 14));
      moreBtn.appendChild(chevronWrap);

      var openMore = function () {
        if (typeof cancelNestedClose === 'function') cancelNestedClose();
        renderMoreResolutionList(rest, recSweetSpot);
        if (typeof openNestedMenu === 'function' && screenMoreEl) {
          openNestedMenu(moreBtn, screenMoreEl);
        }
      };
      moreBtn.addEventListener('pointerenter', openMore);
      moreBtn.addEventListener('focus', openMore);
      moreBtn.addEventListener('click', function (e) {
        e.preventDefault();
        openMore();
      });

      container.appendChild(moreBtn);
    }

    function renderAllResolutionLists() {
      renderResolutionList(screenMainResListEl);
      renderResolutionList(screenResListEl);
      if (screenMoreEl && !screenMoreEl.hidden) {
        var presets = Array.isArray(streamState.presets) ? streamState.presets : [];
        var recSweetSpot = selectRecommendedPreset(presets, streamState.fps, effectiveCodec());
        var split = partitionPresetsForMenu(presets, 5);
        if (split.rest.length) {
          renderMoreResolutionList(split.rest, recSweetSpot);
        } else if (typeof closeNestedMenu === 'function') {
          closeNestedMenu();
        }
      }
      syncAdvisoryHints();
    }

    function handleFitSessionWindow(e) {
      if (e) e.preventDefault();
      closePopover();
      if (IS_TAURI && typeof Bridge.fitSessionWindow === 'function') {
        Bridge.fitSessionWindow()
          .then(function () {
            toast('已适配窗口比例，消除黑边');
          })
          .catch(function (err) {
            toast('适配窗口比例失败：' + String(err || '未知错误'));
          });
        return;
      }
      toast('已适配窗口比例，消除黑边');
    }

    syncCodecMenuUi();
    renderAllResolutionLists();

    /* ── S4 HUD 显隐（控制中心 → 更多 → 显示网络状态）─────────────── */
    if (showHudBtn) {
      showHudBtn.addEventListener('click', function (e) {
        e.preventDefault();
        var show = hudEl ? hudEl.hidden : true;
        applyHudVisibility(show, true);
        toast(show ? '已显示网络状态' : '已隐藏网络状态');
        closePopover();
      });
    }

    var prefsLoaded = false;
    var displaysRequested = false;
    var initialSessionPrefsApplied = false;

    function normalizePrivacyPreset(p) {
      if (p === 'custom') return 'custom';
      return PRESET_LABELS[p] ? p : 'preset-1';
    }

    function requestSessionDisplaysOnce() {
      if (displaysRequested) return;
      if (IS_TAURI && typeof Bridge.listSessionDisplays === 'function') {
        displaysRequested = true;
        Bridge.listSessionDisplays().catch(function () {
          displaysRequested = false;
        });
      }
    }

    function applySavedSessionPrefsIfReady() {
      if (
        initialSessionPrefsApplied ||
        !prefsLoaded ||
        !realSessionActive ||
        sessionLifecycleState !== 'active' ||
        watchMode
      ) {
        return;
      }
      initialSessionPrefsApplied = true;
      if (privacyState.mode && privacyState.mode !== 'off') {
        applyPrivacyMode(privacyState.mode, privacyState.preset, null, false, false);
      }
      if (hostAudioState.muted) {
        applyHostSystemMute(true, hostAudioState.restoreOnExit, false, false);
      }
      if (peripheralsState.mouseMode && peripheralsState.mouseMode !== 'smart') {
        applyMouseMode(peripheralsState.mouseMode, false, false);
      }
      if (peripheralsState.sendSystemShortcuts) {
        applySystemShortcuts(true, peripheralsState.shortcutExceptions, false, false);
      }
    }

    if (IS_TAURI && typeof Bridge.getAppPrefs === 'function') {
      Bridge.getAppPrefs()
        .then(function (prefs) {
          if (!prefs || typeof prefs !== 'object') return;
          applyHudVisibility(prefs.showHud !== false, false);
          applyLockOnExit(Boolean(prefs.lockOnExit), false, false);
          if (prefs.bitratePreset || prefs.bitrateBps) {
            syncBitrateMenuUi(prefs.bitratePreset || prefs.bitrateBps);
          }
          var savedFps = Number(prefs.fps);
          if (isFinite(savedFps) && savedFps > 0 && !seenNegotiated) {
            streamState.fps = savedFps;
            syncFpsMenuUi(savedFps);
          }
          var rw = Number(prefs.resolutionWidth);
          var rh = Number(prefs.resolutionHeight);
          if (isFinite(rw) && rw > 0 && isFinite(rh) && rh > 0) {
            preferredResolution = { width: rw, height: rh };
            maybeAutoApplySavedResolution();
          }
          if (!hasActiveSessionSecurity && typeof prefs.defaultEncryptConnection === 'boolean') {
            linkSecurityState.encryptConnection = prefs.defaultEncryptConnection;
            syncLinkSecurityUi();
          }
          if (typeof prefs.privacyMode === 'string') {
            privacyState.mode = prefs.privacyMode;
          }
          if (typeof prefs.privacyPreset === 'string' && prefs.privacyPreset) {
            privacyState.preset = normalizePrivacyPreset(prefs.privacyPreset);
          }
          if (typeof prefs.hostMuteOnConnect === 'boolean') {
            hostAudioState.muted = prefs.hostMuteOnConnect;
          }
          if (typeof prefs.hostRestoreAudioOnExit === 'boolean') {
            hostAudioState.restoreOnExit = prefs.hostRestoreAudioOnExit;
          }
          if (typeof prefs.sendSystemShortcuts === 'boolean') {
            peripheralsState.sendSystemShortcuts = prefs.sendSystemShortcuts;
          }
          if (Array.isArray(prefs.shortcutExceptions)) {
            peripheralsState.shortcutExceptions = prefs.shortcutExceptions.slice();
          }
          if (typeof prefs.mouseMode === 'string' && prefs.mouseMode) {
            peripheralsState.mouseMode = prefs.mouseMode;
          }
          prefsLoaded = true;
          syncPrivacyAndHostAudioUi();
          syncPeripheralsUi();
          applySavedSessionPrefsIfReady();
        })
        .catch(function () {});
    }
    if (IS_TAURI && typeof Bridge.getPrivacyCustomImage === 'function') {
      Bridge.getPrivacyCustomImage()
        .then(function (dataUrl) {
          if (typeof dataUrl === 'string' && dataUrl.indexOf('data:image/') === 0) {
            privacyState.customDataUrl = dataUrl;
            syncPrivacyModalUi();
          }
        })
        .catch(function () {});
    }
    var hasActiveSessionSecurity = false;
    if (IS_TAURI && typeof Bridge.getSecurityPrefs === 'function') {
      Bridge.getSecurityPrefs()
        .then(function (sec) {
          if (!sec || typeof sec !== 'object') return;
          if (!hasActiveSessionSecurity && typeof sec.defaultEncryptConnection === 'boolean') {
            linkSecurityState.encryptConnection = sec.defaultEncryptConnection;
            syncLinkSecurityUi();
          }
        })
        .catch(function () {});
    }
    if (IS_TAURI && typeof Bridge.getActiveSessionInfo === 'function') {
      Bridge.getActiveSessionInfo()
        .then(function (info) {
          if (!info || !info.active) return;
          hasActiveSessionSecurity = true;
          if (typeof info.mtlsActive === 'boolean' && info.connected) {
            linkSecurityState.encryptConnection = info.mtlsActive;
          } else if (typeof info.encryptConnection === 'boolean') {
            linkSecurityState.encryptConnection = info.encryptConnection;
          }
          if (info.peerCertFingerprint) {
            linkSecurityState.peerFingerprint = String(info.peerCertFingerprint);
          }
          if (info.transport) {
            linkSecurityState.transport = info.transport;
          }
          syncLinkSecurityUi();
        })
        .catch(function () {});
    }
    syncLinkSecurityUi();

    /* ════════════════════════════════════════════════════════════
     * 浮层管理（§7.4：点锚点开合、点外部关闭、同时只允许一个）
     * ════════════════════════════════════════════════════════════ */
    var open = null; /* { el, anchor, align } */

    function setArrow(el, anchor) {
      if (!el.classList.contains('sn-security')) return;
      var pr = el.getBoundingClientRect();
      var ar = anchor.getBoundingClientRect();
      var x = Math.round(ar.left + ar.width / 2 - pr.left);
      var lo = 16;
      var hi = Math.max(lo, pr.width - 16);
      el.style.setProperty('--sn-arrow-x', Math.max(lo, Math.min(hi, x)) + 'px');
    }

    /* §3.6：距锚点垂直 8px；S2 水平居中于锚点，S3 右缘对齐锚点右缘。
     * 边界 = 会话窗口（Tauri 内窗口即视口；浏览器预览时窗口居中，不能溢出窗口）。 */
    function place(el, anchor, align) {
      var r = anchor.getBoundingClientRect();
      var w = el.offsetWidth;
      var h = el.offsetHeight;
      var bounds = win.getBoundingClientRect();
      var gap = 8;
      var margin = 8;
      var loX = bounds.left + margin;
      var hiX = bounds.right - w - margin;
      var loY = bounds.top + margin;
      var hiY = bounds.bottom - h - margin;

      var left;
      if (align === 'center') {
        left = r.left + r.width / 2 - w / 2;
      } else if (align === 'left') {
        left = r.left;
      } else {
        left = r.right - w;
      }
      left = Math.max(loX, Math.min(left, Math.max(loX, hiX)));

      var top = r.bottom + gap;
      var above = false;
      if (top > hiY && r.top - gap - h >= loY) {
        top = r.top - gap - h; /* 文档未定义翻面；空间不足时的镜像兜底 */
        above = true;
      }
      top = Math.max(loY, Math.min(top, Math.max(loY, hiY)));

      el.style.left = Math.round(left) + 'px';
      el.style.top = Math.round(top) + 'px';
      el.classList.toggle('is-above', above);
      setArrow(el, anchor);
    }

    function closePopover() {
      if (!open) return;
      open.el.hidden = true;
      if (open.anchor && open.anchor.setAttribute) {
        open.anchor.setAttribute('aria-expanded', 'false');
      }
      closeSubmenu();
      open = null;
    }

    function openPopover(el, anchor, align) {
      if (open && open.el === el) {
        closePopover();
        return;
      }
      closePopover();
      el.hidden = false;
      open = { el: el, anchor: anchor, align: align };
      place(el, anchor, align);
      if (anchor && anchor.setAttribute) {
        anchor.setAttribute('aria-expanded', 'true');
      }
    }

    function onOutsidePointer(e) {
      if (!open) return;
      if (open.el.contains(e.target)) return;
      if (open.anchor && open.anchor.contains(e.target)) return;
      if (activeSubmenuEl && !activeSubmenuEl.hidden && activeSubmenuEl.contains(e.target)) return;
      if (activeNestedEl && !activeNestedEl.hidden && activeNestedEl.contains(e.target)) return;
      closePopover();
    }

    document.addEventListener('pointerdown', onOutsidePointer, true);

    /* ── S2 安全状态气泡 & 防窥隐私幕布 & 被控端静音 & 外设 & 虚拟屏 ── */
    var secPrivacy = document.getElementById('sec-privacy');
    var secPrivacyDesc = document.getElementById('sec-privacy-desc');
    var secMute = document.getElementById('sec-mute');
    var secLock = document.getElementById('sec-lock');
    var secMuteValue = document.getElementById('sec-mute-value');
    var secLockValue = document.getElementById('sec-lock-value');
    var warningPill = document.getElementById('security-warning');

    var privacyState = {
      mode: 'off',
      preset: 'preset-1',
      customDataUrl: '',
      draftPreset: 'preset-1',
      draftCustomDataUrl: ''
    };
    var hostAudioState = {
      muted: false,
      restoreOnExit: true
    };
    var remoteAudioMuted = false;
    var peripheralsState = {
      sendSystemShortcuts: false,
      shortcutExceptions: [],
      mouseMode: 'smart'
    };
    var displaysState = {
      displays: [],
      activeDisplayId: 0
    };

    var SHORTCUT_CATALOG = [
      { id: 'cmd+tab', label: '切换应用程序', key: '⌘Tab' },
      { id: 'cmd+space', label: '聚焦搜索 (Spotlight)', key: '⌘Space' },
      { id: 'cmd+q', label: '退出当前应用', key: '⌘Q' },
      { id: 'cmd+h', label: '隐藏当前窗口', key: '⌘H' },
      { id: 'cmd+m', label: '最小化当前窗口', key: '⌘M' },
      { id: 'shift+cmd+3', label: '全屏截图', key: '⇧⌘3' },
      { id: 'shift+cmd+4', label: '区域截图', key: '⇧⌘4' },
      { id: 'shift+cmd+5', label: '录屏与截屏面板', key: '⇧⌘5' },
      { id: 'alt+cmd+esc', label: '强制退出应用', key: '⌥⌘Esc' },
      { id: 'ctrl+up', label: '调度中心 (Mission Control)', key: '⌃↑' },
      { id: 'ctrl+left', label: '切换左侧桌面空间', key: '⌃←' },
      { id: 'ctrl+right', label: '切换右侧桌面空间', key: '⌃→' }
    ];

    var PRESET_LABELS = {
      'preset-1': '守护深蓝 · 远程隐私保护中',
      'preset-2': '深邃星空 · 远程隐私保护中',
      'preset-3': '静谧森绿 · 远程隐私保护中',
      'preset-4': '极光紫夜 · 远程隐私保护中',
      'preset-5': '极简石墨 · 远程隐私保护中',
      'preset-6': '纯黑防窥 · 远程隐私保护中',
      custom: '自定义隐私屏保'
    };

    var MOUSE_MODE_LABELS = {
      smart: '智能鼠标',
      remote: '使用被控端鼠标',
      local: '使用主控端鼠标'
    };

    function isAnySessionModalOpen() {
      if (privacyModalBackdrop && !privacyModalBackdrop.hidden) return true;
      if (privacyPreviewOverlay && !privacyPreviewOverlay.hidden) return true;
      if (shortcutsModalBackdrop && !shortcutsModalBackdrop.hidden) return true;
      if (remoteUpdateModalBackdrop && !remoteUpdateModalBackdrop.hidden) return true;
      return false;
    }

    /* ── S1 远程升级被控端（A1 + A3：严格仅本人自控可见 + 弹窗显式确认）── */
    function parseSemverParts(raw) {
      var clean = String(raw || '').trim().replace(/^[vV]/, '').split('-')[0].split('+')[0];
      var parts = clean.split('.');
      if (parts.length < 2 || parts.length > 3) return null;
      var nums = [];
      for (var i = 0; i < 3; i++) {
        var seg = parts[i] === undefined ? '0' : parts[i];
        if (!/^\d+$/.test(seg)) return null;
        nums.push(parseInt(seg, 10));
      }
      return nums;
    }

    function isNewerSemver(candidate, current) {
      var a = parseSemverParts(candidate);
      var b = parseSemverParts(current);
      if (!a || !b) return false;
      for (var i = 0; i < 3; i++) {
        if (a[i] !== b[i]) return a[i] > b[i];
      }
      return false;
    }

    function formatUpdateBytes(bytes) {
      var n = Number(bytes || 0);
      if (!isFinite(n) || n <= 0) return '0 B';
      if (n < 1024) return n + ' B';
      if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
      return (n / (1024 * 1024)).toFixed(1) + ' MB';
    }

    var postRestartVerifyTimer = 0;

    function cancelPostRestartVerifyTimer() {
      if (postRestartVerifyTimer) {
        window.clearTimeout(postRestartVerifyTimer);
        postRestartVerifyTimer = 0;
      }
    }

    function syncRemoteUpdateUi() {
      var hv = remoteUpdateState.hostVersion;
      var rel = remoteUpdateState.latestRelease;
      var targetVer = (rel && rel.version) || remoteUpdateState.targetVersion || '';
      /* 安全红线（A3）：非本人自控会话严禁展示任何升级图标与入口 */
      var isOwnerCapable = Boolean(hv && hv.isOwnerSession === true && hv.updateCapable === true);
      var hasNewer = Boolean(isOwnerCapable && rel && rel.version && isNewerSemver(rel.version, hv.version));
      var visible = isOwnerCapable && (hasNewer || remoteUpdateState.inProgress);

      if (btnRemoteUpdate) {
        btnRemoteUpdate.hidden = !visible;
        if (visible && targetVer) {
          btnRemoteUpdate.title = remoteUpdateState.inProgress
            ? '被控端正在升级至 v' + targetVer
            : '被控端可升级至 v' + targetVer + '（当前 v' + hv.version + '），点击确认升级';
        }
      }
      if (menuRemoteUpdateSep) menuRemoteUpdateSep.hidden = !visible;
      if (btnMenuRemoteUpdate) btnMenuRemoteUpdate.hidden = !visible;
      if (menuRemoteUpdateLabel && targetVer) {
        menuRemoteUpdateLabel.textContent = remoteUpdateState.inProgress
          ? '正在升级被控端…'
          : '升级被控端至 v' + targetVer;
      }
      var awaitingPostRestartHostVersion =
        remoteUpdateState.inProgress && remoteUpdateState.stage === 'restarting' && !hv;
      if (
        !isOwnerCapable &&
        remoteUpdateModalBackdrop &&
        !remoteUpdateModalBackdrop.hidden &&
        !awaitingPostRestartHostVersion
      ) {
        remoteUpdateModalBackdrop.hidden = true;
      }
    }

    function closeRemoteUpdateModal() {
      if (!remoteUpdateModalBackdrop) return;
      if (remoteUpdateState.stage === 'installing') return;
      if (!remoteUpdateState.inProgress) {
        remoteUpdateState.lastError = '';
      }
      remoteUpdateModalBackdrop.hidden = true;
    }

    function openRemoteUpdateModal() {
      if (!remoteUpdateModalBackdrop) return;
      var hv = remoteUpdateState.hostVersion;
      var rel = remoteUpdateState.latestRelease;
      var targetVer = (rel && rel.version) || remoteUpdateState.targetVersion || '';
      if (
        !hv ||
        hv.isOwnerSession !== true ||
        !hv.updateCapable ||
        !targetVer ||
        (!rel && !remoteUpdateState.inProgress)
      ) {
        return;
      }
      closeAllPopovers();
      if (remoteUpdateVerPill) {
        remoteUpdateVerPill.textContent = 'v' + hv.version + ' → v' + targetVer;
      }
      if (remoteUpdateArch) {
        remoteUpdateArch.textContent = (hv.os || 'macos') + ' · ' + (hv.arch || '');
      }
      if (remoteUpdateDesc) {
        remoteUpdateDesc.textContent = hv.serviceInstalled
          ? '升级期间被控端将下载并校验官方签名包，安装完成后会自动重启后台服务并无缝恢复当前远控连接。若被控端直连下载失败，将自动由本机中转推送安装包。'
          : '升级期间被控端将下载并校验官方签名包，安装完成后会自动重启并尝试恢复连接（若应用目录无写权限，需先在被控端安装系统级后台服务）。';
      }
      if (remoteUpdateNotes) {
        if (rel && rel.notes && String(rel.notes).trim()) {
          remoteUpdateNotes.hidden = false;
          remoteUpdateNotes.textContent = String(rel.notes).trim();
        } else {
          remoteUpdateNotes.hidden = true;
        }
      }
      if (!rel && IS_TAURI && typeof Bridge.getCachedRelease === 'function') {
        Bridge.getCachedRelease()
          .then(function (cached) {
            if (cached && cached.version) {
              remoteUpdateState.latestRelease = cached;
              if (remoteUpdateNotes && cached.notes && String(cached.notes).trim()) {
                remoteUpdateNotes.hidden = false;
                remoteUpdateNotes.textContent = String(cached.notes).trim();
              }
              syncRemoteUpdateUi();
            }
          })
          .catch(function () {});
      }
      if (!remoteUpdateState.inProgress) {
        if (remoteUpdateProgress) remoteUpdateProgress.hidden = true;
        if (remoteUpdateError) {
          if (remoteUpdateState.lastError) {
            remoteUpdateError.hidden = false;
            remoteUpdateError.textContent = remoteUpdateState.lastError;
          } else {
            remoteUpdateError.hidden = true;
          }
        }
        if (remoteUpdateConfirmBtn) {
          remoteUpdateConfirmBtn.hidden = false;
          remoteUpdateConfirmBtn.disabled = false;
          remoteUpdateConfirmBtn.textContent = remoteUpdateState.lastError ? '重试升级' : '确认升级';
        }
        if (remoteUpdateCancelBtn) {
          remoteUpdateCancelBtn.disabled = false;
          remoteUpdateCancelBtn.textContent = remoteUpdateState.lastError ? '关闭' : '取消';
        }
      }
      remoteUpdateModalBackdrop.hidden = false;
    }

    function applyRemoteUpdateStatus(data) {
      if (!data || typeof data !== 'object') return;
      var st = String(data.stage || '');
      remoteUpdateState.stage = st;
      if (data.targetVersion) {
        remoteUpdateState.targetVersion = String(data.targetVersion);
      }
      var pct = Math.max(0, Math.min(100, Number(data.progressPct || 0)));

      if (remoteUpdateProgress) remoteUpdateProgress.hidden = false;
      if (remoteUpdateBarFill) remoteUpdateBarFill.style.width = pct + '%';
      if (remoteUpdateStage) {
        remoteUpdateStage.textContent = String(data.message || '正在升级被控端…');
      }
      if (remoteUpdateBytes) {
        if (data.totalBytes && Number(data.totalBytes) > 0) {
          remoteUpdateBytes.textContent =
            formatUpdateBytes(data.downloadedBytes) +
            ' / ' +
            formatUpdateBytes(data.totalBytes) +
            ' (' +
            pct +
            '%)';
        } else if (pct > 0) {
          remoteUpdateBytes.textContent = pct + '%';
        } else {
          remoteUpdateBytes.textContent = '';
        }
      }

      if (st === 'downloading' || st === 'relaying' || st === 'verifying') {
        remoteUpdateState.inProgress = true;
        remoteUpdateState.lastError = '';
        if (remoteUpdateError) remoteUpdateError.hidden = true;
        if (remoteUpdateConfirmBtn) {
          remoteUpdateConfirmBtn.disabled = true;
          remoteUpdateConfirmBtn.textContent = st === 'relaying' ? '中转推送中…' : '升级中…';
        }
        if (remoteUpdateCancelBtn) {
          remoteUpdateCancelBtn.disabled = false;
          remoteUpdateCancelBtn.textContent = '取消升级';
        }
      } else if (st === 'installing' || st === 'restarting') {
        remoteUpdateState.inProgress = true;
        remoteUpdateState.lastError = '';
        if (remoteUpdateError) remoteUpdateError.hidden = true;
        if (remoteUpdateConfirmBtn) {
          remoteUpdateConfirmBtn.disabled = true;
          remoteUpdateConfirmBtn.textContent = st === 'restarting' ? '等待重连…' : '安装中…';
        }
        if (remoteUpdateCancelBtn) {
          remoteUpdateCancelBtn.disabled = st === 'installing';
          remoteUpdateCancelBtn.textContent = st === 'restarting' ? '隐藏窗口' : '安装中…';
        }
        if (st === 'restarting') {
          toast('被控端已完成安装，正在重启并自动重连…');
        }
      } else if (st === 'failed') {
        var errMsg = String(data.error || data.message || '被控端升级失败');
        remoteUpdateState.inProgress = false;
        remoteUpdateState.lastError = errMsg;
        if (remoteUpdateError) {
          remoteUpdateError.hidden = false;
          remoteUpdateError.textContent = errMsg;
        }
        if (remoteUpdateModalBackdrop && remoteUpdateModalBackdrop.hidden) {
          toast('被控端升级失败：' + errMsg);
        }
        if (remoteUpdateConfirmBtn) {
          remoteUpdateConfirmBtn.hidden = false;
          remoteUpdateConfirmBtn.disabled = false;
          remoteUpdateConfirmBtn.textContent = '重试升级';
        }
        if (remoteUpdateCancelBtn) {
          remoteUpdateCancelBtn.disabled = false;
          remoteUpdateCancelBtn.textContent = '关闭';
        }
      }
      syncRemoteUpdateUi();
    }

    if (btnRemoteUpdate) {
      btnRemoteUpdate.addEventListener('click', function () {
        openRemoteUpdateModal();
      });
    }
    if (btnMenuRemoteUpdate) {
      btnMenuRemoteUpdate.addEventListener('click', function () {
        openRemoteUpdateModal();
      });
    }
    if (remoteUpdateCloseBtn) {
      remoteUpdateCloseBtn.addEventListener('click', closeRemoteUpdateModal);
    }
    if (remoteUpdateCancelBtn) {
      remoteUpdateCancelBtn.addEventListener('click', function () {
        if (
          remoteUpdateState.inProgress &&
          remoteUpdateState.stage !== 'restarting' &&
          remoteUpdateState.stage !== 'installing' &&
          Bridge &&
          typeof Bridge.cancelRemoteUpdate === 'function'
        ) {
          remoteUpdateCancelBtn.disabled = true;
          Bridge.cancelRemoteUpdate().catch(function () {});
          remoteUpdateState.inProgress = false;
          remoteUpdateState.stage = 'idle';
          syncRemoteUpdateUi();
        }
        closeRemoteUpdateModal();
      });
    }
    if (remoteUpdateConfirmBtn) {
      remoteUpdateConfirmBtn.addEventListener('click', function () {
        var rel = remoteUpdateState.latestRelease;
        if (!Bridge || typeof Bridge.triggerRemoteUpdate !== 'function' || !rel || !rel.version) {
          return;
        }
        remoteUpdateState.inProgress = true;
        remoteUpdateState.stage = 'downloading';
        remoteUpdateState.targetVersion = String(rel.version);
        remoteUpdateState.lastError = '';
        if (remoteUpdateError) remoteUpdateError.hidden = true;
        if (remoteUpdateProgress) remoteUpdateProgress.hidden = false;
        if (remoteUpdateBarFill) remoteUpdateBarFill.style.width = '0%';
        if (remoteUpdateStage) remoteUpdateStage.textContent = '正在下发升级指令至被控端…';
        if (remoteUpdateBytes) remoteUpdateBytes.textContent = '';
        remoteUpdateConfirmBtn.disabled = true;
        remoteUpdateConfirmBtn.textContent = '升级中…';
        if (remoteUpdateCancelBtn) remoteUpdateCancelBtn.textContent = '取消升级';
        syncRemoteUpdateUi();

        Bridge.triggerRemoteUpdate(rel.version).catch(function (err) {
          applyRemoteUpdateStatus({
            stage: 'failed',
            progressPct: 0,
            message: String(err || '触发远程更新失败'),
            error: String(err || '触发远程更新失败')
          });
        });
      });
    }

    /* 「需关注」判定：仅当 防窥模式=关闭 且 自动锁屏=关 时才显示 */
    function syncSecurityState() {
      if (!secPrivacy || !secMute || !secLock) return;
      var privacyOff = secPrivacy.value === 'off';
      var lockOn = Boolean(secLock.checked);
      var hostMutedOn = Boolean(secMute.checked);

      if (warningPill) warningPill.hidden = !(privacyOff && !lockOn);

      if (secPrivacyDesc) {
        if (secPrivacy.value === 'privacy') {
          secPrivacyDesc.textContent = '被控端屏幕已防护，遮挡屏幕内容';
        } else if (secPrivacy.value === 'black') {
          secPrivacyDesc.textContent = '被控端已黑屏，防止周围窥视';
        } else {
          secPrivacyDesc.textContent = '被控端屏幕内容当前可见';
        }
      }

      if (secMuteValue) secMuteValue.textContent = hostMutedOn ? '已静音' : '已外放';
      if (secLockValue) secLockValue.textContent = lockOn ? '已开启' : '未开启';

      if (soundPlayBtn) {
        var playAudio = !remoteAudioMuted;
        soundPlayBtn.setAttribute('aria-checked', playAudio ? 'true' : 'false');
        var sc = soundPlayBtn.querySelector('.sn-check');
        if (sc) sc.hidden = !playAudio;
      }
      if (autoLockExitBtn) {
        autoLockExitBtn.setAttribute('aria-checked', lockOn ? 'true' : 'false');
        var lc = autoLockExitBtn.querySelector('.sn-check');
        if (lc) lc.hidden = !lockOn;
      }
      if (hostMuteBtn) {
        hostMuteBtn.setAttribute('aria-checked', hostMutedOn ? 'true' : 'false');
        var hmc = hostMuteBtn.querySelector('.sn-check');
        if (hmc) hmc.hidden = !hostMutedOn;
      }
      if (hostRestoreAudioBtn) {
        var restoreOn = Boolean(hostAudioState.restoreOnExit);
        hostRestoreAudioBtn.setAttribute('aria-checked', restoreOn ? 'true' : 'false');
        var hrc = hostRestoreAudioBtn.querySelector('.sn-check');
        if (hrc) hrc.hidden = !restoreOn;
      }
      if (privacyModeValEl) {
        privacyModeValEl.textContent =
          secPrivacy.value === 'privacy'
            ? '隐私屏保'
            : secPrivacy.value === 'black'
              ? '黑屏防窥'
              : '不启用';
      }
      if (privacyModeMenuEl) {
        Array.prototype.forEach.call(
          privacyModeMenuEl.querySelectorAll('[data-privacy-opt]'),
          function (btn) {
            var checked = btn.getAttribute('data-privacy-opt') === secPrivacy.value;
            btn.setAttribute('aria-checked', checked ? 'true' : 'false');
            var ck = btn.querySelector('.sn-check');
            if (ck) ck.hidden = !checked;
          }
        );
      }
    }

    function syncPrivacyAndHostAudioUi() {
      if (secPrivacy) {
        secPrivacy.value =
          privacyState.mode === 'privacy' || privacyState.mode === 'black'
            ? privacyState.mode
            : 'off';
      }
      if (secMute) {
        secMute.checked = Boolean(hostAudioState.muted);
      }
      syncSecurityState();
    }

    function applyPrivacyMode(mode, preset, customDataUrl, persist, showToast) {
      var prevMode = privacyState.mode;
      var prevPreset = privacyState.preset;
      var prevCustom = privacyState.customDataUrl;
      var normMode = mode === 'privacy' || mode === 'black' ? mode : 'off';
      var effPreset = normalizePrivacyPreset(preset || privacyState.preset || 'preset-1');
      privacyState.mode = normMode;
      privacyState.preset = effPreset;
      if (customDataUrl) privacyState.customDataUrl = customDataUrl;
      syncPrivacyAndHostAudioUi();

      if (IS_TAURI && typeof Bridge.setSessionPrivacyMode === 'function') {
        Bridge.setSessionPrivacyMode(
          normMode,
          effPreset,
          effPreset === 'custom' ? privacyState.customDataUrl || null : null,
          persist !== false
        )
          .then(function () {
            if (showToast) {
              toast(
                normMode === 'privacy'
                  ? '已开启被控端隐私屏保'
                  : normMode === 'black'
                    ? '已开启被控端黑屏防窥'
                    : '已关闭被控端防窥模式'
              );
            }
          })
          .catch(function (err) {
            privacyState.mode = prevMode;
            privacyState.preset = prevPreset;
            privacyState.customDataUrl = prevCustom;
            syncPrivacyAndHostAudioUi();
            toast('设置防窥模式失败：' + String(err || '未知错误'));
          });
      } else if (showToast) {
        toast(
          normMode === 'privacy'
            ? '已开启被控端隐私屏保'
            : normMode === 'black'
              ? '已开启被控端黑屏防窥'
              : '已关闭被控端防窥模式'
        );
      }
    }

    function applyHostSystemMute(muted, restoreOnExit, persist, showToast, customSuccessToast) {
      var prevMuted = hostAudioState.muted;
      var prevRestore = hostAudioState.restoreOnExit;
      hostAudioState.muted = Boolean(muted);
      if (typeof restoreOnExit === 'boolean') {
        hostAudioState.restoreOnExit = restoreOnExit;
      }
      syncPrivacyAndHostAudioUi();
      if (IS_TAURI && typeof Bridge.setSessionHostMute === 'function') {
        Bridge.setSessionHostMute(
          hostAudioState.muted,
          hostAudioState.restoreOnExit,
          persist !== false
        )
          .then(function () {
            if (customSuccessToast) {
              toast(customSuccessToast);
            } else if (showToast) {
              toast(hostAudioState.muted ? '已开启被控端静音运行' : '已恢复被控端本机外放');
            }
          })
          .catch(function (err) {
            hostAudioState.muted = prevMuted;
            hostAudioState.restoreOnExit = prevRestore;
            syncPrivacyAndHostAudioUi();
            toast('设置被控端静音失败：' + String(err || '未知错误'));
          });
      } else if (customSuccessToast) {
        toast(customSuccessToast);
      } else if (showToast) {
        toast(hostAudioState.muted ? '已开启被控端静音运行' : '已恢复被控端本机外放');
      }
    }

    function syncPrivacyModalUi() {
      if (!privacyModalBackdrop) return;
      var sel = privacyState.draftPreset || privacyState.preset || 'preset-1';
      var customUrl = privacyState.draftCustomDataUrl || privacyState.customDataUrl || '';
      if (privacyCustomCard) {
        privacyCustomCard.classList.toggle('is-selected', sel === 'custom' && Boolean(customUrl));
      }
      if (privacyCustomImg && privacyCustomEmpty && privacyLastUsedBadge) {
        if (customUrl) {
          privacyCustomImg.src = customUrl;
          privacyCustomImg.hidden = false;
          privacyCustomEmpty.hidden = true;
          privacyLastUsedBadge.hidden = false;
        } else {
          privacyCustomImg.hidden = true;
          privacyCustomEmpty.hidden = false;
          privacyLastUsedBadge.hidden = true;
        }
      }
      if (privacyPresetsGrid) {
        Array.prototype.forEach.call(
          privacyPresetsGrid.querySelectorAll('.sn-privacy-tile'),
          function (tile) {
            tile.classList.toggle('is-selected', tile.getAttribute('data-preset') === sel);
          }
        );
      }
    }

    function openPrivacyModal() {
      closePopover();
      privacyState.draftPreset = privacyState.preset || 'preset-1';
      privacyState.draftCustomDataUrl = privacyState.customDataUrl || '';
      syncPrivacyModalUi();
      if (privacyModalBackdrop) privacyModalBackdrop.hidden = false;
    }

    function closePrivacyModal() {
      if (privacyModalBackdrop) privacyModalBackdrop.hidden = true;
    }

    function openPrivacyPreview() {
      if (!privacyPreviewOverlay || !privacyPreviewSurface) return;
      var sel = normalizePrivacyPreset(privacyState.draftPreset || 'preset-1');
      var customUrl = privacyState.draftCustomDataUrl || privacyState.customDataUrl || '';
      privacyPreviewSurface.className = 'sn-privacy-preview-card';
      if (sel === 'custom' && customUrl) {
        if (privacyPreviewImg) {
          privacyPreviewImg.src = customUrl;
          privacyPreviewImg.hidden = false;
        }
        if (privacyPreviewCaption) privacyPreviewCaption.hidden = true;
      } else {
        if (privacyPreviewImg) privacyPreviewImg.hidden = true;
        var artIdx = sel.replace('preset-', '');
        privacyPreviewSurface.classList.add('sn-privacy-art--' + (artIdx || '1'));
        if (privacyPreviewCaption) {
          privacyPreviewCaption.hidden = false;
          var h3 = privacyPreviewCaption.querySelector('h3');
          if (h3) h3.textContent = PRESET_LABELS[sel] || '远程隐私保护中';
        }
      }
      privacyPreviewOverlay.hidden = false;
    }

    function closePrivacyPreview() {
      if (privacyPreviewOverlay) privacyPreviewOverlay.hidden = true;
    }

    function compressImageFileToDataUrl(file, cb) {
      var reader = new FileReader();
      reader.onload = function () {
        var img = new Image();
        img.onload = function () {
          var maxW = 1280;
          var maxH = 720;
          var scale = Math.min(1, maxW / Math.max(1, img.width), maxH / Math.max(1, img.height));
          var w = Math.max(2, Math.round(img.width * scale));
          var h = Math.max(2, Math.round(img.height * scale));
          var canvas = document.createElement('canvas');
          canvas.width = w;
          canvas.height = h;
          var ctx = canvas.getContext('2d');
          ctx.drawImage(img, 0, 0, w, h);
          var quality = 0.78;
          var out = canvas.toDataURL('image/jpeg', quality);
          // 控制端显示控制单帧上限 256 KiB：压缩 base64 至 <= 220 KiB 确保可靠送达。
          while (out.length > 220000 && quality > 0.25) {
            quality -= 0.1;
            out = canvas.toDataURL('image/jpeg', quality);
          }
          cb(null, out);
        };
        img.onerror = function () {
          cb('无法解析所选图片文件');
        };
        img.src = String(reader.result || '');
      };
      reader.onerror = function () {
        cb('读取图片文件失败');
      };
      reader.readAsDataURL(file);
    }

    function syncPeripheralsUi() {
      if (optSendSysShortcutsBtn) {
        var on = Boolean(peripheralsState.sendSystemShortcuts);
        optSendSysShortcutsBtn.setAttribute('aria-checked', on ? 'true' : 'false');
        var ck = optSendSysShortcutsBtn.querySelector('.sn-check');
        if (ck) ck.hidden = !on;
      }
      var mm = peripheralsState.mouseMode || 'smart';
      if (menuMouseModeValEl) {
        menuMouseModeValEl.textContent = MOUSE_MODE_LABELS[mm] || '智能鼠标';
      }
      if (mouseModeMenuEl) {
        Array.prototype.forEach.call(
          mouseModeMenuEl.querySelectorAll('[data-mouse-mode]'),
          function (btn) {
            var checked = btn.getAttribute('data-mouse-mode') === mm;
            btn.setAttribute('aria-checked', checked ? 'true' : 'false');
            var ck = btn.querySelector('.sn-check');
            if (ck) ck.hidden = !checked;
          }
        );
      }
      stage.classList.toggle('is-remote-cursor', mm === 'remote');
    }

    function applyMouseMode(mode, persist, showToast) {
      var prevMode = peripheralsState.mouseMode;
      var norm = mode === 'remote' || mode === 'local' ? mode : 'smart';
      peripheralsState.mouseMode = norm;
      syncPeripheralsUi();
      if (IS_TAURI && typeof Bridge.setSessionPeripherals === 'function') {
        Bridge.setSessionPeripherals({
          mouseMode: norm,
          persist: persist !== false
        })
          .then(function () {
            if (showToast) {
              toast('鼠标模式已切换为：' + (MOUSE_MODE_LABELS[norm] || '智能鼠标'));
            }
          })
          .catch(function (err) {
            peripheralsState.mouseMode = prevMode;
            syncPeripheralsUi();
            toast('切换鼠标模式失败：' + String(err || '未知错误'));
          });
      } else if (showToast) {
        toast('鼠标模式已切换为：' + (MOUSE_MODE_LABELS[norm] || '智能鼠标'));
      }
    }

    function applySystemShortcuts(enabled, exceptions, persist, showToast, customSuccessToast) {
      var prevEnabled = peripheralsState.sendSystemShortcuts;
      var prevExceptions = peripheralsState.shortcutExceptions.slice();
      peripheralsState.sendSystemShortcuts = Boolean(enabled);
      if (Array.isArray(exceptions)) {
        peripheralsState.shortcutExceptions = exceptions.slice();
      }
      syncPeripheralsUi();
      if (IS_TAURI && typeof Bridge.setSessionPeripherals === 'function') {
        Bridge.setSessionPeripherals({
          sendSystemShortcuts: peripheralsState.sendSystemShortcuts,
          shortcutExceptions: peripheralsState.shortcutExceptions,
          persist: persist !== false
        })
          .then(function () {
            if (customSuccessToast) {
              toast(customSuccessToast);
            } else if (showToast) {
              toast(
                peripheralsState.sendSystemShortcuts
                  ? '已开启发送系统快捷键到被控端'
                  : '已关闭发送系统快捷键到被控端'
              );
            }
          })
          .catch(function (err) {
            peripheralsState.sendSystemShortcuts = prevEnabled;
            peripheralsState.shortcutExceptions = prevExceptions;
            syncPeripheralsUi();
            toast('系统快捷键设置失败：' + String(err || '请检查辅助功能权限'));
          });
      } else if (customSuccessToast) {
        toast(customSuccessToast);
      } else if (showToast) {
        toast(
          peripheralsState.sendSystemShortcuts
            ? '已开启发送系统快捷键到被控端（预览）'
            : '已关闭发送系统快捷键到被控端（预览）'
        );
      }
    }

    function openShortcutsModal() {
      closePopover();
      if (!shortcutsModalBackdrop || !shortcutsListEl) return;
      shortcutsListEl.textContent = '';
      var excSet = Object.create(null);
      peripheralsState.shortcutExceptions.forEach(function (id) {
        excSet[String(id).toLowerCase()] = true;
      });
      SHORTCUT_CATALOG.forEach(function (item) {
        var row = document.createElement('label');
        row.className = 'sn-shortcut-row';
        var left = document.createElement('span');
        left.className = 'sn-shortcut-row__left';
        var cb = document.createElement('input');
        cb.type = 'checkbox';
        cb.value = item.id;
        cb.checked = Boolean(excSet[item.id]);
        var lbl = document.createElement('span');
        lbl.textContent = item.label;
        left.appendChild(cb);
        left.appendChild(lbl);
        var keyEl = document.createElement('span');
        keyEl.className = 'sn-shortcut-row__key';
        keyEl.textContent = item.key;
        row.appendChild(left);
        row.appendChild(keyEl);
        shortcutsListEl.appendChild(row);
      });
      shortcutsModalBackdrop.hidden = false;
    }

    function closeShortcutsModal() {
      if (shortcutsModalBackdrop) shortcutsModalBackdrop.hidden = true;
    }

    function renderDisplayTabs() {
      var list = Array.isArray(displaysState.displays) ? displaysState.displays : [];
      var canCycle = list.length > 1;
      if (nextScreenMenuBtn) {
        nextScreenMenuBtn.setAttribute('aria-disabled', canCycle ? 'false' : 'true');
        nextScreenMenuBtn.title = canCycle
          ? '切换下一个屏幕 (⌥⌘N)'
          : '当前仅检测到 1 个显示器（可点击顶部 + 创建虚拟屏）';
      }
      if (!screenTabsEl || !list.length) return;
      screenTabsEl.textContent = '';
      var activeId = displaysState.activeDisplayId || list[0].id;
      list.forEach(function (disp) {
        var isActive = disp.id === activeId;
        var tab = document.createElement('div');
        tab.className = 'sn-screen-tab' + (isActive ? ' is-active' : '');
        tab.setAttribute('data-display-id', String(disp.id));
        tab.title = disp.name + (isActive ? ' — 右键切换分辨率' : ' — 点击切换至该屏');
        tab.appendChild(createSvgIcon('i-monitor', 16));
        var txt = document.createElement('span');
        txt.className = 'sn-screen-tab__text';
        txt.textContent = disp.name;
        tab.appendChild(txt);

        if (disp.is_virtual) {
          var rmBtn = document.createElement('button');
          rmBtn.type = 'button';
          rmBtn.className = 'sn-screen-tab__close';
          rmBtn.title = '移除此虚拟屏';
          rmBtn.setAttribute('aria-label', '移除虚拟屏');
          rmBtn.textContent = '×';
          rmBtn.addEventListener('click', function (ev) {
            ev.stopPropagation();
            if (IS_TAURI && typeof Bridge.removeSessionVirtualDisplay === 'function') {
              Bridge.removeSessionVirtualDisplay(disp.id)
                .then(function () {
                  toast('正在移除虚拟屏…');
                })
                .catch(function (err) {
                  toast('移除虚拟屏失败：' + String(err || '未知错误'));
                });
            }
          });
          tab.appendChild(rmBtn);
        }

        tab.addEventListener('click', function () {
          if (isActive) {
            if (open && open.el === screenResMenuEl) closePopover();
            return;
          }
          if (IS_TAURI && typeof Bridge.switchSessionDisplay === 'function') {
            Bridge.switchSessionDisplay(disp.id)
              .then(function () {
                toast('正在切换至 ' + disp.name + '…');
              })
              .catch(function (err) {
                toast('切换屏幕失败：' + String(err || '未知错误'));
              });
          } else {
            displaysState.activeDisplayId = disp.id;
            renderDisplayTabs();
            toast('已切换至 ' + disp.name);
          }
        });

        tab.addEventListener('contextmenu', function (ev) {
          ev.preventDefault();
          ev.stopPropagation();
          if (isActive && screenResMenuEl) {
            openPopover(screenResMenuEl, tab, 'left');
          }
        });

        screenTabsEl.appendChild(tab);
        if (isActive && screenItem) {
          var lbl = screenItem.querySelector('.sn-menu__label');
          if (lbl) lbl.textContent = disp.name;
        }
      });
    }

    function switchToNextDisplay() {
      var list = Array.isArray(displaysState.displays) ? displaysState.displays : [];
      if (list.length < 2) {
        toast('当前仅检测到 1 个显示器，可点击顶部「+」创建虚拟屏');
        return;
      }
      var curIdx = 0;
      for (var i = 0; i < list.length; i += 1) {
        if (list[i].id === displaysState.activeDisplayId) {
          curIdx = i;
          break;
        }
      }
      var next = list[(curIdx + 1) % list.length];
      if (!next) return;
      if (IS_TAURI && typeof Bridge.switchSessionDisplay === 'function') {
        Bridge.switchSessionDisplay(next.id)
          .then(function () {
            toast('已切换至 ' + next.name);
          })
          .catch(function (err) {
            toast('切换屏幕失败：' + String(err || '未知错误'));
          });
      } else {
        displaysState.activeDisplayId = next.id;
        renderDisplayTabs();
        toast('已切换至 ' + next.name);
      }
    }

    function applyLockOnExit(enabled, persist, showToast) {
      var on = Boolean(enabled);
      if (secLock) secLock.checked = on;
      syncSecurityState();
      if (persist && IS_TAURI && typeof Bridge.saveStreamPrefs === 'function') {
        Bridge.saveStreamPrefs({ lockOnExit: on }).catch(function () {});
      }
      if (showToast) {
        toast(on ? '已开启远程结束后被控端自动锁屏' : '已关闭远程结束后被控端自动锁屏');
      }
    }

    function lockRemoteScreenNow() {
      if (watchMode) {
        toast('观看模式下无法锁定被控端');
        return false;
      }
      if (!IS_TAURI || !realSessionActive) {
        toast('锁屏指令将在远控连接后生效');
        return false;
      }
      Bridge.sessionCommand({ type: 'lock_remote_screen' }).catch(function () {});
      toast('已发送锁屏指令至被控端');
      return true;
    }

    function applyRemoteAudioMuted(muted, showToast) {
      remoteAudioMuted = Boolean(muted);
      syncSecurityState();
      if (IS_TAURI && typeof Bridge.sessionCommand === 'function') {
        Bridge.sessionCommand({
          type: 'set_audio_muted',
          muted: remoteAudioMuted
        }).catch(function () {});
      }
      if (showToast) {
        toast(remoteAudioMuted ? '已暂停播放远端声音' : '已恢复播放远端声音');
      }
    }

    function sendKeyCombo(modCodes, keyCode) {
      if (watchMode) {
        toast('观看模式下无法发送快捷键');
        return false;
      }
      if (!IS_TAURI || !realSessionActive) {
        toast('快捷键将在远控连接后生效');
        return false;
      }
      var mods = Array.isArray(modCodes) ? modCodes : [];
      var flags = { ctrl: false, shift: false, alt: false, meta: false };
      function setFlag(code, on) {
        if (code === 'ControlLeft' || code === 'ControlRight') flags.ctrl = on;
        if (code === 'ShiftLeft' || code === 'ShiftRight') flags.shift = on;
        if (code === 'AltLeft' || code === 'AltRight') flags.alt = on;
        if (code === 'MetaLeft' || code === 'MetaRight') flags.meta = on;
      }
      function sendKey(code, pressed) {
        Bridge.sendInputEvent({
          kind: 'key',
          code: code,
          pressed: pressed,
          ctrl: flags.ctrl,
          shift: flags.shift,
          alt: flags.alt,
          meta: flags.meta
        }).catch(function () {});
      }
      mods.forEach(function (mc) {
        setFlag(mc, true);
        sendKey(mc, true);
      });
      sendKey(keyCode, true);
      sendKey(keyCode, false);
      for (var i = mods.length - 1; i >= 0; i--) {
        setFlag(mods[i], false);
        sendKey(mods[i], false);
      }
      return true;
    }

    if (secPrivacy && secMute && secLock) {
      secPrivacy.addEventListener('change', function () {
        applyPrivacyMode(secPrivacy.value, privacyState.preset, null, true, true);
      });
      secMute.addEventListener('change', function () {
        applyHostSystemMute(Boolean(secMute.checked), hostAudioState.restoreOnExit, true, true);
      });
      secLock.addEventListener('change', function () {
        applyLockOnExit(Boolean(secLock.checked), true, IS_TAURI);
      });
      syncPrivacyAndHostAudioUi();
      syncPeripheralsUi();
    }

    if (hostMuteBtn) {
      hostMuteBtn.addEventListener('click', function (e) {
        e.preventDefault();
        applyHostSystemMute(!hostAudioState.muted, hostAudioState.restoreOnExit, true, true);
        closePopover();
      });
    }
    if (hostRestoreAudioBtn) {
      hostRestoreAudioBtn.addEventListener('click', function (e) {
        e.preventDefault();
        var nextRestore = !hostAudioState.restoreOnExit;
        applyHostSystemMute(
          hostAudioState.muted,
          nextRestore,
          true,
          false,
          nextRestore ? '已开启：远程结束后恢复被控端声音设置' : '已关闭：远程结束后不再恢复声音设置'
        );
        closePopover();
      });
    }
    if (privacyModeMenuItem && privacyModeMenuEl) {
      var openPrivSub = function () {
        cancelNestedClose();
        openNestedMenu(privacyModeMenuItem, privacyModeMenuEl);
      };
      privacyModeMenuItem.addEventListener('pointerenter', openPrivSub);
      privacyModeMenuItem.addEventListener('focus', openPrivSub);
      privacyModeMenuItem.addEventListener('click', function (e) {
        e.preventDefault();
        openPrivSub();
      });
      privacyModeMenuEl.addEventListener('pointerenter', cancelNestedClose);
      Array.prototype.forEach.call(
        privacyModeMenuEl.querySelectorAll('[data-privacy-opt]'),
        function (btn) {
          btn.addEventListener('click', function (e) {
            e.preventDefault();
            var modeVal = btn.getAttribute('data-privacy-opt') || 'off';
            applyPrivacyMode(modeVal, privacyState.preset, null, true, true);
            closePopover();
          });
        }
      );
    }
    if (openPrivacyModalBtn) {
      openPrivacyModalBtn.addEventListener('click', function (e) {
        e.preventDefault();
        openPrivacyModal();
      });
    }
    if (privacyModalCloseBtn) privacyModalCloseBtn.addEventListener('click', closePrivacyModal);
    if (privacyCancelBtn) privacyCancelBtn.addEventListener('click', closePrivacyModal);
    if (privacyModalBackdrop) {
      privacyModalBackdrop.addEventListener('click', function (e) {
        if (e.target === privacyModalBackdrop) closePrivacyModal();
      });
    }
    if (privacyUploadBtn && privacyFileInput) {
      privacyUploadBtn.addEventListener('click', function () {
        privacyFileInput.value = '';
        privacyFileInput.click();
      });
      privacyFileInput.addEventListener('change', function () {
        var file = privacyFileInput.files && privacyFileInput.files[0];
        if (!file) return;
        compressImageFileToDataUrl(file, function (err, dataUrl) {
          if (err || !dataUrl) {
            toast(err || '处理图片失败');
            return;
          }
          privacyState.draftCustomDataUrl = dataUrl;
          privacyState.draftPreset = 'custom';
          syncPrivacyModalUi();
          if (IS_TAURI && typeof Bridge.savePrivacyCustomImage === 'function') {
            Bridge.savePrivacyCustomImage(dataUrl)
              .then(function (savedUrl) {
                if (savedUrl) {
                  privacyState.customDataUrl = savedUrl;
                  privacyState.draftCustomDataUrl = savedUrl;
                }
                syncPrivacyModalUi();
                toast('自定义屏保图片已保存');
              })
              .catch(function (saveErr) {
                toast('保存自定义图片失败：' + String(saveErr || '未知错误'));
              });
          }
        });
      });
    }
    if (privacyCustomCard) {
      privacyCustomCard.addEventListener('click', function () {
        if (privacyState.draftCustomDataUrl || privacyState.customDataUrl) {
          privacyState.draftPreset = 'custom';
          syncPrivacyModalUi();
        } else if (privacyFileInput) {
          privacyFileInput.value = '';
          privacyFileInput.click();
        }
      });
    }
    if (privacyPresetsGrid) {
      Array.prototype.forEach.call(
        privacyPresetsGrid.querySelectorAll('.sn-privacy-tile'),
        function (tile) {
          tile.addEventListener('click', function () {
            privacyState.draftPreset = tile.getAttribute('data-preset') || 'preset-1';
            syncPrivacyModalUi();
          });
        }
      );
    }
    if (privacyPreviewBtn) privacyPreviewBtn.addEventListener('click', openPrivacyPreview);
    if (privacyPreviewCloseBtn) privacyPreviewCloseBtn.addEventListener('click', closePrivacyPreview);
    if (privacyPreviewOverlay) {
      privacyPreviewOverlay.addEventListener('click', function (e) {
        if (e.target === privacyPreviewOverlay) closePrivacyPreview();
      });
    }
    if (privacyConfirmBtn) {
      privacyConfirmBtn.addEventListener('click', function () {
        var sel = privacyState.draftPreset || 'preset-1';
        var customUrl = privacyState.draftCustomDataUrl || privacyState.customDataUrl || '';
        if (sel === 'custom' && !customUrl) {
          toast('请先选择自定义屏保图片，或选择一个内置预设');
          return;
        }
        closePrivacyModal();
        applyPrivacyMode('privacy', sel, sel === 'custom' ? customUrl : null, true, true);
      });
    }

    if (optSendSysShortcutsBtn) {
      optSendSysShortcutsBtn.addEventListener('click', function (e) {
        e.preventDefault();
        applySystemShortcuts(
          !peripheralsState.sendSystemShortcuts,
          peripheralsState.shortcutExceptions,
          true,
          true
        );
        closePopover();
      });
    }
    if (btnConfigShortcuts) {
      btnConfigShortcuts.addEventListener('click', function (e) {
        e.preventDefault();
        openShortcutsModal();
      });
    }
    if (shortcutsModalCloseBtn) shortcutsModalCloseBtn.addEventListener('click', closeShortcutsModal);
    if (shortcutsCancelBtn) shortcutsCancelBtn.addEventListener('click', closeShortcutsModal);
    if (shortcutsModalBackdrop) {
      shortcutsModalBackdrop.addEventListener('click', function (e) {
        if (e.target === shortcutsModalBackdrop) closeShortcutsModal();
      });
    }
    if (shortcutsResetBtn && shortcutsListEl) {
      shortcutsResetBtn.addEventListener('click', function () {
        Array.prototype.forEach.call(
          shortcutsListEl.querySelectorAll('input[type="checkbox"]'),
          function (cb) {
            cb.checked = false;
          }
        );
      });
    }
    if (shortcutsSaveBtn && shortcutsListEl) {
      shortcutsSaveBtn.addEventListener('click', function () {
        var nextExc = [];
        Array.prototype.forEach.call(
          shortcutsListEl.querySelectorAll('input[type="checkbox"]'),
          function (cb) {
            if (cb.checked && cb.value) nextExc.push(cb.value);
          }
        );
        closeShortcutsModal();
        applySystemShortcuts(
          peripheralsState.sendSystemShortcuts,
          nextExc,
          true,
          false,
          '已保存快捷键例外配置（本机保留 ' + nextExc.length + ' 项）'
        );
      });
    }
    if (securitySubEl) {
      Array.prototype.forEach.call(
        securitySubEl.querySelectorAll('.sn-submenu__item, .sn-submenu__row'),
        function (mi) {
          if (mi !== privacyModeMenuItem) {
            mi.addEventListener('pointerenter', scheduleNestedClose);
          }
        }
      );
    }
    if (peripheralsEl) {
      Array.prototype.forEach.call(
        peripheralsEl.querySelectorAll('.sn-submenu__item, .sn-submenu__row'),
        function (mi) {
          if (mi !== menuMouseModeItem) {
            mi.addEventListener('pointerenter', scheduleNestedClose);
          }
        }
      );
    }
    if (menuMouseModeItem && mouseModeMenuEl) {
      var openMouseSub = function () {
        cancelNestedClose();
        openNestedMenu(menuMouseModeItem, mouseModeMenuEl);
      };
      menuMouseModeItem.addEventListener('pointerenter', openMouseSub);
      menuMouseModeItem.addEventListener('focus', openMouseSub);
      menuMouseModeItem.addEventListener('click', function (e) {
        e.preventDefault();
        openMouseSub();
      });
      mouseModeMenuEl.addEventListener('pointerenter', cancelNestedClose);
      Array.prototype.forEach.call(
        mouseModeMenuEl.querySelectorAll('[data-mouse-mode]'),
        function (btn) {
          btn.addEventListener('click', function (e) {
            e.preventDefault();
            var mode = btn.getAttribute('data-mouse-mode') || 'smart';
            applyMouseMode(mode, true, true);
            closePopover();
          });
        }
      );
    }

    if (addScreenBtn) {
      addScreenBtn.addEventListener('click', function (e) {
        e.preventDefault();
        if (watchMode) {
          toast('观看模式下无法创建虚拟屏');
          return;
        }
        if (IS_TAURI && typeof Bridge.createSessionVirtualDisplay === 'function') {
          addScreenBtn.disabled = true;
          toast('正在创建虚拟屏…');
          Bridge.createSessionVirtualDisplay(1920, 1080, true)
            .catch(function (err) {
              toast('创建虚拟屏失败：' + String(err || '未知错误'));
            })
            .then(function () {
              window.setTimeout(function () {
                addScreenBtn.disabled = false;
              }, 600);
            });
        } else {
          var nextId = 100 + displaysState.displays.length + 1;
          if (!displaysState.displays.length) {
            displaysState.displays.push({
              id: 1,
              name: MOCK.screenName,
              width: 1512,
              height: 982,
              is_main: true,
              is_virtual: false
            });
          }
          displaysState.displays.push({
            id: nextId,
            name: '虚拟显示屏 ' + displaysState.displays.length,
            width: 1920,
            height: 1080,
            is_main: false,
            is_virtual: true
          });
          displaysState.activeDisplayId = nextId;
          renderDisplayTabs();
          toast('已创建虚拟屏（预览）');
        }
      });
    }

    if (shieldBtn) {
      shieldBtn.addEventListener('click', function (e) {
        e.preventDefault();
        openPopover(securityEl, shieldBtn, 'center');
      });
    }

    /* ── 显示屏选项卡右键分辨率菜单（Item #5）──────────────────── */
    if (screenTabEl && screenResMenuEl) {
      screenTabEl.addEventListener('contextmenu', function (e) {
        e.preventDefault();
        e.stopPropagation();
        openPopover(screenResMenuEl, screenTabEl, 'left');
      });
      screenTabEl.addEventListener('click', function () {
        if (open && open.el === screenResMenuEl) {
          closePopover();
        }
      });
    }

    if (fitWindowBtn) {
      fitWindowBtn.addEventListener('click', handleFitSessionWindow);
    }
    if (tabFitWindowBtn) {
      tabFitWindowBtn.addEventListener('click', handleFitSessionWindow);
    }
    if (windowFullscreenBtn) {
      windowFullscreenBtn.addEventListener('click', function (e) {
        e.preventDefault();
        closePopover();
        toggleFullscreen();
      });
    }

    /* ── S3 控制中心菜单 ───────────────────────────────────────── */
    var submenuAnchorItem = null;
    var activeSubmenuEl = null;
    var nestedAnchorItem = null;
    var activeNestedEl = null;
    var nestedCloseTimer = null;

    function cancelNestedClose() {
      if (nestedCloseTimer !== null) {
        window.clearTimeout(nestedCloseTimer);
        nestedCloseTimer = null;
      }
    }

    function closeNestedMenu() {
      cancelNestedClose();
      if (activeNestedEl) {
        activeNestedEl.hidden = true;
        activeNestedEl = null;
      }
      if (screenMoreEl) screenMoreEl.hidden = true;
      if (privacyModeMenuEl) privacyModeMenuEl.hidden = true;
      if (mouseModeMenuEl) mouseModeMenuEl.hidden = true;
      if (nestedAnchorItem) {
        nestedAnchorItem.classList.remove('is-open');
        nestedAnchorItem.setAttribute('aria-expanded', 'false');
      }
      nestedAnchorItem = null;
    }

    function scheduleNestedClose() {
      if (!activeNestedEl || activeNestedEl.hidden) return;
      cancelNestedClose();
      nestedCloseTimer = window.setTimeout(function () {
        nestedCloseTimer = null;
        closeNestedMenu();
      }, 180);
    }

    if (screenMoreEl) {
      screenMoreEl.addEventListener('pointerenter', cancelNestedClose);
    }

    function closeSubmenu() {
      closeNestedMenu();
      if (activeSubmenuEl) {
        activeSubmenuEl.hidden = true;
        activeSubmenuEl = null;
      }
      if (screenEl) screenEl.hidden = true;
      if (qualityEl) qualityEl.hidden = true;
      if (windowEl) windowEl.hidden = true;
      if (soundEl) soundEl.hidden = true;
      if (securitySubEl) securitySubEl.hidden = true;
      if (peripheralsEl) peripheralsEl.hidden = true;
      if (toolsEl) toolsEl.hidden = true;
      if (moreEl) moreEl.hidden = true;
      if (submenuAnchorItem) {
        submenuAnchorItem.classList.remove('is-open');
        submenuAnchorItem.setAttribute('aria-expanded', 'false');
      }
      submenuAnchorItem = null;
    }

    /* 子菜单贴父菜单侧边展开（§4 NativeMenu，支持二/三级菜单） */
    function placeFloatingMenu(item, subEl, parentEl) {
      var hostEl = parentEl || menuEl;
      if (!item || !subEl || !hostEl) return;
      var parentRect = hostEl.getBoundingClientRect();
      var itemRect = item.getBoundingClientRect();
      var bounds = win.getBoundingClientRect();
      var w = subEl.offsetWidth;
      var h = subEl.offsetHeight;
      var margin = 8;

      var left = parentRect.right - 2;
      if (left + w > bounds.right - margin) left = parentRect.left - w + 2;
      left = Math.max(bounds.left + margin, left);
      var top = Math.max(bounds.top + margin, Math.min(itemRect.top, bounds.bottom - h - margin));

      subEl.style.left = Math.round(left) + 'px';
      subEl.style.top = Math.round(top) + 'px';
    }

    function placeSubmenu(item, subEl) {
      placeFloatingMenu(item, subEl, menuEl);
    }

    function openNestedMenu(item, targetNested) {
      var nestEl = targetNested || screenMoreEl;
      if (!nestEl || !item) return;
      var parentEl = item.closest('.sn-popover') || activeSubmenuEl || (open && open.el);
      if (nestedAnchorItem === item && activeNestedEl === nestEl && !nestEl.hidden) {
        placeFloatingMenu(item, nestEl, parentEl);
        return;
      }
      closeNestedMenu();
      nestedAnchorItem = item;
      activeNestedEl = nestEl;
      nestEl.hidden = false;
      item.classList.add('is-open');
      item.setAttribute('aria-expanded', 'true');
      placeFloatingMenu(item, nestEl, parentEl);
    }

    function openSubmenu(item, targetSubmenu) {
      var subEl = targetSubmenu || qualityEl;
      if (!subEl) return;
      if (submenuAnchorItem === item && activeSubmenuEl === subEl && !subEl.hidden) {
        placeSubmenu(item, subEl);
        return;
      }
      closeSubmenu();
      submenuAnchorItem = item;
      activeSubmenuEl = subEl;
      subEl.hidden = false;
      item.classList.add('is-open');
      item.setAttribute('aria-expanded', 'true');
      placeSubmenu(item, subEl);
    }

    if (ctrlBtn) {
      ctrlBtn.addEventListener('click', function (e) {
        e.preventDefault();
        openPopover(menuEl, ctrlBtn, 'right');
      });
    }

    if (menuEl) {
      var items = Array.prototype.slice.call(menuEl.querySelectorAll('.sn-menu__item'));
      var submenuMap = {
        screen: screenEl,
        quality: qualityEl,
        window: windowEl,
        sound: soundEl,
        security: securitySubEl,
        peripherals: peripheralsEl,
        tools: toolsEl,
        more: moreEl
      };

      items.forEach(function (item) {
        var subKind = item.getAttribute('data-submenu');
        if (subKind && submenuMap[subKind]) {
          var subTarget = submenuMap[subKind];
          item.addEventListener('pointerenter', function () {
            openSubmenu(item, subTarget);
          });
          item.addEventListener('focus', function () {
            openSubmenu(item, subTarget);
          });
          item.addEventListener('click', function (e) {
            e.preventDefault();
            openSubmenu(item, subTarget);
          });
          return;
        }

        item.addEventListener('pointerenter', closeSubmenu);
        item.addEventListener('click', function (e) {
          e.preventDefault();
          var action = item.getAttribute('data-action');
          if (action === 'clipboard') {
            clipboardEnabled = !clipboardEnabled;
            syncClipboardMenuUi();
            if (IS_TAURI) {
              Bridge.sessionCommand({
                type: 'set_clipboard_enabled',
                enabled: clipboardEnabled
              }).catch(function () {});
            }
            toast(clipboardEnabled ? '已开启剪贴板同步' : '已关闭剪贴板同步');
            closePopover();
            return;
          }
          if (action === 'fullscreen') {
            closePopover();
            toggleFullscreen();
            return;
          }
          if (action === 'exit') {
            exitSession();
            return;
          }
          if (action === 'next-screen') {
            closePopover();
            switchToNextDisplay();
          }
        });
      });
    }

    if (soundPlayBtn) {
      soundPlayBtn.addEventListener('click', function (e) {
        e.preventDefault();
        var nextPlay = soundPlayBtn.getAttribute('aria-checked') !== 'true';
        applyRemoteAudioMuted(!nextPlay, true);
        closePopover();
      });
    }

    if (watchOnlyMenuBtn) {
      watchOnlyMenuBtn.addEventListener('click', function (e) {
        e.preventDefault();
        var nextWatch = !watchMode;
        if (IS_TAURI && typeof Bridge.setSessionWatchOnly === 'function') {
          Bridge.setSessionWatchOnly(nextWatch)
            .then(function () {
              setWatchMode(nextWatch);
              toast(nextWatch ? '已开启观看模式（禁止操作）' : '已恢复远程操作控制');
            })
            .catch(function (err) {
              toast('切换观看模式失败：' + String(err || '未知错误'));
            });
        } else {
          setWatchMode(nextWatch);
          toast(nextWatch ? '已开启观看模式（预览）' : '已恢复远程操作控制（预览）');
        }
        closePopover();
      });
    }

    if (lockNowBtn) {
      lockNowBtn.addEventListener('click', function (e) {
        e.preventDefault();
        closePopover();
        lockRemoteScreenNow();
      });
    }

    if (autoLockExitBtn && secLock) {
      autoLockExitBtn.addEventListener('click', function (e) {
        e.preventDefault();
        applyLockOnExit(!Boolean(secLock.checked), true, true);
        closePopover();
      });
    }

    if (mtlsSwitchEl) {
      mtlsSwitchEl.addEventListener('change', function () {
        var enabled = Boolean(mtlsSwitchEl.checked);
        var previewMsg = enabled
          ? '已开启 mTLS 双向加密握手'
          : '已关闭端到端 mTLS 加密（中转外层仍受 mTLS 保护）';
        if (!IS_TAURI || typeof Bridge.setSessionMtlsMode !== 'function') {
          linkSecurityState.encryptConnection = enabled;
          syncLinkSecurityUi();
          toast(previewMsg);
          return;
        }
        mtlsSwitchEl.disabled = true;
        Bridge.setSessionMtlsMode(enabled)
          .then(function () {
            linkSecurityState.encryptConnection = enabled;
            toast(enabled ? '正在重新协商 mTLS 加密连接…' : '正在切换为标准连接（中转外层仍受 mTLS 保护）…');
          })
          .catch(function (err) {
            mtlsSwitchEl.checked = !enabled;
            toast('切换 mTLS 模式失败：' + String(err || '未知错误'));
          })
          .then(function () {
            mtlsSwitchEl.disabled = false;
          });
      });
    }

    if (peripheralsEl) {
      var COMBO_MAP = {
        spotlight: { mods: ['MetaLeft'], key: 'Space', label: 'Spotlight (⌘Space)' },
        'switch-app': { mods: ['MetaLeft'], key: 'Tab', label: '切换应用 (⌘Tab)' },
        'force-quit': { mods: ['AltLeft', 'MetaLeft'], key: 'Escape', label: '强制退出应用 (⌥⌘Esc)' },
        screenshot: { mods: ['ShiftLeft', 'MetaLeft'], key: 'Digit3', label: '截屏 (⇧⌘3)' }
      };
      Array.prototype.forEach.call(
        peripheralsEl.querySelectorAll('[data-combo]'),
        function (btn) {
          btn.addEventListener('click', function (e) {
            e.preventDefault();
            var kind = btn.getAttribute('data-combo');
            closePopover();
            if (kind === 'lock-screen') {
              lockRemoteScreenNow();
              return;
            }
            var spec = COMBO_MAP[kind];
            if (!spec) return;
            if (sendKeyCombo(spec.mods, spec.key)) {
              toast('已发送快捷键：' + spec.label);
            }
          });
        }
      );
    }

    if (toolsEl) {
      var toolBtns = Array.prototype.slice.call(toolsEl.querySelectorAll('[data-tool]'));
      toolBtns.forEach(function (btn) {
        btn.addEventListener('click', function (e) {
          e.preventDefault();
          var kind = btn.getAttribute('data-tool');
          closePopover();
          if (IS_TAURI && typeof Bridge.openToolWindow === 'function') {
            Bridge.openToolWindow(kind).catch(function (err) {
              toast('打开工具窗口失败：' + String(err || '未知错误'));
            });
            return;
          }
          if (kind === 'transfer' || kind === 'terminal' || kind === 'portmap') {
            window.open(kind + '.html', '_blank', 'noopener');
          }
        });
      });
    }

    if (qualityEl) {
      var options = Array.prototype.slice.call(qualityEl.querySelectorAll('.sn-submenu__item'));
      options.forEach(function (opt) {
        opt.addEventListener('click', function () {
          var group = opt.getAttribute('data-group');
          var val = opt.getAttribute('data-value');
          if (opt.getAttribute('aria-disabled') === 'true') {
            if (group === 'codec' && val === 'hevc') {
              toast('当前主机或本机解码端暂不支持 HEVC 硬件编解码');
            }
            return;
          }
          options.forEach(function (other) {
            if (other.getAttribute('data-group') !== group) return;
            var checked = other === opt;
            other.setAttribute('aria-checked', checked ? 'true' : 'false');
            var check = other.querySelector('.sn-check');
            if (check) check.hidden = !checked;
          });
          if (group === 'codec') {
            var targetCodec = normalizeCodec(val) || 'h264';
            if (hasDisplayConfig && normalizeCodec(streamState.codec) === targetCodec) {
              syncCodecMenuUi();
              return;
            }
            var prevRawCodecChoice = streamState.codec;
            var prevPinnedCodec = userPinnedCodec;
            userPinnedCodec = targetCodec;
            streamState.codec = targetCodec;
            syncCodecMenuUi();
            renderAllResolutionLists();
            if (hudRows.length >= 8 && streamState.width > 0 && streamState.height > 0) {
              hudRows[7].textContent = formatHudResolution(
                streamState.width + 'x' + streamState.height
              );
            }
            if (IS_TAURI) {
              pendingUserCodecTarget = targetCodec;
              Bridge.sessionCommand({
                type: 'set_codec',
                codec: targetCodec
              }).catch(function (err) {
                pendingUserCodecTarget = null;
                streamState.codec = prevRawCodecChoice;
                userPinnedCodec = prevPinnedCodec;
                syncCodecMenuUi();
                renderAllResolutionLists();
                if (hudRows.length >= 8 && streamState.width > 0 && streamState.height > 0) {
                  hudRows[7].textContent = formatHudResolution(
                    streamState.width + 'x' + streamState.height
                  );
                }
                toast('切换编码器失败：' + String(err || '未知错误'));
              });
            }
            var advAfterCodec = evaluateHighRefreshAdvisoryLocal(
              streamState.width,
              streamState.height,
              streamState.fps,
              targetCodec
            );
            var codecLabel = targetCodec === 'hevc' ? 'HEVC (H.265)' : 'H.264';
            if (targetCodec === 'h264' && advAfterCodec.level !== 'optimal') {
              toast(
                advAfterCodec.can_enable_hevc
                  ? '已切换至 H.264；当前高分高刷可能受硬编限制，满帧建议开启 HEVC 或切至 ' +
                      formatRecommendedTarget(advAfterCodec)
                  : '已切换至 H.264；当前高分高刷可能受硬编限制，满帧建议切至 ' +
                      formatRecommendedTarget(advAfterCodec)
              );
            } else {
              toast('已切换视频编码器至 ' + codecLabel);
            }
            return;
          }
          if (group === 'fps') {
            var fpsNum = Number(val);
            var prevFpsVal = streamState.fps;
            var prevFpsRawCodec = streamState.codec;
            var prevCodec = effectiveCodec();
            if (isFinite(fpsNum) && fpsNum > 0) {
              streamState.fps = fpsNum;
            }
            var fpsFeedback = buildHighRefreshSwitchToast(
              streamState.width,
              streamState.height,
              fpsNum,
              prevCodec,
              '已切换目标帧率至 ' + val + ' fps'
            );
            if (fpsFeedback.autoHevc) {
              streamState.codec = 'hevc';
              syncCodecMenuUi();
              if (hudRows.length >= 8 && streamState.width > 0 && streamState.height > 0) {
                hudRows[7].textContent = formatHudResolution(
                  streamState.width + 'x' + streamState.height
                );
              }
            }
            renderAllResolutionLists();
            if (IS_TAURI) {
              if (typeof Bridge.saveStreamPrefs === 'function' && isFinite(fpsNum) && fpsNum > 0) {
                Bridge.saveStreamPrefs({ fps: fpsNum }).catch(function () {});
              }
              Bridge.sessionCommand({
                type: 'set_stream_params',
                width: Number(streamState.width || 0),
                height: Number(streamState.height || 0),
                fps: Number(val)
              }).catch(function (err) {
                streamState.fps = prevFpsVal;
                streamState.codec = prevFpsRawCodec;
                syncFpsMenuUi(prevFpsVal);
                syncCodecMenuUi();
                renderAllResolutionLists();
                if (hudRows.length >= 8 && streamState.width > 0 && streamState.height > 0) {
                  hudRows[7].textContent = formatHudResolution(
                    streamState.width + 'x' + streamState.height
                  );
                }
                toast('切换帧率失败：' + String(err || '未知错误'));
              });
            }
            toast(fpsFeedback.text || '已切换目标帧率至 ' + val + ' fps');
            return;
          }
          if (IS_TAURI && group === 'bitrate' && BITRATE_BPS[val]) {
            if (typeof Bridge.saveStreamPrefs === 'function') {
              Bridge.saveStreamPrefs({
                bitratePreset: val,
                bitrateBps: BITRATE_BPS[val]
              }).catch(function () {});
            }
            Bridge.sessionCommand({ type: 'set_bitrate', bps: BITRATE_BPS[val] }).catch(
              function () {}
            );
            toast('已切换码率');
            return;
          }
          /* 浏览器预览码率切换：仅改勾选态 + Toast（§6 S3 / T21 U3） */
          toast('画质将在下次连接生效');
        });
      });

      /* 安全中转 Switch 为独立开关（默认关），不属单选组 */
      if (relayEl) {
        relayEl.addEventListener('change', function () {
          if (!IS_TAURI) {
            toast(relayEl.checked ? '已开启安全中转连接（预览）' : '已关闭安全中转连接（预览）');
            relayEl.blur();
            return;
          }
          var desired = Boolean(relayEl.checked);
          if (typeof Bridge.setSessionRelayMode !== 'function') {
            relayEl.checked = !desired;
            toast('当前版本不支持切换安全中转');
            relayEl.blur();
            return;
          }
          relaySwitching = true;
          relayEl.disabled = true;
          toast(desired ? '正在切换至安全中转连接…' : '正在切换至直连模式…');
          function finishRelaySwitch() {
            relaySwitching = false;
            relayEl.disabled = false;
            relayEl.blur();
          }
          Bridge.setSessionRelayMode(desired)
            .then(function (label) {
              var modeLabel = label || (desired ? 'TCP Relay' : '直连');
              if (hudRows.length >= 2 && label) {
                hudRows[1].textContent = String(label);
              }
              toast('已切换传输模式：' + modeLabel);
              finishRelaySwitch();
            })
            .catch(function (err) {
              relayEl.checked = !desired;
              toast(String(err || '切换安全中转失败'));
              finishRelaySwitch();
            });
        });
      }
    }

    /* ── 全屏（§6 S3：使用 Tauri 原生 NSWindow 全屏，保留 sibling CAMetalLayer） ── */
    var nativeFullscreen = false;

    function syncFullscreenLabels() {
      var isFs = Boolean(nativeFullscreen || win.classList.contains('is-fullscreen'));
      var text = isFs ? '退出全屏幕' : '进入全屏幕';
      var labels = document.querySelectorAll('.sn-fullscreen-label');
      Array.prototype.forEach.call(labels, function (el) {
        el.textContent = text;
      });
    }

    function applyNativeFullscreenState(isFs) {
      nativeFullscreen = Boolean(isFs);
      win.classList.toggle('is-fullscreen', nativeFullscreen);
      syncFullscreenLabels();
      if (typeof window.__rcScheduleStageRectSync === 'function') {
        window.__rcScheduleStageRectSync();
      }
    }

    if (IS_TAURI) {
      if (typeof Bridge.isSessionFullscreen === 'function') {
        Bridge.isSessionFullscreen()
          .then(applyNativeFullscreenState)
          .catch(function () {});
      }
      if (typeof Bridge.onSessionFullscreenChanged === 'function') {
        Bridge.onSessionFullscreenChanged(function (payload) {
          applyNativeFullscreenState(Boolean(payload && payload.fullscreen));
        });
      }
    }

    function enterFallback() {
      win.classList.add('is-fullscreen');
      syncFullscreenLabels();
    }

    function exitFallback() {
      win.classList.remove('is-fullscreen');
      syncFullscreenLabels();
    }

    function toggleFullscreen() {
      if (IS_TAURI && typeof Bridge.toggleSessionFullscreen === 'function') {
        Bridge.toggleSessionFullscreen()
          .then(applyNativeFullscreenState)
          .catch(function (err) {
            toast('切换全屏失败：' + String(err || '未知错误'));
          });
        return;
      }
      if (win.classList.contains('is-fullscreen')) {
        exitFallback();
      } else {
        enterFallback();
      }
    }

    /* ── 退出远控（§6 S3：断开并销毁 S1 窗口回到主窗口 P1）────────── */
    var sessionSwitchTimer = 0;
    var sessionSwitchingPending = false;
    var sessionSwitchAttempts = 0;
    var SESSION_SWITCH_DELAYS = [1500, 2000, 2500, 3000, 3000, 3000];
    function cancelSessionSwitchTimer() {
      if (sessionSwitchTimer) {
        window.clearTimeout(sessionSwitchTimer);
        sessionSwitchTimer = 0;
      }
    }
    function closeSessionWindow(lockRemote) {
      sessionSwitchingPending = false;
      sessionSwitchAttempts = 0;
      cancelSessionSwitchTimer();
      var inTauri = !!(window.__TAURI_INTERNALS__ || window.__TAURI__);
      if (IS_TAURI && typeof Bridge.closeSessionWindow === 'function') {
        Bridge.closeSessionWindow(lockRemote).catch(function () {
          try {
            window.close();
          } catch (err) {
            /* 忽略 */
          }
        });
        return;
      }
      try {
        window.close();
      } catch (err) {
        /* 忽略：落到下方的回主窗口兜底 */
      }
      if (inTauri) return;

      /* 浏览器预览兜底：窗口没被关闭时跳到主窗口页 */
      window.setTimeout(function () {
        if (!window.closed) window.location.assign('index.html');
      }, 80);
    }

    function exitSession() {
      closePopover();
      closeSessionWindow(Boolean(secLock && secLock.checked));
    }

    /* ── 快捷键（§7.3）────────────────────────────────────────── */
    var swallowEscapeKeyup = false;
    var swallowNextScreenKeyup = false;
    document.addEventListener('keydown', function (e) {
      var target = e.target;
      var typing =
        target &&
        (target.tagName === 'INPUT' ||
          target.tagName === 'SELECT' ||
          target.tagName === 'TEXTAREA' ||
          target.isContentEditable);

      if (e.key === 'Escape') {
        if (privacyPreviewOverlay && !privacyPreviewOverlay.hidden) {
          e.preventDefault();
          swallowEscapeKeyup = true;
          closePrivacyPreview();
          return;
        }
        if (privacyModalBackdrop && !privacyModalBackdrop.hidden) {
          e.preventDefault();
          swallowEscapeKeyup = true;
          closePrivacyModal();
          return;
        }
        if (shortcutsModalBackdrop && !shortcutsModalBackdrop.hidden) {
          e.preventDefault();
          swallowEscapeKeyup = true;
          closeShortcutsModal();
          return;
        }
        if (remoteUpdateModalBackdrop && !remoteUpdateModalBackdrop.hidden) {
          e.preventDefault();
          swallowEscapeKeyup = true;
          closeRemoteUpdateModal();
          return;
        }
        /* Esc 仅在有打开的菜单/气泡时逐级关闭并吞掉按压与抬起事件；否则透传给被控端 */
        if (activeNestedEl && !activeNestedEl.hidden) {
          e.preventDefault();
          swallowEscapeKeyup = true;
          closeNestedMenu();
          return;
        }
        if (activeSubmenuEl && !activeSubmenuEl.hidden) {
          e.preventDefault();
          swallowEscapeKeyup = true;
          closeSubmenu();
          return;
        }
        if (open) {
          e.preventDefault();
          swallowEscapeKeyup = true;
          closePopover();
          return;
        }
      }

      if (typing || isAnySessionModalOpen()) return;

      /* ⌥⌘N：切换下一个屏幕（macOS 上 Option+N 会产出 Dead/˜，必须按物理 e.code 匹配并吞掉对应 keyup） */
      if (
        e.altKey &&
        e.metaKey &&
        !e.ctrlKey &&
        (e.code === 'KeyN' || e.key === 'n' || e.key === 'N')
      ) {
        e.preventDefault();
        swallowNextScreenKeyup = true;
        switchToNextDisplay();
        return;
      }

      /* Tauri 真实控制会话：转发键盘按下（含单键 f/F 与未消费的 Esc） */
      if (Bridge && Bridge.isTauri && realSessionActive && !watchMode && !open) {
        e.preventDefault();
        Bridge.sendInputEvent({
          kind: 'key',
          code: e.code || '',
          pressed: true,
          ctrl: e.ctrlKey,
          shift: e.shiftKey,
          alt: e.altKey,
          meta: e.metaKey
        }).catch(function () {});
      }
    });

    if (Bridge && Bridge.isTauri) {
      document.addEventListener('keyup', function (e) {
        if (e.key === 'Escape' && swallowEscapeKeyup) {
          swallowEscapeKeyup = false;
          e.preventDefault();
          return;
        }
        if (swallowNextScreenKeyup && (e.code === 'KeyN' || e.key === 'n' || e.key === 'N')) {
          swallowNextScreenKeyup = false;
          e.preventDefault();
          return;
        }
        if (!realSessionActive || watchMode || open || isAnySessionModalOpen()) return;
        var target = e.target;
        var typing =
          target &&
          (target.tagName === 'INPUT' ||
            target.tagName === 'SELECT' ||
            target.tagName === 'TEXTAREA' ||
            target.isContentEditable);
        if (typing) return;
        e.preventDefault();
        Bridge.sendInputEvent({
          kind: 'key',
          code: e.code || '',
          pressed: false,
          ctrl: e.ctrlKey,
          shift: e.shiftKey,
          alt: e.altKey,
          meta: e.metaKey
        }).catch(function () {});
      });

      /* Stage ResizeObserver → Rust stage_rect（CSS 像素，由 Rust 乘以 scale_factor） */
      function reportStageRect() {
        var r = stage.getBoundingClientRect();
        if (!(r.width > 0 && r.height > 0)) return;
        Bridge.stageRect({
          x: r.left,
          y: r.top,
          width: r.width,
          height: r.height
        }).catch(function () {});
      }
      window.__rcScheduleStageRectSync = function () {
        if (typeof window.requestAnimationFrame === 'function') {
          window.requestAnimationFrame(reportStageRect);
        } else {
          reportStageRect();
        }
        window.setTimeout(reportStageRect, 120);
        window.setTimeout(reportStageRect, 350);
      };

      /* 订阅 Rust 侧 session://update 事件流（T21 U5 / §6 / U-M7 / U-M8） */
      var CONNECT_PHASE_TEXT = {
        endpoint_creating: '正在初始化连接…',
        connecting: '正在连接目标设备…',
        awaiting_welcome: '正在验证身份与协商参数…',
        first_frame_pending: '已连接，正在等待远端画面…'
      };
      var endedCloseTimer = 0;
      var sessionLifecycleState = 'idle'; /* idle | connecting | active | ended */
      var hasHardwareVideo = false;
      function setHasVideo(active) {
        var on = Boolean(active);
        document.body.classList.toggle('has-video', on);
        stage.classList.toggle('has-video', on);
      }
      function cancelEndedCloseTimer() {
        if (endedCloseTimer) {
          window.clearTimeout(endedCloseTimer);
          endedCloseTimer = 0;
        }
      }
      function scheduleSessionSwitchRetry() {
        if (sessionSwitchAttempts >= SESSION_SWITCH_DELAYS.length) {
          sessionSwitchingPending = false;
          if (stageHintEl) {
            stageHintEl.hidden = false;
            stageHintEl.textContent = '自动重连超时，远端桌面切换未完成';
          }
          cancelEndedCloseTimer();
          endedCloseTimer = window.setTimeout(function () {
            endedCloseTimer = 0;
            closeSessionWindow();
          }, 2500);
          return;
        }
        var delay = SESSION_SWITCH_DELAYS[sessionSwitchAttempts];
        sessionSwitchAttempts += 1;
        var attemptLabel = sessionSwitchAttempts + '/' + SESSION_SWITCH_DELAYS.length;
        if (stageHintEl) {
          stageHintEl.hidden = false;
          stageHintEl.textContent = '远端正在切换系统桌面，自动重连中… (' + attemptLabel + ')';
        }
        cancelSessionSwitchTimer();
        sessionSwitchTimer = window.setTimeout(function () {
          sessionSwitchTimer = 0;
          if (!sessionSwitchingPending) return;
          if (typeof Bridge.reconnectLastSession !== 'function') {
            scheduleSessionSwitchRetry();
            return;
          }
          Bridge.reconnectLastSession().catch(function () {
            if (sessionSwitchingPending) {
              scheduleSessionSwitchRetry();
            }
          });
        }, delay);
      }
      function isUserDisconnectReason(reason) {
        return reason === '用户主动断开' || reason === '已主动断开远控会话';
      }
      function isNormalEndReason(reason) {
        return isUserDisconnectReason(reason) || reason === '会话已结束';
      }
      function resetSessionControlsForNewSession() {
        remoteAudioMuted = false;
        displaysRequested = false;
        initialSessionPrefsApplied = false;
        displaysState = {
          displays: [],
          activeDisplayId: 0
        };
        renderDisplayTabs();
        syncSecurityState();
        clipboardEnabled = true;
        syncClipboardMenuUi();
        seenNegotiated = false;
        hasActiveSessionSecurity = false;
        linkSecurityState.peerFingerprint = '';
        syncLinkSecurityUi();
        autoAppliedSavedRes = false;
        hasDisplayConfig = false;
        pendingUserResTarget = null;
        pendingUserCodecTarget = null;
        prevDisplayWidth = 0;
        prevDisplayHeight = 0;
        if (!relaySwitching && relayEl) {
          relayEl.checked = false;
          relayEl.disabled = false;
        }
        relaySwitching = false;
        streamState.width = 0;
        streamState.height = 0;
        streamState.nativeWidth = 0;
        streamState.nativeHeight = 0;
        streamState.codec = '';
        streamState.supportedCodecs = [];
        streamState.clientSupportsHevc = false;
        streamState.advisory = null;
        streamState.hasServerPresets = false;
        streamState.presets = DEFAULT_MOCK_PRESETS.slice();
        syncCodecMenuUi();
        renderAllResolutionLists();
        remoteUpdateState.hostVersion = null;
        if (!sessionSwitchingPending) {
          cancelPostRestartVerifyTimer();
          remoteUpdateState.inProgress = false;
          remoteUpdateState.stage = 'idle';
          remoteUpdateState.targetVersion = '';
          remoteUpdateState.lastError = '';
          if (remoteUpdateModalBackdrop) remoteUpdateModalBackdrop.hidden = true;
        }
        syncRemoteUpdateUi();
      }

      function handleSessionUpdate(upd) {
        if (!upd || !upd.kind) return;
        var data = upd.data || {};
        if (upd.kind === 'connecting') {
          if (sessionLifecycleState !== 'connecting') {
            resetSessionControlsForNewSession();
          }
          sessionLifecycleState = 'connecting';
          cancelSessionSwitchTimer();
          seenNegotiated = false;
          hasHardwareVideo = false;
          setHasVideo(false);
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (typeof data.watch_only === 'boolean') {
            setWatchMode(data.watch_only);
          }
          if (stageHintEl) {
            stageHintEl.hidden = false;
            stageHintEl.textContent =
              (data.phase && CONNECT_PHASE_TEXT[data.phase]) || '正在连接远端画面…';
          }
          reportStageRect();
          return;
        }
        if (upd.kind === 'negotiated') {
          if (sessionLifecycleState === 'ended') {
            resetSessionControlsForNewSession();
          }
          sessionLifecycleState = 'active';
          sessionSwitchingPending = false;
          sessionSwitchAttempts = 0;
          cancelSessionSwitchTimer();
          if (
            remoteUpdateState.inProgress &&
            remoteUpdateState.stage === 'restarting' &&
            !remoteUpdateState.hostVersion
          ) {
            cancelPostRestartVerifyTimer();
            postRestartVerifyTimer = window.setTimeout(function () {
              postRestartVerifyTimer = 0;
              if (
                remoteUpdateState.inProgress &&
                remoteUpdateState.stage === 'restarting' &&
                !remoteUpdateState.hostVersion
              ) {
                remoteUpdateState.inProgress = false;
                remoteUpdateState.stage = 'idle';
                remoteUpdateState.targetVersion = '';
                remoteUpdateState.lastError = '';
                if (remoteUpdateModalBackdrop) remoteUpdateModalBackdrop.hidden = true;
                syncRemoteUpdateUi();
              }
            }, 8000);
          }
          syncRemoteUpdateUi();
          seenNegotiated = true;
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (typeof data.watch_only === 'boolean') {
            setWatchMode(data.watch_only);
          }
          var backendStr = typeof data.backend === 'string' ? data.backend : '';
          hasHardwareVideo = backendStr.indexOf('metal') !== -1;
          setHasVideo(hasHardwareVideo);
          if (stageHintEl) {
            if (!hasHardwareVideo && backendStr.indexOf('降级') !== -1) {
              stageHintEl.hidden = false;
              stageHintEl.textContent = '硬件视频渲染不可用（当前为降级模式）';
            } else {
              stageHintEl.hidden = true;
            }
          }
          var negW = Number(data.width);
          var negH = Number(data.height);
          var negFps = Number(data.fps);
          if (isFinite(negW) && negW > 0) streamState.width = negW;
          if (isFinite(negH) && negH > 0) streamState.height = negH;
          if (isFinite(negFps) && negFps > 0) {
            streamState.fps = negFps;
            syncFpsMenuUi(negFps);
          }
          hasActiveSessionSecurity = true;
          if (typeof data.mtls_active === 'boolean') {
            linkSecurityState.encryptConnection = data.mtls_active;
          }
          linkSecurityState.peerFingerprint = data.peer_cert_fingerprint
            ? String(data.peer_cert_fingerprint)
            : '';
          syncRelaySwitchUi(data.transport);
          if (!streamState.hasServerPresets && streamState.width > 0 && streamState.height > 0) {
            streamState.nativeWidth = streamState.nativeWidth || streamState.width;
            streamState.nativeHeight = streamState.nativeHeight || streamState.height;
            streamState.presets = buildFallbackPresets(streamState.width, streamState.height);
          }
          renderAllResolutionLists();
          if (hudRows.length >= 8 && data.width && data.height) {
            hudRows[7].textContent = formatHudResolution(data.width + 'x' + data.height);
          }
          applySavedSessionPrefsIfReady();
          requestSessionDisplaysOnce();
          reportStageRect();
          return;
        }
        if (upd.kind === 'privacy_state') {
          if (typeof data.mode === 'string') privacyState.mode = data.mode;
          if (typeof data.preset === 'string' && data.preset) {
            privacyState.preset = normalizePrivacyPreset(data.preset);
          }
          syncPrivacyAndHostAudioUi();
          if (data.supported === false && privacyState.mode === 'off') {
            toast('当前被控端暂不支持物理防窥幕布');
          }
          return;
        }
        if (upd.kind === 'host_mute_state') {
          hostAudioState.muted = Boolean(data.muted);
          syncPrivacyAndHostAudioUi();
          if (data.supported === false) {
            toast('当前被控端音频设备不支持系统静音控制');
          }
          return;
        }
        if (upd.kind === 'mouse_mode_state') {
          if (typeof data.mode === 'string' && data.mode) {
            peripheralsState.mouseMode = data.mode;
            syncPeripheralsUi();
          }
          return;
        }
        if (upd.kind === 'displays_state') {
          if (Array.isArray(data.displays)) {
            displaysState.displays = data.displays.slice();
            displaysState.activeDisplayId = Number(data.active_display_id || 0);
            renderDisplayTabs();
          }
          return;
        }
        if (upd.kind === 'virtual_display_error') {
          toast('虚拟屏操作失败：' + String(data.message || '系统不支持或权限不足'));
          return;
        }
        if (upd.kind === 'host_version') {
          cancelPostRestartVerifyTimer();
          remoteUpdateState.hostVersion = data;
          if (remoteUpdateState.inProgress && remoteUpdateState.stage === 'restarting') {
            if (!data || data.isOwnerSession !== true) {
              remoteUpdateState.inProgress = false;
              remoteUpdateState.stage = 'idle';
              remoteUpdateState.targetVersion = '';
              remoteUpdateState.lastError = '';
              if (remoteUpdateModalBackdrop) remoteUpdateModalBackdrop.hidden = true;
              syncRemoteUpdateUi();
              return;
            }
            if (data.version) {
              var expectedTarget =
                remoteUpdateState.targetVersion ||
                (remoteUpdateState.latestRelease && remoteUpdateState.latestRelease.version) ||
                '';
              if (expectedTarget && !isNewerSemver(expectedTarget, data.version)) {
                remoteUpdateState.inProgress = false;
                remoteUpdateState.stage = 'succeeded';
                remoteUpdateState.lastError = '';
                closeRemoteUpdateModal();
                toast('被控端已成功升级至 v' + data.version);
              } else if (expectedTarget) {
                var staleMsg =
                  '被控端重连后仍为旧版本 v' +
                  data.version +
                  '（目标 v' +
                  expectedTarget +
                  '），升级未生效，可重试';
                applyRemoteUpdateStatus({
                  stage: 'failed',
                  progressPct: 0,
                  message: staleMsg,
                  error: staleMsg
                });
                return;
              } else {
                remoteUpdateState.inProgress = false;
                remoteUpdateState.stage = 'idle';
                remoteUpdateState.lastError = '';
                closeRemoteUpdateModal();
              }
            }
          }
          syncRemoteUpdateUi();
          return;
        }
        if (upd.kind === 'remote_update_status') {
          if (remoteUpdateState.hostVersion && remoteUpdateState.hostVersion.isOwnerSession === true) {
            applyRemoteUpdateStatus(data);
          }
          return;
        }
        if (upd.kind === 'display_config') {
          sessionLifecycleState = 'active';
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (!seenNegotiated) {
            setHasVideo(true);
            reportStageRect();
          }
          if (stageHintEl && (hasHardwareVideo || !seenNegotiated)) {
            stageHintEl.hidden = true;
          }
          var cfgW = Number(data.width);
          var cfgH = Number(data.height);
          var cfgFps = Number(data.fps);
          var natW = Number(data.native_width);
          var natH = Number(data.native_height);
          var maxFps = Number(data.max_fps);
          var hasValidCfgDims = isFinite(cfgW) && cfgW > 0 && isFinite(cfgH) && cfgH > 0;
          var dimsChanged =
            hasValidCfgDims &&
            (cfgW !== prevDisplayWidth || cfgH !== prevDisplayHeight);
          if (isFinite(cfgW) && cfgW > 0) {
            streamState.width = cfgW;
            prevDisplayWidth = cfgW;
          }
          if (isFinite(cfgH) && cfgH > 0) {
            streamState.height = cfgH;
            prevDisplayHeight = cfgH;
          }
          if (isFinite(cfgFps) && cfgFps > 0) {
            streamState.fps = cfgFps;
            syncFpsMenuUi(cfgFps);
          }
          if (isFinite(natW) && natW > 0) streamState.nativeWidth = natW;
          if (isFinite(natH) && natH > 0) streamState.nativeHeight = natH;
          if (isFinite(maxFps) && maxFps > 0) streamState.maxFps = maxFps;
          var cfgCodec = normalizeCodec(data.codec);
          streamState.codec = cfgCodec;
          streamState.supportedCodecs = Array.isArray(data.supported_codecs)
            ? data.supported_codecs.slice()
            : [];
          if (typeof data.client_supports_hevc === 'boolean') {
            streamState.clientSupportsHevc = data.client_supports_hevc;
          }
          if (data.advisory && typeof data.advisory === 'object') {
            streamState.advisory = data.advisory;
          }
          if (Array.isArray(data.presets) && data.presets.length > 0) {
            streamState.presets = data.presets;
            streamState.hasServerPresets = true;
          } else if (!streamState.hasServerPresets) {
            var fbW = streamState.nativeWidth || streamState.width;
            var fbH = streamState.nativeHeight || streamState.height;
            if (fbW > 0 && fbH > 0) {
              streamState.presets = buildFallbackPresets(fbW, fbH);
            }
          }
          if (hudRows.length >= 8 && streamState.width > 0 && streamState.height > 0) {
            hudRows[7].textContent = formatHudResolution(
              streamState.width + 'x' + streamState.height
            );
          }
          hasDisplayConfig = true;
          syncCodecMenuUi();
          renderAllResolutionLists();
          maybeAutoApplySavedResolution();
          if (pendingUserCodecTarget) {
            var wantedCodec = pendingUserCodecTarget;
            var authoritativeCodec = cfgCodec || 'h264';
            pendingUserCodecTarget = null;
            if (authoritativeCodec !== wantedCodec) {
              toast(
                '被控端保持 ' +
                  (authoritativeCodec === 'hevc' ? 'HEVC (H.265)' : 'H.264') +
                  ' 编码'
              );
            }
          }
          if (pendingUserResTarget && hasValidCfgDims) {
            var matchedTarget =
              Math.abs(cfgW - pendingUserResTarget.width) <= 2 &&
              Math.abs(cfgH - pendingUserResTarget.height) <= 2;
            var suppressConfirm = Boolean(pendingUserResTarget.suppressConfirmToast);
            if (matchedTarget || dimsChanged) {
              pendingUserResTarget = null;
              var confirmedAdv = evaluateHighRefreshAdvisoryLocal(
                cfgW,
                cfgH,
                streamState.fps,
                effectiveCodec()
              );
              if (confirmedAdv.level === 'optimal' && !(matchedTarget && suppressConfirm)) {
                toast('当前分辨率：' + cfgW + '×' + cfgH);
              }
            }
          }
          sessionLifecycleState = 'active';
          realSessionActive = true;
          applySavedSessionPrefsIfReady();
          requestSessionDisplaysOnce();
          return;
        }
        if (upd.kind === 'hud') {
          sessionLifecycleState = 'active';
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (!seenNegotiated) {
            /* 窗口迟到打开、仅经 sessionSnapshot 拉到 Hud 时，仍需透出底层 Metal 画面并补报矩形 */
            setHasVideo(true);
            reportStageRect();
          } else if (hasHardwareVideo) {
            setHasVideo(true);
          }
          if (stageHintEl && (hasHardwareVideo || !seenNegotiated)) {
            stageHintEl.hidden = true;
          }
          if (
            streamState.width === 0 &&
            data.resolution &&
            typeof data.resolution === 'string'
          ) {
            var resMatch = data.resolution.match(/^(\d+)\s*[x×]\s*(\d+)$/i);
            if (resMatch) {
              streamState.width = Number(resMatch[1]);
              streamState.height = Number(resMatch[2]);
              if (!streamState.hasServerPresets && streamState.width > 0 && streamState.height > 0) {
                streamState.nativeWidth = streamState.nativeWidth || streamState.width;
                streamState.nativeHeight = streamState.nativeHeight || streamState.height;
                streamState.presets = buildFallbackPresets(streamState.width, streamState.height);
              }
              renderAllResolutionLists();
            }
          }
          syncRelaySwitchUi(data.transport);
          applySavedSessionPrefsIfReady();
          requestSessionDisplaysOnce();
          applyHudSnapshot(data);
          return;
        }
        if (upd.kind === 'session_switching') {
          sessionSwitchingPending = true;
          cancelEndedCloseTimer();
          if (stageHintEl) {
            stageHintEl.hidden = false;
            stageHintEl.textContent = data.reason || '远端正在切换系统桌面，自动重连中…';
          }
          return;
        }
        if (upd.kind === 'ended') {
          sessionLifecycleState = 'ended';
          seenNegotiated = false;
          hasHardwareVideo = false;
          setHasVideo(false);
          realSessionActive = false;
          if (relayEl) {
            relayEl.disabled = true;
          }
          var reason = data.reason || '会话已结束';
          if (isUserDisconnectReason(reason)) {
            sessionSwitchingPending = false;
            sessionSwitchAttempts = 0;
            cancelSessionSwitchTimer();
          } else if (
            (sessionSwitchingPending ||
              (remoteUpdateState.inProgress && remoteUpdateState.stage === 'restarting') ||
              (typeof reason === 'string' && reason.indexOf('正在切换系统桌面') !== -1)) &&
            typeof Bridge.reconnectLastSession === 'function'
          ) {
            sessionSwitchingPending = true;
            toast(
              remoteUpdateState.inProgress && remoteUpdateState.stage === 'restarting'
                ? '被控端正在重启升级，自动重连中…'
                : reason
            );
            scheduleSessionSwitchRetry();
            return;
          }
          toast(reason);
          if (isNormalEndReason(reason)) {
            closeSessionWindow();
          } else {
            /* U-M8：异常结束时在 stage 展示原因并延迟关闭，避免窗口秒闪导致用户看不到失败原因 */
            if (stageHintEl) {
              stageHintEl.hidden = false;
              stageHintEl.textContent = reason;
            }
            cancelEndedCloseTimer();
            endedCloseTimer = window.setTimeout(function () {
              endedCloseTimer = 0;
              if (sessionLifecycleState === 'ended') {
                closeSessionWindow();
              }
            }, 2500);
          }
        }
      }

      Bridge.onSessionUpdate(handleSessionUpdate);

      /* Review M8：首帧主动拉取最近一次会话快照，消除窗口加载与事件推送之间的竞态窗口 */
      if (typeof Bridge.sessionSnapshot === 'function') {
        Bridge.sessionSnapshot()
          .then(function (snap) {
            if (snap) handleSessionUpdate(snap);
          })
          .catch(function () {});
      }
      if (typeof Bridge.getSessionHostVersion === 'function') {
        Bridge.getSessionHostVersion()
          .then(function (hv) {
            if (hv) {
              remoteUpdateState.hostVersion = hv;
              syncRemoteUpdateUi();
            }
          })
          .catch(function () {});
      }
      if (typeof Bridge.getCachedRelease === 'function') {
        Bridge.getCachedRelease()
          .then(function (rel) {
            if (rel && rel.version) {
              remoteUpdateState.latestRelease = rel;
              syncRemoteUpdateUi();
              return;
            }
            if (typeof Bridge.checkForUpdates === 'function') {
              return Bridge.checkForUpdates().then(function (res) {
                if (res && res.release && res.release.version) {
                  remoteUpdateState.latestRelease = res.release;
                  syncRemoteUpdateUi();
                }
              });
            }
          })
          .catch(function () {});
      }

      reportStageRect();
      if (typeof window.ResizeObserver === 'function') {
        var ro = new window.ResizeObserver(reportStageRect);
        ro.observe(stage);
      }

      /* Stage 指针与滚轮输入转发（按实时 watchMode 动态门控，观看模式零注入，T21 U9 / U-M7） */
      var hudWrap = document.getElementById('hud-wrap');
      function isStageInputTarget(target) {
        if (!target) return false;
        if (hudWrap && hudWrap.contains(target)) return false;
        if (open || isAnySessionModalOpen()) return false;
        return true;
      }

      function sendStagePointerMove(e) {
        var r = stage.getBoundingClientRect();
        if (!(r.width > 0 && r.height > 0)) return;
        Bridge.sendInputEvent({
          kind: 'mouse_move',
          x: e.clientX - r.left,
          y: e.clientY - r.top,
          stage_w: r.width,
          stage_h: r.height
        }).catch(function () {});
      }

      stage.addEventListener('pointermove', function (e) {
        if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
        sendStagePointerMove(e);
      });

      stage.addEventListener('pointerdown', function (e) {
        if (e.button === 2) {
          e.preventDefault();
        }
        if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
        sendStagePointerMove(e);
        Bridge.sendInputEvent({
          kind: 'mouse_button',
          button: e.button,
          pressed: true
        }).catch(function () {});
      });

      stage.addEventListener('pointerup', function (e) {
        if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
        sendStagePointerMove(e);
        Bridge.sendInputEvent({
          kind: 'mouse_button',
          button: e.button,
          pressed: false
        }).catch(function () {});
      });

      stage.addEventListener(
        'wheel',
        function (e) {
          if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
          e.preventDefault();
          var lh = parseFloat(window.getComputedStyle(stage).lineHeight);
          if (!isFinite(lh) || lh <= 0) lh = 16;
          Bridge.sendInputEvent({
            kind: 'mouse_wheel',
            delta_y_px: e.deltaY,
            line_height_px: lh,
            delta_x_px: e.deltaX || 0
          }).catch(function () {});
        },
        { passive: false }
      );
    }

    /* 窗口尺寸变化：重新定位当前浮层（§3.6 定位跟随锚点） */
    window.addEventListener('resize', function () {
      if (open) place(open.el, open.anchor, open.align);
      if (activeSubmenuEl && !activeSubmenuEl.hidden && submenuAnchorItem) {
        placeSubmenu(submenuAnchorItem, activeSubmenuEl);
      }
      if (activeNestedEl && !activeNestedEl.hidden && nestedAnchorItem) {
        var parentEl = nestedAnchorItem.closest('.sn-popover') || activeSubmenuEl || (open && open.el);
        placeFloatingMenu(nestedAnchorItem, activeNestedEl, parentEl);
      }
    });

    /* ── 独立预览（?preview=s2|s3|s3-quality|s4）─────────────────
     * 浮层默认收起；带参数时自动打开对应浮层，便于 file:// 逐屏验收。
     * 不属于产品交互。 */
    var preview = params.get('preview') || '';
    if (preview === 's2') {
      openPopover(securityEl, shieldBtn, 'center');
    } else if (preview === 's3' || preview === 's3-quality') {
      openPopover(menuEl, ctrlBtn, 'right');
      if (preview === 's3-quality' && qualityItem) openSubmenu(qualityItem, qualityEl);
    } else if (preview === 's4') {
      /* HUD 常驻，无需额外动作 */
    }
  }

  /* ── 启动 ─────────────────────────────────────────────────────── */
  function boot() {
    initTerminal();
    initSession();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }

  /* 时钟注入点（§8.6.2）：传一个返回毫秒的函数；换钟后计时从新钟的当前时刻起算 */
  window.RCSession = {
    setClock: function (fn) {
      if (typeof fn === 'function') clock.nowMs = fn;
    },
    formatHms: formatHms
  };
})();
