#Requires -Version 5.1
<#
.SYNOPSIS
    Rebuild the local/release-merge-wasm32 and local/release-merge-wasm64
    integration branches.

.DESCRIPTION
    Rebuilds both integration branches on the local `master` branch:
      1. local/release-merge-wasm32 = master + all feature branches
         (merged in a defined order, with the CI-gate rustfmt+clippy run).
      2. local/release-merge-wasm64 = local/release-merge-wasm32 +
         add-wasm64-target (single-merge chain, no additional CI gate).
    PowerShell 5.1+ compatible.

    Both integration branches are INTENTIONALLY LOCAL-ONLY (the `local/`
    prefix is by convention). They are build artefacts, not shared branches:
    every run rewrites their history from scratch. Publishable release state
    lives on annotated tags (`enterprise-vX.Y.Z-wasm32` / `-wasm64`)
    applied to a validated integration tip, not on the integration branch
    itself.

    Scope / assumptions (deliberately minimal):
      * The script performs NO state verification (working tree, current
        branch, remote configuration, feature-branch existence) and NO
        implicit fetch/pull. It is the caller's responsibility to make sure
        `master` and the `origin/*` feature-branch refs are at the desired
        commits before invoking.
      * The script does NOT touch `master` (no fetch/ff/push) and does NOT
        touch `ruffle-enterprise` (the fork base). It only rewrites the
        two integration branches.
      * The script does NOT push anywhere. Local artefacts only; publish
        via release tags applied manually after validating each build.
      * The script may be launched from any working directory and from any
        active branch. It anchors on the git repo root via
        `git rev-parse --show-toplevel`.

    Branch topology (produced by this script):
        master (local, unchanged)
            | (reset --hard + merge features)
            v
        local/release-merge-wasm32 (rebuilt fresh every run, local-only)
            | (reset --hard + merge add-wasm64-target)
            v
        local/release-merge-wasm64 (rebuilt fresh every run, local-only)

    Merge strategy:
        Each feature branch is merged as `origin/<branch>` with `--no-ff`,
        producing an explicit merge commit named "Integrate <branch>".
        git rerere is enabled so recurring conflict resolutions
        (e.g. xml-general vs xml-readonly) are replayed automatically.

    On merge conflict the script fails fast. Resolve manually (edit files,
    git add, git commit) and re-run -- rerere will replay the resolution on
    the next run.

.PARAMETER DryRun
    Print each git/cargo command instead of executing it. No side effects.

.PARAMETER NoChecks
    Skip the cargo fmt / clippy CI-gate checks at the end.

.EXAMPLE
    powershell.exe -File .\scripts\Rebuild-EnterpriseIntegration.ps1 -DryRun
    Show the sequence without touching anything.

.EXAMPLE
    powershell.exe -File .\scripts\Rebuild-EnterpriseIntegration.ps1
    Rebuild both integration branches and run the CI-gate checks.
    After validating each build, tag the release with:
        git tag -a enterprise-vX.Y.Z-wasm32 -m "Enterprise release X.Y.Z (wasm32)"
        git tag -a enterprise-vX.Y.Z-wasm64 -m "Enterprise release X.Y.Z (wasm64)"
        git push origin enterprise-vX.Y.Z-wasm32 enterprise-vX.Y.Z-wasm64

.NOTES
    Companion: scripts/rebuild-enterprise-integration.sh (bash version).
    Both scripts implement the same MERGE_ORDER and must stay in sync.
#>

[CmdletBinding()]
param(
    [switch]$DryRun,
    [switch]$NoChecks
)

$ErrorActionPreference = 'Stop'

# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------

$IntegrationBranch = 'local/release-merge-wasm32'
$MasterBranch      = 'master'
$OriginRemote      = 'origin'

