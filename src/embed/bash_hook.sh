# gitsitter shell hook
__gitsitter_hook() {
    if command -v gitsitter &>/dev/null; then
        gitsitter register &>/dev/null &
        disown 2>/dev/null
        local msg
        msg=$(gitsitter _prompt 2>/dev/null)
        if [ -n "$msg" ]; then
            echo "$msg"
        fi
    fi
}

if [[ ! "$PROMPT_COMMAND" == *"__gitsitter_hook"* ]]; then
    PROMPT_COMMAND="__gitsitter_hook${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
fi
