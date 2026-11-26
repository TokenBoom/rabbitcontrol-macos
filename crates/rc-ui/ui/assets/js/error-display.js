/* error-display.js —— 基于 rabbitcontrol-spec UiError 的 4 级统一错误渲染模块
 *
 * 纪律：
 *   - 零外部依赖、纯静态 JS（支持 Tauri WebView、浏览器预览与 Node 单测）；
 *   - 仅使用 createElement / textContent 构建安全 DOM 节点，绝不拼接原始 HTML 字符串；
 *   - 样式完全复用 tokens.css / base.css 主题 token。
 */
(function (root) {
  "use strict";

  var ACTION_LABELS = {
    none: "",
    retry: "重试",
    relogin: "重新登录",
    change_password: "修改密码",
    open_settings: "打开设置",
    open_permissions: "打开系统权限",
    contact_admin: "联系管理员"
  };

  var VALID_SEVERITIES = {
    info: true,
    warning: true,
    error: true,
    critical: true
  };

  var VALID_PRESENTATIONS = {
    inline: true,
    toast: true,
    banner: true,
    modal: true
  };

  function normalizeUiErrorObject(raw) {
    if (!raw || typeof raw !== "object") return null;
    if (typeof raw.code !== "string" || !raw.code.trim()) return null;
    var title = typeof raw.title === "string" ? raw.title.trim() : "";
    var message = typeof raw.message === "string" ? raw.message.trim() : "";
    if (!title && !message) return null;
    var severity =
      typeof raw.severity === "string" && VALID_SEVERITIES[raw.severity]
        ? raw.severity
        : "error";
    var presentation =
      typeof raw.presentation === "string" && VALID_PRESENTATIONS[raw.presentation]
        ? raw.presentation
        : "toast";
    var action = typeof raw.action === "string" ? raw.action : "none";
    var suggestion = typeof raw.suggestion === "string" ? raw.suggestion.trim() : "";
    var detail = typeof raw.detail === "string" && raw.detail.trim() ? raw.detail.trim() : null;
    var status = typeof raw.status === "number" && isFinite(raw.status) ? raw.status : null;
    return {
      code: raw.code.trim(),
      title: title || message,
      message: message || title,
      suggestion: suggestion,
      severity: severity,
      presentation: presentation,
      action: action,
      retryable: Boolean(raw.retryable),
      status: status,
      detail: detail
    };
  }

  function extractUiError(input) {
    if (!input) return null;
    if (typeof input === "string") {
      var trimmed = input.trim();
      if (trimmed.charAt(0) === "{" && trimmed.charAt(trimmed.length - 1) === "}") {
        try {
          return extractUiError(JSON.parse(trimmed));
        } catch (e) {
          return null;
        }
      }
      return null;
    }
    if (typeof input === "object") {
      if (input.uiError && typeof input.uiError === "object") {
        var fromCamel = normalizeUiErrorObject(input.uiError);
        if (fromCamel) return fromCamel;
      }
      if (input.ui_error && typeof input.ui_error === "object") {
        var fromSnake = normalizeUiErrorObject(input.ui_error);
        if (fromSnake) return fromSnake;
      }
      return normalizeUiErrorObject(input);
    }
    return null;
  }

  function actionLabel(action) {
    if (!action || typeof action !== "string") return "";
    return ACTION_LABELS[action] || "";
  }

  function formatSummary(input, fallback) {
    var ui = extractUiError(input);
    if (ui) {
      if (ui.message) return ui.message;
      if (ui.title) return ui.title;
    }
    if (typeof input === "string") {
      var t = input.trim();
      if (t) return t;
    } else if (input && typeof input === "object") {
      if (typeof input.message === "string" && input.message.trim()) {
        return input.message.trim();
      }
      if (typeof input.reason === "string" && input.reason.trim()) {
        return input.reason.trim();
      }
    }
    return fallback ? String(fallback) : "";
  }

  function formatToastText(input, fallback) {
    var ui = extractUiError(input);
    if (ui) {
      if (ui.title && ui.message && ui.message.indexOf(ui.title) === -1 && ui.title.length <= 16) {
        return ui.title + "：" + ui.message;
      }
      return ui.message || ui.title;
    }
    return formatSummary(input, fallback);
  }

  function buildErrorNode(doc, ui, kind, options) {
    var opts = options || {};
    var box = doc.createElement("div");
    var variantCls = opts.variant ? " rc-uierr--variant-" + opts.variant : "";
    box.className = "rc-uierr rc-uierr--" + kind + " rc-uierr--" + ui.severity + variantCls;
    box.setAttribute("role", ui.severity === "critical" || ui.severity === "error" ? "alert" : "status");

    var body = doc.createElement("div");
    body.className = "rc-uierr__body";

    if (ui.title && (!opts.omitTitle) && ui.title !== ui.message) {
      var titleEl = doc.createElement("div");
      titleEl.className = "rc-uierr__title";
      titleEl.textContent = ui.title;
      body.appendChild(titleEl);
    }

    if (ui.message && !opts.omitMessage) {
      var msgEl = doc.createElement("div");
      msgEl.className = "rc-uierr__msg";
      msgEl.textContent = ui.message;
      body.appendChild(msgEl);
    }

    if (ui.suggestion) {
      var sugEl = doc.createElement("div");
      sugEl.className = "rc-uierr__suggestion";
      sugEl.textContent = ui.suggestion;
      body.appendChild(sugEl);
    }

    var showDetail =
      ui.detail &&
      ui.detail !== ui.message &&
      ui.detail !== ui.title &&
      opts.hideDetail !== true;
    if (showDetail) {
      var detailsEl = doc.createElement("details");
      detailsEl.className = "rc-uierr__detail";
      var summaryEl = doc.createElement("summary");
      summaryEl.textContent = "查看诊断详情";
      var preEl = doc.createElement("pre");
      preEl.className = "rc-uierr__detail-text";
      preEl.textContent =
        "[" + ui.code + (ui.status ? " / HTTP " + ui.status : "") + "] " + ui.detail;
      detailsEl.appendChild(summaryEl);
      detailsEl.appendChild(preEl);
      body.appendChild(detailsEl);
    }

    box.appendChild(body);

    var label = actionLabel(ui.action);
    var hasAction = Boolean(label && typeof opts.onAction === "function");
    var hasDismiss = Boolean(opts.dismissible && typeof opts.onDismiss === "function");
    if (hasAction || hasDismiss) {
      var actionsEl = doc.createElement("div");
      actionsEl.className = "rc-uierr__actions";
      if (hasAction) {
        var actBtn = doc.createElement("button");
        actBtn.type = "button";
        actBtn.className = "rc-uierr__btn rc-uierr__btn--primary";
        actBtn.textContent = label;
        actBtn.addEventListener("click", function () {
          opts.onAction(ui.action, ui);
        });
        actionsEl.appendChild(actBtn);
      }
      if (hasDismiss) {
        var closeBtn = doc.createElement("button");
        closeBtn.type = "button";
        closeBtn.className = "rc-uierr__btn rc-uierr__btn--ghost";
        closeBtn.textContent = "关闭";
        closeBtn.addEventListener("click", function () {
          opts.onDismiss(ui);
        });
        actionsEl.appendChild(closeBtn);
      }
      box.appendChild(actionsEl);
    }

    return box;
  }

  function renderIntoContainer(container, input, kind, options) {
    if (!container) return false;
    var doc = container.ownerDocument || root.document;
    container.textContent = "";
    if (!input) {
      container.hidden = true;
      return false;
    }
    var ui = extractUiError(input);
    if (ui) {
      container.appendChild(buildErrorNode(doc, ui, kind, options));
      container.hidden = false;
      return true;
    }
    var text = formatSummary(input, "");
    if (!text) {
      container.hidden = true;
      return false;
    }
    if (kind === "banner") {
      var fallbackUi = {
        code: "common.unknown",
        title: "",
        message: text,
        suggestion: "",
        severity: (options && options.severity) || "warning",
        presentation: "banner",
        action: "none",
        retryable: false,
        status: null,
        detail: null
      };
      container.appendChild(buildErrorNode(doc, fallbackUi, "banner", options));
    } else {
      container.textContent = text;
    }
    container.hidden = false;
    return true;
  }

  function renderInline(container, input, options) {
    return renderIntoContainer(container, input, "inline", options);
  }

  function renderBanner(container, input, options) {
    return renderIntoContainer(container, input, "banner", options);
  }

  var activeModalBackdrop = null;
  var activeModalKeyHandler = null;
  var previousModalFocus = null;

  function channelLabel(channel) {
    if (channel === 1) return "文件传输";
    if (channel === 2) return "远程终端";
    if (channel === 3) return "端口映射";
    return "远程工具";
  }

  function isModalOpen() {
    return Boolean(activeModalBackdrop);
  }

  function closeModal() {
    if (activeModalKeyHandler && root.document) {
      root.document.removeEventListener("keydown", activeModalKeyHandler);
      activeModalKeyHandler = null;
    }
    if (activeModalBackdrop && activeModalBackdrop.parentNode) {
      activeModalBackdrop.parentNode.removeChild(activeModalBackdrop);
    }
    activeModalBackdrop = null;
    var toRestore = previousModalFocus;
    previousModalFocus = null;
    if (
      toRestore &&
      typeof toRestore.focus === "function" &&
      (!root.document || !root.document.contains || root.document.contains(toRestore))
    ) {
      try {
        toRestore.focus();
      } catch (e) {
        /* ignore */
      }
    }
  }

  function showModal(input, options) {
    var doc = root.document;
    if (!doc || !doc.body) return false;
    var opts = options || {};
    var ui = extractUiError(input) || {
      code: "common.unknown",
      title: opts.fallbackTitle || "操作失败",
      message: formatSummary(input, "发生未知错误"),
      suggestion: "",
      severity: "error",
      presentation: "modal",
      action: "none",
      retryable: false,
      status: null,
      detail: null
    };

    var priorFocus = activeModalBackdrop ? previousModalFocus : doc.activeElement;
    closeModal();
    previousModalFocus = priorFocus || null;

    var backdrop = doc.createElement("div");
    backdrop.className = "rc-uierr-modal-backdrop";

    var dialog = doc.createElement("div");
    dialog.className = "rc-uierr-modal rc-uierr--" + ui.severity;
    dialog.setAttribute("role", "alertdialog");
    dialog.setAttribute("aria-modal", "true");

    var titleEl = doc.createElement("h3");
    titleEl.className = "rc-uierr-modal__title";
    titleEl.textContent = ui.title || "提示";
    dialog.appendChild(titleEl);

    if (ui.message && ui.message !== ui.title) {
      var msgEl = doc.createElement("p");
      msgEl.className = "rc-uierr-modal__msg";
      msgEl.textContent = ui.message;
      dialog.appendChild(msgEl);
    }

    if (ui.suggestion) {
      var sugEl = doc.createElement("p");
      sugEl.className = "rc-uierr-modal__suggestion";
      sugEl.textContent = ui.suggestion;
      dialog.appendChild(sugEl);
    }

    if (ui.detail && ui.detail !== ui.message && ui.detail !== ui.title) {
      var detailsEl = doc.createElement("details");
      detailsEl.className = "rc-uierr__detail";
      var summaryEl = doc.createElement("summary");
      summaryEl.textContent = "查看诊断详情";
      var preEl = doc.createElement("pre");
      preEl.className = "rc-uierr__detail-text";
      preEl.textContent =
        "[" + ui.code + (ui.status ? " / HTTP " + ui.status : "") + "] " + ui.detail;
      detailsEl.appendChild(summaryEl);
      detailsEl.appendChild(preEl);
      dialog.appendChild(detailsEl);
    }

    var actionsEl = doc.createElement("div");
    actionsEl.className = "rc-uierr-modal__actions";

    var dismissBtn = doc.createElement("button");
    dismissBtn.type = "button";
    dismissBtn.className = "rc-uierr__btn rc-uierr__btn--ghost";
    dismissBtn.textContent = "知道了";
    dismissBtn.addEventListener("click", function () {
      closeModal();
      if (typeof opts.onDismiss === "function") opts.onDismiss(ui);
    });
    actionsEl.appendChild(dismissBtn);

    var focusTarget = dismissBtn;
    var label = actionLabel(ui.action);
    if (label && typeof opts.onAction === "function") {
      var primaryBtn = doc.createElement("button");
      primaryBtn.type = "button";
      primaryBtn.className = "rc-uierr__btn rc-uierr__btn--primary";
      primaryBtn.textContent = label;
      primaryBtn.addEventListener("click", function () {
        closeModal();
        opts.onAction(ui.action, ui);
      });
      actionsEl.appendChild(primaryBtn);
      focusTarget = primaryBtn;
    }

    dialog.appendChild(actionsEl);
    backdrop.appendChild(dialog);
    backdrop.addEventListener("click", function (ev) {
      if (ev.target === backdrop) {
        closeModal();
        if (typeof opts.onDismiss === "function") opts.onDismiss(ui);
      }
    });

    activeModalKeyHandler = function (ev) {
      if (ev.key === "Escape") {
        ev.preventDefault();
        closeModal();
        if (typeof opts.onDismiss === "function") opts.onDismiss(ui);
      }
    };
    doc.addEventListener("keydown", activeModalKeyHandler);
    doc.body.appendChild(backdrop);
    activeModalBackdrop = backdrop;
    if (focusTarget && typeof focusTarget.focus === "function") {
      try {
        focusTarget.focus();
      } catch (e) {
        /* ignore */
      }
    }
    return true;
  }

  function showToast(input, options) {
    var opts = options || {};
    var text = formatToastText(input, opts.fallback || "操作失败");
    if (!text) return false;
    if (typeof opts.toastFn === "function") {
      opts.toastFn(text);
      return true;
    }
    return false;
  }

  function present(input, options) {
    var opts = options || {};
    var ui = extractUiError(input);
    var presentation = opts.forcePresentation || (ui ? ui.presentation : "toast");

    if (presentation === "modal") {
      return showModal(ui || input, opts);
    }
    if (presentation === "banner" && opts.bannerEl) {
      return renderBanner(opts.bannerEl, ui || input, opts);
    }
    if (presentation === "inline" && opts.inlineEl) {
      return renderInline(opts.inlineEl, ui || input, opts);
    }
    if (opts.toastFn) {
      return showToast(ui || input, opts);
    }
    if (opts.bannerEl) {
      return renderBanner(opts.bannerEl, ui || input, opts);
    }
    if (opts.inlineEl) {
      return renderInline(opts.inlineEl, ui || input, opts);
    }
    return showModal(ui || input, opts);
  }

  var api = {
    extractUiError: extractUiError,
    actionLabel: actionLabel,
    channelLabel: channelLabel,
    formatSummary: formatSummary,
    formatToastText: formatToastText,
    renderInline: renderInline,
    renderBanner: renderBanner,
    showToast: showToast,
    showModal: showModal,
    closeModal: closeModal,
    isModalOpen: isModalOpen,
    present: present
  };

  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
  root.RCErrorDisplay = api;
})(typeof window !== "undefined" ? window : globalThis);