# Merge order
$MergeOrder = @(
    'fix-memory-leaks',                        # bug vari che causano memory leaks
    'fix-blurry-grid-separators',              # bug su rendering blurry di linee orizzontali e verticali
    'fix-mouse-events-clipping',               # bug su eventi mouse non clippati sul container host
    'fix-mouse-hover-on-mx-buttons',           # bug su eventi hover non corretti
    'fix-mouse-click-release-outside-buttons', # bug su eventi di rilascio mouse fuori dal container 
    'fix-bitmapdata-filter-expansion',         # bug su ombreggiatura dei toolTip
    'fix-collator-compare-normalization',      # bug su sort array che va in loop su s:DataGrid    
    'fix-arcgis-flex-sdk-too-much-recursion',  # bug su mappe arcgis che vanno in ricorsione    
    'add-text-input-event-dispatch',           # implementa dispaccio eventi mouse su oggetti non text 
    'add-mouse-cursor-property',               # implementa impossibilità di ridefinire cursore mouse    
    'add-urlstream-improvements',              # implementa URLStream con funzionalità estese (fork only)
    'add-font-text-engine-implementation',     # implementa api FTE
    'add-export-instruments',                  # ottimizzazioni varie su XML 
    'tile-memory-allocator',                   # aggiunge nuovo memory allocator ottimizzato
    'pluggable-font-renderer'                  # aggiunge custom renderer esternalizzazbile su plugin
)

# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------

function Write-Step { param($Message) Write-Host ''; Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok   { param($Message) Write-Host "[OK] $Message" -ForegroundColor Green }
function Write-Warn { param($Message) Write-Host ''; Write-Host "[WARN] $Message" -ForegroundColor Yellow }
function Write-Err  { param($Message) Write-Host ''; Write-Host "[ERR] $Message" -ForegroundColor Red }

function Invoke-Git {
    [CmdletBinding()]
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$GitArgs)

    $cmd = 'git ' + ($GitArgs -join ' ')
    if ($script:DryRun) {
        Write-Host "  (dry-run) $cmd" -ForegroundColor DarkGray
        return
    }
    Write-Host "  -> $cmd" -ForegroundColor DarkGray

    # Retry on git's generic fatal exit (128), which includes transient
    # .git/index.lock contention from other git tools (SmartGit, VSCode
    # integration, indexers) polling the repo while our merges run.
    #
    # Race note: Test-Path on .git/index.lock is unreliable — the lock
    # holder may release the file BETWEEN git failing and our check, so
    # we retry on exit code alone. Real merge conflicts exit with 1 (not
    # 128), so this does NOT retry legitimate conflicts. Other exit-128
    # errors (missing ref, corrupt repo) fail identically on retry and
    # throw after the retry budget is exhausted.
    $lockPath = Join-Path $script:repoRoot '.git\index.lock'
    $maxLockRetries = 6
    $delayMs = 200

    for ($attempt = 1; $attempt -le $maxLockRetries; $attempt++) {
        & git @GitArgs
        if ($LASTEXITCODE -eq 0) { return }

        $isRetriable = ($LASTEXITCODE -eq 128) -or (Test-Path $lockPath)
        if ($isRetriable -and $attempt -lt $maxLockRetries) {
            $why = if (Test-Path $lockPath) { '.git/index.lock held by another process' } else { 'git exit 128 (possibly transient lock, released before check)' }
            Write-Host "  -> $why; retry $($attempt + 1)/$maxLockRetries in ${delayMs}ms" -ForegroundColor DarkYellow
            Start-Sleep -Milliseconds $delayMs
            $delayMs *= 2   # 200 -> 400 -> 800 -> 1600 -> 3200 -> ~6200 ms worst case
            continue
        }

        if (Test-Path $lockPath) {
            throw "git command failed (exit $LASTEXITCODE): $cmd`n  .git/index.lock still present after $maxLockRetries retries.`n  If no other git tool is running, the lock may be stale from a crashed process. Remove it manually: Remove-Item '$lockPath'"
        }
        throw "git command failed (exit $LASTEXITCODE): $cmd"
    }
}

function Invoke-Cargo {
    [CmdletBinding()]
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$CargoArgs)

    $cmd = 'cargo ' + ($CargoArgs -join ' ')
    if ($script:DryRun) {
        Write-Host "  (dry-run) $cmd" -ForegroundColor DarkGray
        return
    }
    Write-Host "  -> $cmd" -ForegroundColor DarkGray
    & cargo @CargoArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo command failed (exit $LASTEXITCODE): $cmd"
    }
}

# Silent existence check for a git ref. Returns $true / $false.
# Only stdout is redirected: git --quiet suppresses stderr, so no
# NativeCommandError wrapping happens on PS 5.1.
function Test-GitRef {
    param([string]$Ref)
    & git rev-parse --verify --quiet $Ref > $null
    return ($LASTEXITCODE -eq 0)
}

