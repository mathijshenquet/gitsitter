# gitsitter shell hook
__gitsitter_hook() {
    if (( $+commands[gitsitter] )); then
        gitsitter register &>/dev/null &!
        local msg
        msg=$(gitsitter _prompt 2>/dev/null)
        if [[ -n "$msg" ]]; then
            echo "$msg"
        fi
    fi
}

if [[ -z "${precmd_functions[(r)__gitsitter_hook]}" ]]; then
    precmd_functions+=(__gitsitter_hook)
fi
