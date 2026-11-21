# Environment every shell gets, zsh (zshenv) and bash (bashrc) alike: plain
# POSIX assignments, set unconditionally so every shell agrees whatever its
# parent.

# Userland in one spelling: symlinks resolved. Every path valet prints or
# exports uses it; files that hosts share name only $USERLAND. Agents treat
# two spellings of one file as two files.
_ul=$(cd "{{ userland_seed }}" 2>/dev/null && pwd -P)
export USERLAND="${_ul:-{{ userland_seed }}}"
unset _ul
# Tools without a variable of their own keep their files in Userland's
# Library (macOS ~/Library), never in the home dir. Apps with their own
# variables get their app folders (environment.sh).
export XDG_CONFIG_HOME="$USERLAND/Library/Preferences"
export XDG_DATA_HOME="$USERLAND/Library/ApplicationSupport"
export XDG_CACHE_HOME="$USERLAND/Library/Caches"
export XDG_STATE_HOME="$USERLAND/Library/State"
export EDITOR=vim VISUAL=vim
# rustup's proxies pin their children to one toolchain with this; a shell
# started under one (a herdr server, an editor) must not keep the pin.
unset RUSTUP_TOOLCHAIN