# Silent existence check for a local branch.
function Test-GitBranch {
    param([string]$Branch)
    return (Test-GitRef "refs/heads/$Branch")
}

# --------------------------------------------------------------------------
# Anchor at repo root
# --------------------------------------------------------------------------

$repoRoot = (& git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) {
    Write-Err 'Not inside a git repository.'
    exit 1
}
Set-Location $repoRoot
Write-Ok "Repository root: $repoRoot"

# --------------------------------------------------------------------------
# Enable rerere silently (idempotent, config-level flag)
# --------------------------------------------------------------------------

& git config rerere.enabled true    | Out-Null
& git config rerere.autoUpdate true | Out-Null

# --------------------------------------------------------------------------
# Reset integration branch to local master
# --------------------------------------------------------------------------

Write-Step "Resetting $IntegrationBranch to local $MasterBranch"
if (Test-GitBranch $IntegrationBranch) {
    Invoke-Git 'checkout' $IntegrationBranch
    Invoke-Git 'reset' '--hard' $MasterBranch
} else {
    Invoke-Git 'checkout' '-b' $IntegrationBranch $MasterBranch
}

# --------------------------------------------------------------------------
# Merge each feature branch in the configured order
# --------------------------------------------------------------------------

Write-Step "Merging $($MergeOrder.Count) feature branches"
$merged = @()
foreach ($branch in $MergeOrder) {
    Write-Host ''
    Write-Host "  * $branch" -ForegroundColor Magenta
    Invoke-Git 'merge' '--no-ff' "$OriginRemote/$branch" '-m' "Integrate $branch"
    $merged += $branch
}
Write-Ok 'All branches merged'

# --------------------------------------------------------------------------
# CI-gate checks (branch-scoped: only flag findings on files the branch
# actually modified vs. upstream/master; upstream lints stay informational)
# --------------------------------------------------------------------------

if (-not $NoChecks) {
    Write-Step 'Determining files touched by the branch (vs upstream/master)'
    $branchDiff = @(& git diff --name-only upstream/master...HEAD)
    if ($LASTEXITCODE -ne 0) {
        throw 'Could not compute branch diff. Is upstream/master fetched?'
    }
    $branchRustFiles = @($branchDiff `
        | ForEach-Object { $_ -replace '\\', '/' } `
        | Where-Object { $_ -match '\.rs$' -and (Test-Path $_) })
    Write-Ok "Branch touches $($branchRustFiles.Count) Rust file(s) (of $($branchDiff.Count) total)"

    # ----- fmt: file-scoped rustfmt (cargo fmt cannot narrow to files) -----
    # NOTE: --edition 2024 is required because Ruffle uses let-chains and
    #       async fn / async move; without an explicit edition, rustfmt
    #       falls back to Rust 2015 and refuses to parse the files.
    if ($branchRustFiles.Count -gt 0) {
        Write-Step "rustfmt --check --edition 2024 on $($branchRustFiles.Count) branch-modified Rust file(s)"
        if ($DryRun) {
            Write-Host "  (dry-run) rustfmt --check --edition 2024 $($branchRustFiles -join ' ')" -ForegroundColor DarkGray
        } else {
            & rustfmt --check --edition 2024 @branchRustFiles
            if ($LASTEXITCODE -ne 0) {
                throw "rustfmt reported formatting issues on branch-modified files"
            }
            Write-Ok 'rustfmt clean on branch-modified files'
        }
    } else {
        Write-Warn 'No Rust files changed by branch; skipping rustfmt check'
    }

    # ----- clippy: full run, JSON-parsed, gate only on in-scope findings ---
    Write-Step 'clippy (full workspace, JSON-filtered to branch scope)'
    if ($DryRun) {
        Write-Host "  (dry-run) cargo +beta clippy --all --tests --message-format=json | <filter>" -ForegroundColor DarkGray
    } else {
        $repoRootFwd = $repoRoot -replace '\\', '/'
        $branchFileSet = @{}
        foreach ($f in $branchRustFiles) { $branchFileSet[$f] = $true }

        $script:ourFindings   = 0
        $script:otherFindings = 0

        & cargo +beta clippy --all --tests --message-format=json | ForEach-Object {
            $line = $_
            if ([string]::IsNullOrWhiteSpace($line)) { return }
            if (-not $line.StartsWith('{')) { return }
            try { $msg = $line | ConvertFrom-Json } catch { return }
            if ($msg.reason -ne 'compiler-message') { return }
            $diag = $msg.message
            if (-not $diag) { return }
            if ($diag.level -notin @('error', 'warning')) { return }

            # Resolve each span to a repo-relative path and check scope
            $inScope = $false
            foreach ($span in $diag.spans) {
                if (-not $span.file_name) { continue }
                $sp = $span.file_name -replace '\\', '/'
                $sp = $sp -replace '^//\?/', ''    # strip Windows \\?\ prefix
                if ($sp.StartsWith("$repoRootFwd/")) {
                    $rel = $sp.Substring($repoRootFwd.Length + 1)
                    if ($branchFileSet.ContainsKey($rel)) {
                        $inScope = $true
                        break
                    }
                }
            }

            if ($inScope) {
                if ($diag.rendered) {
                    Write-Host $diag.rendered.TrimEnd() -ForegroundColor Red
                }
                $script:ourFindings++
            } else {
                $script:otherFindings++
            }
        }
        $cargoExit = $LASTEXITCODE

        Write-Host ''
        Write-Host 'Clippy summary:' -ForegroundColor Cyan
        Write-Host "  In-scope (branch files):   $($script:ourFindings) finding(s)"
        Write-Host "  Out-of-scope (upstream):   $($script:otherFindings) finding(s) -- informational, not gated"

        if ($script:ourFindings -gt 0) {
            throw "clippy gate FAILED: $($script:ourFindings) finding(s) on branch-modified files"
        }
        if ($cargoExit -ne 0 -and $script:otherFindings -eq 0) {
            Write-Warn "cargo exited $cargoExit but no diagnostics were parsed -- check output above (link error? missing toolchain?)"
        }
        Write-Ok 'Clippy gate clean on branch-modified files'
    }
} else {
    Write-Warn 'CI-gate checks skipped (-NoChecks)'
}

