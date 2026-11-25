#!/usr/bin/env bash
# macOS 原生打包脚本：直接产出签名好的 RabbitControl.app 与拖拽安装用 .dmg
#
# 特点：
#   - 零 npm / 零 cargo-tauri 外部工具依赖（纯 cargo + 系统 codesign / hdiutil）
#   - 单一事实源：自动读取 crates/rc-ui/tauri.conf.json 的 productName / version / identifier
#   - 优先使用登录钥匙串中的固定证书 "RabbitControl Dev" 签名（重编译不掉 macOS TCC 权限），
#     找不到该证书时自动回退 ad-hoc 签名（"-"），也可通过 --sign 显式指定证书名
#
# 用法：
#   ./scripts/package-macos.sh                 # 默认 release，产出 .app + .dmg
#   ./scripts/package-macos.sh --debug         # debug 快速打包
#   ./scripts/package-macos.sh --no-dmg        # 只产出 .app
#   ./scripts/package-macos.sh --sign "-"      # 强制 ad-hoc 签名（无开发证书机器用）
#   ./scripts/package-macos.sh --out ./dist    # 指定输出目录（默认 target/dist）

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UI_DIR="$REPO_ROOT/crates/rc-ui"
TAURI_CONF="$UI_DIR/tauri.conf.json"
ICON_ICNS="$UI_DIR/icons/icon.icns"

PROFILE="release"
MAKE_DMG=1
SIGN_ID="${RC_DEV_CERT:-}"
SIGN_KEYCHAIN="${RC_SIGN_KEYCHAIN:-}"
OUT_DIR="$REPO_ROOT/target/dist"
TARGET_TRIPLE=""
VERSION_OVERRIDE=""

die() {
    echo "错误: $*" >&2
    exit 1
}

