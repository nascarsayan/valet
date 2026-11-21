# zsh's config lives in its app folder; nothing else in the home dir. Every
# host reads this file, so the folder is found at run time, in this host's
# spelling (as env-common.sh finds $USERLAND; :A costs no fork).
_ul=${${:-{{ userland_seed }}}:A}
export ZDOTDIR="$_ul/Applications/Zsh/Settings"
unset _ul
source "$ZDOTDIR/.zshenv"
