# Sourced from ~/.zshenv for every zsh, so only assignments.
# Personal values and secrets are not here: `valet shell-init` exports them
# from `valet vars`.

{% include "env-common.sh" %}
# Userland ($USERLAND): every custom tool, one folder per app, per CPU
# architecture; `valet apps` fills Commands/<Arch>. $CPUTYPE costs no fork.
case $CPUTYPE in
  aarch64|arm64) export USERLAND_ARCH=Arm64 ;;
  *) export USERLAND_ARCH=Amd64 ;;
esac
# Host name ($HOST: no fork) for per-host app state ({Host} in config.toml).
export USERLAND_HOST="$HOST"
[[ -r "{{ generated }}/environment.sh" ]] && source "{{ generated }}/environment.sh"
# PATH has one home for custom tools: Commands/<Arch> (links made by `valet
# apps`). Rebuilt from scratch, so entries inherited from long-lived parents
# (herdr, tmux, older shells) never pile up; /etc/profile adds system dirs.
typeset -gU path
path=("$USERLAND/Commands/$USERLAND_ARCH" "$USERLAND/Scripts" /usr/local/bin /usr/bin /usr/local/sbin /usr/sbin)
