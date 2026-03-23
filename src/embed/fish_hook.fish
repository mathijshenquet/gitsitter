# gitsitter shell hook
function __gitsitter_hook --on-event fish_prompt
    if command -q gitsitter
        gitsitter register &>/dev/null &
        set -l msg (gitsitter _prompt 2>/dev/null)
        if test -n "$msg"
            echo $msg
        end
    end
end
