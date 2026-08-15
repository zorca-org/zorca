#!/usr/bin/env sh
set -eu

main() {
    if [ "$(uname -s)" != "Linux" ]; then
        echo "This helper installs a locally built Linux bundle. On macOS, use ./script/bundle-mac -i." >&2
        exit 1
    fi

    bundle_path="${ZORCA_BUNDLE_PATH:-}"
    if [ -z "$bundle_path" ] || [ ! -f "$bundle_path" ]; then
        echo "Set ZORCA_BUNDLE_PATH to a ZOrca Linux bundle created by ./script/bundle-linux." >&2
        exit 1
    fi

    channel="${ZORCA_CHANNEL:-stable}"
    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    app_id="dev.zorca.ZOrca"
    case "$channel" in
        stable) ;;
        nightly) app_id="dev.zorca.ZOrca-Nightly" ;;
        preview) app_id="dev.zorca.ZOrca-Preview" ;;
        dev) app_id="dev.zorca.ZOrca-Dev" ;;
        *)
            echo "Unknown release channel: $channel" >&2
            exit 1
            ;;
    esac

    app_dir="$HOME/.local/zorca$suffix.app"
    rm -rf "$app_dir"
    tar -xzf "$bundle_path" -C "$HOME/.local/"

    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
    ln -sf "$app_dir/bin/zorca" "$HOME/.local/bin/zorca"

    desktop_file="$HOME/.local/share/applications/$app_id.desktop"
    cp "$app_dir/share/applications/$app_id.desktop" "$desktop_file"
    sed -i "s|Icon=zorca|Icon=$app_dir/share/icons/hicolor/512x512/apps/zorca.png|g" "$desktop_file"
    sed -i "s|Exec=zorca|Exec=$app_dir/bin/zorca|g" "$desktop_file"

    echo "ZOrca has been installed. Run it with 'zorca'."
}

main "$@"
