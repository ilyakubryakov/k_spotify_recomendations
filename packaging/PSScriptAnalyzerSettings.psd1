# PSScriptAnalyzer configuration for packaging/install.ps1.
#
# Rules are excluded here, with a reason, rather than by lowering the severity
# threshold in CI -- so every exclusion is visible and arguable, and any *new*
# warning still fails the build.
@{
    Severity     = @('Error', 'Warning')

    ExcludeRules = @(
        # The script's entire job is to talk to a person standing at a
        # terminal. Write-Output would put installer chatter on the success
        # stream, so `$result = .\install.ps1` would capture progress text
        # instead of nothing. Write-Host is correct here; the rule is aimed at
        # reusable modules.
        'PSAvoidUsingWriteHost',

        # False positive: -NoVerify is read inside Test-Checksum. The analyser
        # does not follow parameter use across function boundaries.
        'PSReviewUnusedParameter',

        # Remove-AgentTask and Remove-FromUserPath are private helpers inside a
        # script that exists to change machine state, and the destructive path
        # is already gated behind an explicit -Uninstall. Threading
        # SupportsShouldProcess through internal helpers would add -WhatIf
        # plumbing that nothing calls.
        'PSUseShouldProcessForStateChangingFunctions'
    )
}
