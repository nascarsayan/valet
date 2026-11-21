# Interactive zsh config. Kept as .zsh so `zsh -n` can check it.

mkcd() {
  if [ $# -eq 0 ]; then
    printf 'usage: mkcd <directory>\n' >&2
    return 1
  fi
  mkdir -p -- "$1" && cd -- "$1"
}

if (( $+commands[eza] )); then
  alias ls='eza -lh --group-directories-first --icons=auto'
fi

DISABLE_MAGIC_FUNCTIONS=true
DISABLE_UPDATE_PROMPT=true

# fzf's zsh integration.
#
# It snapshots $options and later restores it with `eval 'options=(...)'`.
# Assigning that array re-applies `privileged off`, and zsh implements that by
# calling setgid/setuid/setgroups -- which the user namespace denies when running
# as root, so every shell starts with two copies of
#
#   (eval):unsetopt:1: can't drop privileges; failed to set supplementary group
#   list: operation not permitted
#   (eval):1: can't change option: privileged
#
# Purely cosmetic: fzf loads fine either way. Filtered rather than blanket
# silenced, so any other fzf error still reaches the terminal. Only for root --
# an ordinary user never hits it.
if (( $+commands[fzf] )); then
  # Ctrl-R stays with the history manager; fzf keeps Ctrl-T and Alt-C.
  export FZF_CTRL_R_COMMAND=""
  if (( EUID == 0 )); then
    source <(fzf --zsh) 2> >(grep -v \
      -e "can't drop privileges" \
      -e "can't change option: privileged" >&2)
  else
    _valet_init fzf fzf --zsh
  fi
fi

zstyle ':completion:*' menu select
zstyle ':completion:*:descriptions' format '%d'

WORDCHARS=''
bindkey -e

# Terminal title = short host name at each prompt: herdr labels each tab by
# it (local or remote shell), and a tab goes back to it when a program that
# set its own title (Claude Code) exits.
if [[ $TERM != dumb ]]; then
  _valet_title() { print -Pn '\e]0;%m\a' }
  autoload -Uz add-zsh-hook
  add-zsh-hook precmd _valet_title
  # Before a plain `ssh`, the host it connects to (`ssh -G` resolves
  # aliases); the next prompt sets it back.
  _valet_ssh_title() {
    local -a w=(${(z)1})
    [[ ${w[1]} == ssh ]] || return
    local h=$(command ssh -G ${(Q)w[2,-1]} 2>/dev/null | awk '$1=="hostname"{print $2; exit}')
    [[ -n $h ]] && print -n "\e]0;${h%%.*}\a"
  }
  add-zsh-hook preexec _valet_ssh_title
fi