# --------------------------------------------------------------------------
# Chain wasm64 integration branch on top of the wasm32 tip
#
# The wasm64 branch is defined as `local/release-merge-wasm32` + the single
# `add-wasm64-target` feature branch. This block runs only if the wasm32
# rebuild + CI gate succeeded (a prior throw would have terminated the
# script). The wasm64 branch is intentionally kept local-only, mirroring
# the wasm32 sibling's convention, and by construction
#   git diff $IntegrationBranch $Wasm64Branch
# equals exactly the wasm64 feature branch.
# --------------------------------------------------------------------------

$Wasm64Branch  = 'local/release-merge-wasm64'
$Wasm64Feature = 'add-wasm64-target'

Write-Step "Chaining $Wasm64Branch on top of $IntegrationBranch"
if (Test-GitBranch $Wasm64Branch) {
    Invoke-Git 'checkout' $Wasm64Branch
    Invoke-Git 'reset' '--hard' $IntegrationBranch
} else {
    Invoke-Git 'checkout' '-b' $Wasm64Branch $IntegrationBranch
}

Write-Host ''
Write-Host "  * $Wasm64Feature" -ForegroundColor Magenta
Invoke-Git 'merge' '--no-ff' "$OriginRemote/$Wasm64Feature" '-m' "Integrate $Wasm64Feature (Memory64)"
Write-Ok "$Wasm64Branch ready"

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------

Write-Step 'Summary'
Write-Host "  Integration branches produced (local-only, ephemeral):"
Write-Host "    - $IntegrationBranch  (master + $($merged.Count) feature branches)"
Write-Host "    - $Wasm64Branch  (= $IntegrationBranch + $Wasm64Feature)"
Write-Host ''
Write-Host "  wasm32 merge order:"
foreach ($b in $merged) { Write-Host "    - $b" }
Write-Host ''
Write-Host "  After validating each build, tag the release and push the tag:"
Write-Host "    git tag -a enterprise-vX.Y.Z-wasm32 -m 'Enterprise release X.Y.Z (wasm32)'"
Write-Host "    git tag -a enterprise-vX.Y.Z-wasm64 -m 'Enterprise release X.Y.Z (wasm64)'"
Write-Host "    git push $OriginRemote enterprise-vX.Y.Z-wasm32 enterprise-vX.Y.Z-wasm64"
