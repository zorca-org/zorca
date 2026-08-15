#!/usr/bin/env sh
set -eu

prompt_remove_preferences() {
    printf "Do you want to keep your ZOrca preferences? [Y/n] "
    read -r response
    case "$response" in
        [nN]|[nN][oO])
            rm -rf "$HOME/.config/zorca"
            echo "Preferences removed."
            ;;
        *) echo "Preferences kept." ;;
    esac
}

linux() {
    rm -rf "$HOME/.local/zorca"*.app
    rm -f "$HOME/.local/bin/zorca"
    rm -f "$HOME/.local/share/applications/dev.zorca.ZOrca"*.desktop
    rm -rf "$HOME/.local/share/zorca"
    rm -rf "$HOME/.zed_server"
    prompt_remove_preferences
}

macos() {
    rm -rf "/Applications/ZOrca.app"
    rm -f "$HOME/.local/bin/zorca"
    rm -rf "$HOME/Library/Application Support/ZOrca"
    rm -rf "$HOME/Library/Logs/ZOrca"
    rm -rf "$HOME/Library/Caches/dev.zorca.ZOrca"
    rm -rf "$HOME/Library/HTTPStorages/dev.zorca.ZOrca"
    rm -f "$HOME/Library/Preferences/dev.zorca.ZOrca.plist"
    rm -rf "$HOME/Library/Saved Application State/dev.zorca.ZOrca.savedState"
    rm -rf "$HOME/.zed_server"
    prompt_remove_preferences
}

case "$(uname -s)" in
    Darwin) macos ;;
    Linux) linux ;;
    *)
        echo "Unsupported platform: $(uname -s)" >&2
        exit 1
        ;;
esac

echo "ZOrca has been uninstalled."
