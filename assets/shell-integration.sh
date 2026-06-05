# WSL Terminal — shell integration for bash and zsh.
#
# Emits the escape sequences the terminal understands:
#   OSC 7    — report the current directory (new tabs/splits inherit it)
#   OSC 133  — semantic prompt marks: A = prompt start, D;<exit> = command done
#              (powers Ctrl+Shift+Up/Down "jump to prompt" and the red
#               failed-command ticks in the scrollbar)
#
# Enable it by sourcing this file from your shell rc. For example, copy it into
# WSL and add to ~/.bashrc and/or ~/.zshrc:
#
#     [ -f ~/.config/wslterm/shell-integration.sh ] && . ~/.config/wslterm/shell-integration.sh
#
# It is safe to source unconditionally: it no-ops for non-interactive shells,
# for shells other than bash/zsh, and (optionally) outside WSL Terminal.

# Interactive shells only.
case $- in
    *i*) ;;
    *) return 0 2>/dev/null || exit 0 ;;
esac

# Load once.
[ -n "${__WSLTERM_INTEGRATION:-}" ] && return 0 2>/dev/null
__WSLTERM_INTEGRATION=1

# Optional: only activate inside WSL Terminal (wslptyd sets WSLTERM=1). Comment
# out the next two lines to always activate (e.g. to test under another terminal).
[ -z "${WSLTERM:-}" ] && return 0 2>/dev/null

# Runs just before each prompt: report the prior command's exit (OSC 133;D), the
# current directory (OSC 7), then mark the new prompt (OSC 133;A). Preserves $?.
__wslterm_mark() {
    local ec=$?
    printf '\033]133;D;%d\007' "$ec"
    printf '\033]7;file://%s%s\007' "${HOSTNAME:-${HOST:-}}" "$PWD"
    printf '\033]133;A\007'
    return $ec
}

if [ -n "${ZSH_VERSION:-}" ]; then
    autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook precmd __wslterm_mark
elif [ -n "${BASH_VERSION:-}" ]; then
    case "${PROMPT_COMMAND:-}" in
        *__wslterm_mark*) ;;                                  # already installed
        "") PROMPT_COMMAND="__wslterm_mark" ;;
        *) PROMPT_COMMAND="__wslterm_mark;${PROMPT_COMMAND}" ;;
    esac
fi
