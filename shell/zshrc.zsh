# Sourced at the end of ~/.zshrc.

# Userland first again (.zshrc prepends its own dirs) before anything below
# looks a command up.
[[ -n ${USERLAND_ARCH:-} ]] && path=("$USERLAND/Commands/$USERLAND_ARCH" "$USERLAND/Scripts" $path)

# `valet shell-init zsh`, cached and compiled by `valet hooks install` (zsh
# loads the .zwc beside it); its completion is autoloaded from completions/.
# Live when the cache is missing.
# Top level, not in a function: the snippets' assignments must stay global.
if [[ -r "{{ generated }}/shell-init.zsh" ]]; then
  source "{{ generated }}/shell-init.zsh"
  fpath=("{{ generated }}/completions" $fpath)
  autoload -Uz _{{ name }} && compdef _{{ name }} {{ name }}
elif (( $+commands[{{ name }}] )); then
  eval "$(command {{ name }} shell-init zsh)"
fi

# Last word on PATH: no tool homes under $HOME besides Userland (old
# installs, ~/.local/bin, ~/bin), no missing dirs. System dirs from /etc and
# other dirs outside $HOME stay.
if [[ -n ${USERLAND_ARCH:-} ]]; then
  () {
    local d; local -a keep
    for d in "$USERLAND/Commands/$USERLAND_ARCH" "$USERLAND/Scripts" ${path:#($HOME|${HOME:A})/*}; do
      [[ -d $d ]] && keep+=("$d")
    done
    path=($keep)
  }
fi

# History lives in atuin alone (Ctrl-R searches it). zsh keeps this shell's
# commands in memory for up-arrow and !!, and writes no history file. zsh
# reads HISTFILE only after all startup files, so this overrides the template.
unset HISTFILE
SAVEHIST=0
HISTSIZE=10000