usage() {
    cat << 'EOF'
用法: ./scripts/package-macos.sh [选项]

选项:
  --release           构建 release 包（默认）
  --debug             构建 debug 包
  --no-dmg            仅生成 .app，不打包 .dmg
  --sign <身份>       指定 codesign 签名身份（如 "RabbitControl Dev"、"Developer ID Application: ..." 或 "-"）
  --keychain <路径>   指定 codesign 使用的钥匙串路径（CI 临时钥匙串用）
  --out <目录>        指定输出目录（默认 target/dist）
  --target <triple>   指定 Cargo 目标三元组（如 aarch64-apple-darwin、x86_64-apple-darwin）
  --version <版本>    覆盖应用版本号与 DMG 文件名版本（自动去除可选前导 v）
  -h, --help          显示帮助
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release) PROFILE="release"; shift ;;
        --debug) PROFILE="debug"; shift ;;
        --no-dmg) MAKE_DMG=0; shift ;;
        --sign)
            [[ $# -ge 2 ]] || die "--sign 需要提供签名身份"
            SIGN_ID="$2"
            shift 2
            ;;
        --keychain)
            [[ $# -ge 2 ]] || die "--keychain 需要提供钥匙串路径"
            SIGN_KEYCHAIN="$2"
            shift 2
            ;;
        --out)
            [[ $# -ge 2 ]] || die "--out 需要提供输出目录"
            OUT_DIR="$2"
            shift 2
            ;;
        --target)
            [[ $# -ge 2 ]] || die "--target 需要提供 Cargo target triple"
            TARGET_TRIPLE="$2"
            shift 2
            ;;
        --version)
            [[ $# -ge 2 ]] || die "--version 需要提供版本号"
            VERSION_OVERRIDE="$2"
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) die "未知参数: $1（可用 --help 查看用法）" ;;
    esac
done

[[ "$(uname -s)" == "Darwin" ]] || die "本脚本仅支持在 macOS 上运行"
[[ -f "$TAURI_CONF" ]] || die "找不到 $TAURI_CONF"
[[ -f "$ICON_ICNS" ]] || die "找不到图标文件 $ICON_ICNS"

# 从 tauri.conf.json 读取应用元信息
read -r PRODUCT_NAME APP_VERSION BUNDLE_ID MIN_MACOS < <(
    python3 -c '
import json, sys
cfg = json.load(open(sys.argv[1], encoding="utf-8"))
print(
    cfg.get("productName", "RabbitControl"),
    cfg.get("version", "0.1.0"),
    cfg.get("identifier", "com.rabbitcontrol.dev"),
    cfg.get("bundle", {}).get("macOS", {}).get("minimumSystemVersion", "13.0"),
)
' "$TAURI_CONF"
)

if [[ -n "$VERSION_OVERRIDE" ]]; then
    APP_VERSION="${VERSION_OVERRIDE#v}"
fi
[[ -n "$APP_VERSION" ]] || die "版本号不能为空"

if [[ -n "$TARGET_TRIPLE" ]]; then
    case "$TARGET_TRIPLE" in
        aarch64-apple-darwin) ARCH="arm64" ;;
        x86_64-apple-darwin) ARCH="x86_64" ;;
        *) ARCH="$TARGET_TRIPLE" ;;
    esac
else
    ARCH="$(uname -m)"
fi

if [[ -z "$SIGN_ID" ]]; then
    if security find-identity -p codesigning 2>/dev/null | grep -Fq '"RabbitControl Dev"'; then
        SIGN_ID="RabbitControl Dev"
    else
        SIGN_ID="-"
    fi
fi

echo "==> 构建 $PRODUCT_NAME v$APP_VERSION ($PROFILE, $ARCH)"
GIT_SHA="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || true)"
BUILD_ARGS=(-p rc-ui --bin rc-ui --bin rc-daemon --bin rc-host-agent)
if [[ -n "$TARGET_TRIPLE" ]]; then
    BUILD_ARGS+=(--target "$TARGET_TRIPLE")
fi
if [[ "$PROFILE" == "release" ]]; then
    BUILD_ARGS+=(--release)
fi
(cd "$REPO_ROOT" && RC_APP_VERSION="$APP_VERSION" RC_GIT_SHA="$GIT_SHA" cargo build "${BUILD_ARGS[@]}")

if [[ -n "$TARGET_TRIPLE" ]]; then
    BIN_DIR="$REPO_ROOT/target/$TARGET_TRIPLE/$PROFILE"
else
    BIN_DIR="$REPO_ROOT/target/$PROFILE"
fi
SRC_BIN="$BIN_DIR/rc-ui"
DAEMON_BIN="$BIN_DIR/rc-daemon"
AGENT_BIN="$BIN_DIR/rc-host-agent"
[[ -x "$SRC_BIN" ]] || die "构建产物不存在: $SRC_BIN"
[[ -x "$DAEMON_BIN" ]] || die "构建产物不存在: $DAEMON_BIN"
[[ -x "$AGENT_BIN" ]] || die "构建产物不存在: $AGENT_BIN"

mkdir -p "$OUT_DIR"
APP_BUNDLE="$OUT_DIR/${PRODUCT_NAME}.app"
echo "==> 组装应用包: $APP_BUNDLE"

rm -rf "$APP_BUNDLE"
mkdir -p "$APP_BUNDLE/Contents/MacOS" "$APP_BUNDLE/Contents/Resources"

cp "$SRC_BIN" "$APP_BUNDLE/Contents/MacOS/$PRODUCT_NAME"
cp "$DAEMON_BIN" "$APP_BUNDLE/Contents/MacOS/rc-daemon"
cp "$AGENT_BIN" "$APP_BUNDLE/Contents/MacOS/rc-host-agent"
chmod +x \
    "$APP_BUNDLE/Contents/MacOS/$PRODUCT_NAME" \
    "$APP_BUNDLE/Contents/MacOS/rc-daemon" \
    "$APP_BUNDLE/Contents/MacOS/rc-host-agent"
cp "$ICON_ICNS" "$APP_BUNDLE/Contents/Resources/icon.icns"
printf 'APPL????' > "$APP_BUNDLE/Contents/PkgInfo"

cat > "$APP_BUNDLE/Contents/Resources/com.rabbitcontrol.daemon.plist.template" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.rabbitcontrol.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/${PRODUCT_NAME}.app/Contents/MacOS/rc-daemon</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RC_DAEMON_MODE</key>
        <string>1</string>
        <key>RC_CONFIG_DIR</key>
        <string>/Library/Application Support/RabbitControl</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>PathState</key>
        <dict>
            <key>/Library/Application Support/RabbitControl/run/manual-stop</key>
            <false/>
        </dict>
    </dict>
    <key>ProcessType</key>
    <string>Adaptive</string>
    <key>StandardOutPath</key>
    <string>/Library/Application Support/RabbitControl/logs/daemon.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>/Library/Application Support/RabbitControl/logs/daemon.stderr.log</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>${BUNDLE_ID}</string>
    </array>
</dict>
</plist>
EOF

cat > "$APP_BUNDLE/Contents/Resources/com.rabbitcontrol.agent.plist.template" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.rabbitcontrol.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/${PRODUCT_NAME}.app/Contents/MacOS/rc-host-agent</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RC_DAEMON_MODE</key>
        <string>1</string>
        <key>RC_CONFIG_DIR</key>
        <string>/Library/Application Support/RabbitControl</string>
    </dict>
    <key>LimitLoadToSessionType</key>
    <array>
        <string>LoginWindow</string>
        <string>Aqua</string>
    </array>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>PathState</key>
        <dict>
            <key>/Library/Application Support/RabbitControl/run/manual-stop</key>
            <false/>
        </dict>
    </dict>
    <key>StandardOutPath</key>
    <string>/Library/Application Support/RabbitControl/logs/agent.stdout.log</string>
    <key>StandardErrorPath</key>
    <string>/Library/Application Support/RabbitControl/logs/agent.stderr.log</string>
    <key>AssociatedBundleIdentifiers</key>
    <array>
        <string>${BUNDLE_ID}</string>
    </array>
</dict>
</plist>
EOF

cat > "$APP_BUNDLE/Contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>zh_CN</string>
    <key>CFBundleDisplayName</key>
    <string>${PRODUCT_NAME}</string>
    <key>CFBundleExecutable</key>
    <string>${PRODUCT_NAME}</string>
    <key>CFBundleIconFile</key>
    <string>icon</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${PRODUCT_NAME}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${APP_VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${APP_VERSION}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.utilities</string>
    <key>LSMinimumSystemVersion</key>
    <string>${MIN_MACOS}</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>NSScreenCaptureUsageDescription</key>
    <string>RabbitControl 需要屏幕录制权限以在被控端采集本机画面与系统音频。</string>
    <key>NSAudioCaptureUsageDescription</key>
    <string>RabbitControl 需要系统音频采集权限以将本机音频传输给控制端。</string>
    <key>NSLocalNetworkUsageDescription</key>
    <string>RabbitControl 需要本地网络权限以发现局域网内的设备并建立低延迟直连。</string>
</dict>
</plist>
EOF

echo "==> 代码签名 (identity=$SIGN_ID, bundle_id=$BUNDLE_ID)"
CODESIGN_KEYCHAIN_ARGS=()
if [[ -n "$SIGN_KEYCHAIN" ]]; then
    CODESIGN_KEYCHAIN_ARGS=(--keychain "$SIGN_KEYCHAIN")
fi
codesign --force ${CODESIGN_KEYCHAIN_ARGS[@]+"${CODESIGN_KEYCHAIN_ARGS[@]}"} --sign "$SIGN_ID" --identifier "${BUNDLE_ID}.daemon" "$APP_BUNDLE/Contents/MacOS/rc-daemon"
codesign --force ${CODESIGN_KEYCHAIN_ARGS[@]+"${CODESIGN_KEYCHAIN_ARGS[@]}"} --sign "$SIGN_ID" --identifier "${BUNDLE_ID}.agent" "$APP_BUNDLE/Contents/MacOS/rc-host-agent"
codesign --force ${CODESIGN_KEYCHAIN_ARGS[@]+"${CODESIGN_KEYCHAIN_ARGS[@]}"} --sign "$SIGN_ID" --identifier "$BUNDLE_ID" "$APP_BUNDLE"
codesign --verify --deep --strict "$APP_BUNDLE"

DMG_PATH=""
if [[ "$MAKE_DMG" -eq 1 ]]; then
    DMG_PATH="$OUT_DIR/${PRODUCT_NAME}-${APP_VERSION}-${ARCH}.dmg"
    STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rc-dmg-stage.XXXXXX")"
    trap 'rm -rf "$STAGE_DIR"' EXIT

    cp -R "$APP_BUNDLE" "$STAGE_DIR/"
    ln -s /Applications "$STAGE_DIR/Applications"

    rm -f "$DMG_PATH"
    echo "==> 生成 DMG 安装镜像: $DMG_PATH"
    for attempt in 1 2 3; do
        if hdiutil create \
            -volname "$PRODUCT_NAME" \
            -srcfolder "$STAGE_DIR" \
            -ov \
            -format UDZO \
            "$DMG_PATH" >/dev/null; then
            break
        fi
        if [[ "$attempt" -eq 3 ]]; then
            die "hdiutil create 连续失败 3 次: $DMG_PATH"
        fi
        echo "hdiutil create 第 ${attempt} 次失败，2 秒后重试..." >&2
        rm -f "$DMG_PATH"
        sleep 2
    done
    rm -rf "$STAGE_DIR"
    trap - EXIT
fi

echo ""
echo "打包完成："
echo "  App: $APP_BUNDLE ($(du -sh "$APP_BUNDLE" | awk '{print $1}'))"
if [[ -n "$DMG_PATH" ]]; then
    echo "  DMG: $DMG_PATH ($(du -sh "$DMG_PATH" | awk '{print $1}'))"
fi
echo "  签名: $SIGN_ID ($BUNDLE_ID)"
