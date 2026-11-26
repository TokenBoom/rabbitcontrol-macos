/* bridge.js —— Tauri v2 IPC 轻量封装（纯静态前端，零 npm 依赖）
 *
 * 纪律：
 *   - 浏览器 / Playwright 静态预览（无 __TAURI_INTERNALS__）时完全静默、0 控制台错误；
 *   - Tauri 真窗口内封装 invoke 与 session://update 事件订阅，供 app.js / session.js / portmap.js 复用。
 */
(function (root) {
  "use strict";

  function getInvoke() {
    if (root.__TAURI__ && root.__TAURI__.core && typeof root.__TAURI__.core.invoke === "function") {
      return root.__TAURI__.core.invoke;
    }
    if (root.__TAURI_INTERNALS__ && typeof root.__TAURI_INTERNALS__.invoke === "function") {
      return root.__TAURI_INTERNALS__.invoke;
    }
    return null;
  }

  function getListen() {
    if (root.__TAURI__ && root.__TAURI__.event && typeof root.__TAURI__.event.listen === "function") {
      return root.__TAURI__.event.listen;
    }
    return null;
  }

  var isTauri = Boolean(getInvoke());

  function invoke(cmd, args) {
    var fn = getInvoke();
    if (!fn) {
      return Promise.reject(new Error("Tauri IPC 不可用"));
    }
    return fn(cmd, args || {});
  }

  function listen(eventName, handler) {
    var fn = getListen();
    if (!fn) {
      return Promise.resolve(function () {});
    }
    return fn(eventName, handler);
  }

  root.RCBridge = {
    isTauri: isTauri,
    invoke: invoke,
    listen: listen,

    openToolWindow: function (kind) {
      return invoke("open_tool_window", { kind: kind });
    },
    openSessionWindow: function (watchOnly) {
      return invoke("open_session_window", { watchOnly: Boolean(watchOnly) });
    },
    listDevices: function () {
      return invoke("list_devices");
    },
    parseConnectTarget: function (target) {
      return invoke("parse_connect_target", { target: target });
    },
    openSession: function (cfg) {
      return invoke("open_session", { cfg: cfg });
    },
    sessionCommand: function (cmd) {
      return invoke("session_command", { cmd: cmd });
    },
    sessionSnapshot: function () {
      return invoke("session_snapshot");
    },
    sendInputEvent: function (ev) {
      return invoke("send_input_event", { ev: ev });
    },
    stageRect: function (rect) {
      return invoke("stage_rect", { rect: rect });
    },
    fitSessionWindow: function () {
      return invoke("fit_session_window");
    },
    isSessionFullscreen: function () {
      return invoke("is_session_fullscreen");
    },
    setSessionFullscreen: function (fullscreen) {
      return invoke("set_session_fullscreen", { fullscreen: Boolean(fullscreen) });
    },
    toggleSessionFullscreen: function () {
      return invoke("toggle_session_fullscreen");
    },
    validateMapForm: function (form) {
      return invoke("validate_map_form", { form: form });
    },
    copyText: function (text) {
      return invoke("copy_text", { text: text });
    },
    getHostStatus: function () {
      return invoke("get_host_status");
    },
    checkPermissions: function () {
      return invoke("check_permissions");
    },
    requestScreenRecordingPermission: function () {
      return invoke("request_screen_recording_permission");
    },
    openPermissionSettings: function (kind) {
      return invoke("open_permission_settings", { kind: String(kind) });
    },
    setHostEnabled: function (enabled) {
      return invoke("set_host_enabled", { enabled: Boolean(enabled) });
    },
    setHostPassword: function (password, regenerate) {
      return invoke("set_host_password", {
        password: password == null ? null : String(password),
        regenerate: Boolean(regenerate)
      });
    },
    respondApproval: function (generation, accepted) {
      return invoke("respond_approval", {
        generation: generation,
        accepted: Boolean(accepted)
      });
    },
    disconnectHostClient: function () {
      return invoke("disconnect_host_client");
    },
    listLanDevices: function () {
      return invoke("list_lan_devices");
    },
    getServerConfig: function () {
      return invoke("get_server_config");
    },
    saveServerConfig: function (cfg) {
      return invoke("save_server_config", { cfg: cfg });
    },
    testServerConnection: function (serverHost, signalPort, relayPort) {
      return invoke("test_server_connection", {
        serverHost: serverHost == null ? "" : String(serverHost),
        signalPort: signalPort == null ? 21116 : Number(signalPort),
        relayPort: relayPort == null ? 21117 : Number(relayPort)
      });
    },
    fileListLocal: function (path) {
      return invoke("file_list_local", { path: path == null ? null : String(path) });
    },
    fileMkdirLocal: function (parent, name) {
      return invoke("file_mkdir_local", { parent: String(parent), name: String(name) });
    },
    fileDeleteLocal: function (path) {
      return invoke("file_delete_local", { path: String(path) });
    },
    fileRenameLocal: function (path, newName) {
      return invoke("file_rename_local", { path: String(path), newName: String(newName) });
    },
    fileListRemote: function (path) {
      return invoke("file_list_remote", { path: path == null ? null : String(path) });
    },
    fileMkdirRemote: function (parent, name) {
      return invoke("file_mkdir_remote", { parent: String(parent), name: String(name) });
    },
    fileDeleteRemote: function (path) {
      return invoke("file_delete_remote", { path: String(path) });
    },
    fileRenameRemote: function (path, newName) {
      return invoke("file_rename_remote", { path: String(path), newName: String(newName) });
    },
    fileUpload: function (localPath, remoteDir) {
      return invoke("file_upload", { localPath: String(localPath), remoteDir: String(remoteDir) });
    },
    fileDownload: function (remotePath, localDir) {
      return invoke("file_download", { remotePath: String(remotePath), localDir: String(localDir) });
    },
    fileCancel: function (id) {
      return invoke("file_cancel", { id: Number(id) });
    },
    fileListTransfers: function () {
      return invoke("file_list_transfers");
    },
    fileClearFinished: function () {
      return invoke("file_clear_finished");
    },
    fileRemoveTransfer: function (id) {
      return invoke("file_remove_transfer", { id: Number(id) });
    },
    terminalOpen: function (cols, rows, name, attach) {
      var args = { cols: Number(cols), rows: Number(rows) };
      if (typeof name === "string" && name.length > 0) {
        args.name = name;
      }
      if (typeof attach === "boolean") {
        args.attach = attach;
      }
      return invoke("terminal_open", args);
    },
    terminalList: function () {
      return invoke("terminal_list");
    },
    terminalAttach: function (termId, cols, rows) {
      return invoke("terminal_attach", {
        termId: Number(termId),
        cols: Number(cols),
        rows: Number(rows)
      });
    },
    terminalDetach: function (termId) {
      return invoke("terminal_detach", { termId: Number(termId) });
    },
    terminalRename: function (termId, name) {
      return invoke("terminal_rename", { termId: Number(termId), name: String(name) });
    },
    terminalInput: function (termId, data) {
      return invoke("terminal_input", { termId: Number(termId), data: String(data) });
    },
    terminalResize: function (termId, cols, rows) {
      return invoke("terminal_resize", { termId: Number(termId), cols: Number(cols), rows: Number(rows) });
    },
    terminalClose: function (termId) {
      return invoke("terminal_close", { termId: Number(termId) });
    },
    terminalListLocalFonts: function () {
      return invoke("terminal_list_local_fonts");
    },
    terminalListRemoteFonts: function () {
      return invoke("terminal_list_remote_fonts");
    },
    terminalFetchRemoteFont: function (fontId) {
      return invoke("terminal_fetch_remote_font", { fontId: String(fontId) });
    },
    terminalGetRemoteFontBytes: function (fontId) {
      return invoke("terminal_get_remote_font_bytes", { fontId: String(fontId) });
    },
    terminalSetFontPrefs: function (patch) {
      return invoke("terminal_set_font_prefs", { patch: patch || {} });
    },
    portmapList: function () {
      return invoke("portmap_list");
    },
    portmapAdd: function (name, localPort, targetAddr, targetPort) {
      return invoke("portmap_add", {
        name: String(name),
        localPort: Number(localPort),
        targetAddr: String(targetAddr),
        targetPort: Number(targetPort)
      });
    },
    portmapToggle: function (id, enabled) {
      return invoke("portmap_toggle", { id: Number(id), enabled: Boolean(enabled) });
    },
    portmapDelete: function (id) {
      return invoke("portmap_delete", { id: Number(id) });
    },
    focusSessionWindow: function () {
      return invoke("focus_session_window");
    },
    closeSessionWindow: function (lockRemote) {
      return invoke("close_session_window", {
        lockRemote: typeof lockRemote === "boolean" ? lockRemote : null
      });
    },
    getAppPrefs: function () {
      return invoke("get_app_prefs");
    },
    getAppInfo: function () {
      return invoke("get_app_info");
    },
    checkForUpdates: function () {
      return invoke("check_for_updates");
    },
    getCachedRelease: function () {
      return invoke("get_cached_release");
    },
    startSelfUpdate: function () {
      return invoke("start_self_update");
    },
    cancelSelfUpdate: function () {
      return invoke("cancel_self_update");
    },
    getUpdateMirrorUrl: function () {
      return invoke("get_update_mirror_url");
    },
    saveUpdateMirrorUrl: function (mirrorUrl) {
      return invoke("save_update_mirror_url", { mirrorUrl: String(mirrorUrl || "") });
    },
    getSessionHostVersion: function () {
      return invoke("get_session_host_version");
    },
    triggerRemoteUpdate: function (targetVersion) {
      return invoke("trigger_remote_update", {
        targetVersion: targetVersion ? String(targetVersion) : null
      });
    },
    cancelRemoteUpdate: function () {
      return invoke("cancel_remote_update");
    },
    onUpdaterProgress: function (cb) {
      return listen("updater://progress", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    saveStreamPrefs: function (patch) {
      return invoke("save_stream_prefs", { patch: patch || {} });
    },
    getDeviceSnapshots: function () {
      return invoke("get_device_snapshots");
    },
    getActiveSessionInfo: function () {
      return invoke("get_active_session_info");
    },
    refreshDeviceId: function () {
      return invoke("refresh_device_id");
    },
    setSessionRelayMode: function (forceRelay) {
      return invoke("set_session_relay_mode", { forceRelay: Boolean(forceRelay) });
    },
    setSessionWatchOnly: function (enabled) {
      return invoke("set_session_watch_only", { enabled: Boolean(enabled) });
    },
    setSessionMtlsMode: function (enabled) {
      return invoke("set_session_mtls_mode", { enabled: Boolean(enabled) });
    },
    getGateStatus: function () {
      return invoke("get_gate_status");
    },
    probeAndSaveServer: function (req) {
      return invoke("probe_and_save_server", { req: req || {} });
    },
    fetchAuthConfig: function () {
      return invoke("fetch_auth_config");
    },
    authRegister: function (req) {
      return invoke("auth_register", { req: req || {} });
    },
    authLogin: function (req) {
      return invoke("auth_login", { req: req || {} });
    },
    authLogout: function () {
      return invoke("auth_logout");
    },
    authChangePassword: function (req) {
      return invoke("auth_change_password", { req: req || {} });
    },
    listMyDevices: function () {
      return invoke("list_my_devices");
    },
    renameMyDevice: function (deviceId, alias) {
      return invoke("rename_my_device", {
        deviceId: String(deviceId),
        alias: String(alias)
      });
    },
    removeMyDevice: function (deviceId) {
      return invoke("remove_my_device", { deviceId: String(deviceId) });
    },
    getSecurityPrefs: function () {
      return invoke("get_security_prefs");
    },
    saveSecurityPrefs: function (patch) {
      return invoke("save_security_prefs", { patch: patch || {} });
    },
    renewDeviceCert: function () {
      return invoke("renew_device_cert");
    },
    getGeneralPrefs: function () {
      return invoke("get_general_prefs");
    },
    saveGeneralPrefs: function (patch) {
      return invoke("save_general_prefs", { patch: patch || {} });
    },
    getServiceStatus: function () {
      return invoke("get_service_status");
    },
    installSystemService: function () {
      return invoke("install_system_service");
    },
    uninstallSystemService: function () {
      return invoke("uninstall_system_service");
    },
    consumeTrayPermCheck: function () {
      return invoke("consume_tray_perm_check");
    },
    quitAppCompletely: function () {
      return invoke("quit_app_completely");
    },
    getClientLockStatus: function () {
      return invoke("get_client_lock_status");
    },
    getPrivacyCustomImage: function () {
      return invoke("get_privacy_custom_image");
    },
    savePrivacyCustomImage: function (dataUrl) {
      return invoke("save_privacy_custom_image", { dataUrl: String(dataUrl || "") });
    },
    setSessionPrivacyMode: function (mode, preset, customImageDataUrl, persist) {
      return invoke("set_session_privacy_mode", {
        mode: String(mode || "off"),
        preset: preset ? String(preset) : null,
        customImageDataUrl: customImageDataUrl ? String(customImageDataUrl) : null,
        persist: typeof persist === "boolean" ? persist : true
      });
    },
    setSessionHostMute: function (muted, restoreOnExit, persist) {
      return invoke("set_session_host_mute", {
        muted: Boolean(muted),
        restoreOnExit: typeof restoreOnExit === "boolean" ? restoreOnExit : null,
        persist: typeof persist === "boolean" ? persist : true
      });
    },
    setSessionPeripherals: function (opts) {
      var o = opts || {};
      return invoke("set_session_peripherals", {
        sendSystemShortcuts: typeof o.sendSystemShortcuts === "boolean" ? o.sendSystemShortcuts : null,
        shortcutExceptions: Array.isArray(o.shortcutExceptions) ? o.shortcutExceptions : null,
        mouseMode: typeof o.mouseMode === "string" ? o.mouseMode : null,
        persist: typeof o.persist === "boolean" ? o.persist : true
      });
    },
    listSessionDisplays: function () {
      return invoke("list_session_displays");
    },
    switchSessionDisplay: function (displayId) {
      return invoke("switch_session_display", { displayId: Number(displayId) });
    },
    createSessionVirtualDisplay: function (width, height, hidpi) {
      return invoke("create_session_virtual_display", {
        width: width ? Number(width) : null,
        height: height ? Number(height) : null,
        hidpi: typeof hidpi === "boolean" ? hidpi : true
      });
    },
    removeSessionVirtualDisplay: function (displayId) {
      return invoke("remove_session_virtual_display", { displayId: Number(displayId) });
    },
    lockClient: function (password) {
      return invoke("lock_client", { password: password == null ? null : String(password) });
    },
    unlockClient: function (password) {
      return invoke("unlock_client", { password: String(password || "") });
    },
    changeClientLockPassword: function (currentPassword, newPassword) {
      return invoke("change_client_lock_password", {
        currentPassword: currentPassword == null ? null : String(currentPassword),
        newPassword: String(newPassword || "")
      });
    },
    onClientLockChanged: function (cb) {
      return listen("client://lock-changed", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    reconnectLastSession: function () {
      return invoke("reconnect_last_session");
    },
    onTrayOpenPermissions: function (cb) {
      return listen("tray://open-permissions", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onServiceStatus: function (cb) {
      return listen("service://status", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onGateStatus: function (cb) {
      return listen("gate://status", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onSessionUpdate: function (cb) {
      return listen("session://update", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onHostUpdate: function (cb) {
      return listen("host://update", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onLanDevices: function (cb) {
      return listen("lan://devices", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onDeviceStatusChanged: function (cb) {
      return listen("device://status-changed", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onAuthKicked: function (cb) {
      return listen("auth://kicked", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onServerStatus: function (cb) {
      return listen("server://status", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onFileEvent: function (cb) {
      return listen("file://event", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onTerminalEvent: function (cb) {
      return listen("terminal://event", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onPortmapUpdate: function (cb) {
      return listen("portmap://update", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onActiveSessionState: function (cb) {
      return listen("session://active-state", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onSessionFullscreenChanged: function (cb) {
      return listen("session://fullscreen-changed", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onDeviceSnapshots: function (cb) {
      return listen("device://snapshots", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    },
    onDockConnectDevice: function (cb) {
      return listen("dock://connect-device", function (ev) {
        if (typeof cb === "function") {
          cb(ev && ev.payload !== undefined ? ev.payload : ev);
        }
      });
    }
  };
})(window);
