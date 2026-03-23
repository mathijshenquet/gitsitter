# gitsitter shell hook
function __gitsitter_hook {
    $command = Get-Command gitsitter -ErrorAction SilentlyContinue
    if ($null -eq $command) {
        return
    }

    $msg = & gitsitter _prompt 2>$null
    if (-not [string]::IsNullOrWhiteSpace($msg)) {
        Write-Host $msg
    }
}

if ($null -eq $function:__gitsitter_prompt_installed) {
    $function:__gitsitter_prompt_installed = $true
    $function:prompt_original = $function:prompt

    function global:prompt {
        __gitsitter_hook
        if ($null -ne $function:prompt_original) {
            & $function:prompt_original
        }
    }
}
