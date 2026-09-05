#Requires -Version 5.1
<#
.SYNOPSIS
    Rebuild the local/merge-wasm32 and local/merge-wasm64
    integration branches.

.DESCRIPTION
    Rebuilds both integration branches on the local `master` branch:
      1. local/merge-wasm32 = master + all feature branches
         (merged in a defined order, with the CI-gate rustfmt+clippy run).
      2. local/merge-wasm64 = local/merge-wasm32 +
         add-wasm64-target (single-merge chain, no additional CI gate).
    PowerShell 5.1+ compatible.

    Both integration branches are INTENTIONALLY LOCAL-ONLY (the `local/`
    prefix is by convention). They are build artefacts, not shared branches:
    every run rewrites their history from scratch. Publishable release state
    lives on annotated release tags applied to a validated integration tip,
    not on the integration branch itself.

    Scope / assumptions (deliberately minimal):
      * The script performs NO state verification (working tree, current
        branch, remote configuration, feature-branch existence) and NO
        implicit fetch/pull. It is the caller's responsibility to make sure
        `master` and the feature branches (local, or `origin/*` under
        -FromOrigin) are at the desired commits before invoking.
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
        local/merge-wasm32 (rebuilt fresh every run, local-only)
            | (reset --hard + merge add-wasm64-target)
            v
        local/merge-wasm64 (rebuilt fresh every run, local-only)

    Merge strategy:
        Each feature branch is merged with `--no-ff`, producing an explicit
        merge commit named "Integrate <branch>". By default the LOCAL branch
        is merged when it exists (so unpushed work can be built and tested
        before publishing), falling back to `origin/<branch>`; -FromOrigin
        forces the pushed `origin/<branch>` for a build fully determined by
        the remote. git rerere is enabled so recurring conflict resolutions
        (e.g. xml-general vs xml-readonly) are replayed automatically.

    Recurring conflicts whose resolution rerere has recorded are replayed
    and committed automatically: `git merge` exits non-zero on ANY conflict,
    even one rerere fully re-resolved and staged, so the merge commit is
    finalized explicitly whenever no unmerged paths remain. A conflict with
    NO recorded resolution fails fast -- resolve it once (edit files, git
    add, git commit) to record it in rerere, then re-run.

.PARAMETER DryRun
    Print each git/cargo command instead of executing it. No side effects.

.PARAMETER NoChecks
    Skip the cargo fmt / clippy CI-gate checks at the end.

.PARAMETER Snapshot
    Merge a frozen snapshot instead of the live branches: merge the
    `snapshot-<tag>/<branch>` tags onto `snapshot-<tag>/master`, into
    `local/merge-wasm32-<tag>` / `local/merge-wasm64-<tag>`. Implies
    -NoChecks. Omit to merge the live integration (local branches by
    default; see -FromOrigin).
    NOTE: uses THIS script's MergeOrder; to reproduce a snapshot exactly, run
    that snapshot's own frozen tooling (snapshot-<tag>/ruffle-enterprise).

.PARAMETER FromOrigin
    Live builds only: merge the pushed `origin/<branch>` refs instead of the
    local branches, for a build fully determined by the remote ("push to
    publish"). Ignored with -Snapshot (frozen tags are the source).

.EXAMPLE
    powershell.exe -File .\scripts\Merge-Branches.ps1 -DryRun
    Show the sequence without touching anything.

.EXAMPLE
    powershell.exe -File .\scripts\Merge-Branches.ps1
    Rebuild both integration branches and run the CI-gate checks.
    After validating each build, tag the release with your chosen release-tag
    standard (not yet fixed) and push the tags.

.NOTES
    Companion snapshot tooling: scripts/Tag-Snapshot.ps1 freezes the tag set
    that -Snapshot consumes; scripts/Rebase-Branches.ps1 restacks the feature
    branches onto an updated upstream master.
#>

[CmdletBinding()]
param(
    [switch]$DryRun,
    [switch]$NoChecks,
    [string]$Snapshot,
    [switch]$FromOrigin
)

$ErrorActionPreference = 'Stop'

# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------

$MasterBranch      = 'master'
$OriginRemote      = 'origin'

# --------------------------------------------------------------------------
# Source selection (three modes):
#
#   default (local):  merge the LOCAL feature branch when it exists, else
#                     `origin/<branch>`, onto local `master`, into
#                     `local/merge-wasm32/64`. Builds/tests unpushed work
#                     before publishing; warns per branch when local diverges
#                     from origin so nothing enters the build unnoticed.
#   -FromOrigin:      same, but always the pushed `origin/<branch>` (build
#                     fully determined by the remote, "push to publish").
#   -Snapshot <tag>:  merge the FROZEN snapshot tags `snapshot-<tag>/<branch>`
#                     onto the pinned `snapshot-<tag>/master`, into the output
#                     branches `local/merge-wasm32-<tag>` / `-wasm64-<tag>`.
#                     The output stays under `local/` (ephemeral), never in
#                     the immutable `snapshot-<tag>/` namespace. Frozen tags are
#                     immutable refs, so this path does not depend on `origin`.
#
#   NOTE: -Snapshot uses THIS script's $MergeOrder. To reproduce a snapshot
#   exactly, run that snapshot's own frozen tooling
#   (snapshot-<tag>/ruffle-enterprise) whose $MergeOrder is pinned; this live
#   $MergeOrder evolves and may differ.
# --------------------------------------------------------------------------
if ($Snapshot) {
    $SourceMode        = 'snapshot'
    $MasterRef         = "snapshot-$Snapshot/master"
    $IntegrationBranch = "local/merge-wasm32-$Snapshot"
    $Wasm64Branch      = "local/merge-wasm64-$Snapshot"
    # A frozen snapshot was already validated when it was cut; the
    # branch-scope CI gate diffs against `upstream/master`, meaningless for
    # a pinned historical base, so it is skipped here.
    $NoChecks          = $true
} elseif ($FromOrigin) {
    $SourceMode        = 'origin'
    $MasterRef         = $MasterBranch
    $IntegrationBranch = 'local/merge-wasm32'
    $Wasm64Branch      = 'local/merge-wasm64'
} else {
    $SourceMode        = 'local'
    $MasterRef         = $MasterBranch
    $IntegrationBranch = 'local/merge-wasm32'
    $Wasm64Branch      = 'local/merge-wasm64'
}

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
    'add-urlstream-improvements',              # implementa URLStream con funzionalità estese (fork only)
    'add-export-instruments',                  # ottimizzazioni varie su XML 
    'tile-memory-allocator',                   # aggiunge nuovo memory allocator ottimizzato
    'add-font-text-engine-implementation',     # implementa api FTE
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

# Merge a ref, tolerating conflicts that rerere has already resolved.
#
# `git merge` exits non-zero whenever a conflict occurred, even when
# rerere.autoUpdate re-applied a recorded resolution and staged every
# conflicted path (leaving zero unmerged entries and a merge that only
# needs to be committed). That is exactly the steady state for the fork's
# recurring conflicts (e.g. fix-blurry-grid-separators vs the upstream
# drawing.rs delta): the resolution is known, rerere replays it, and the
# only thing left is the commit. Treat that case as success -- finalize the
# merge commit and continue. A merge left with genuine unmerged paths
# (rerere had no recorded resolution) is a real conflict: abort it and fail
# fast so the caller resolves it once (recording it in rerere) and re-runs.
function Invoke-Merge {
    param([string]$Ref, [string]$Message)

    if ($script:DryRun) {
        Write-Host "  (dry-run) git merge --no-ff $Ref -m `"$Message`"" -ForegroundColor DarkGray
        return
    }
    Write-Host "  -> git merge --no-ff $Ref -m `"$Message`"" -ForegroundColor DarkGray

    # Retry loop mirrors Invoke-Git: a transient .git/index.lock (SmartGit,
    # VSCode, indexers polling the repo) makes git fail with exit 128 before
    # the merge even starts. Retry ONLY on 128 -- a merge conflict is exit 1
    # (never retried), so recorded-resolution replay is handled below, not
    # mistaken for a lock.
    $lockPath = Join-Path $script:repoRoot '.git\index.lock'
    $maxLockRetries = 6
    $delayMs = 200
    for ($attempt = 1; $attempt -le $maxLockRetries; $attempt++) {
        & git merge --no-ff $Ref -m $Message
        if ($LASTEXITCODE -eq 0) { return }
        if ($LASTEXITCODE -eq 128 -and $attempt -lt $maxLockRetries) {
            Write-Host "  -> git exit 128 (transient index.lock?); retry $($attempt + 1)/$maxLockRetries in ${delayMs}ms" -ForegroundColor DarkYellow
            Start-Sleep -Milliseconds $delayMs
            $delayMs *= 2
            continue
        }
        break
    }

    # Non-zero exit that is not a transient lock: tell a rerere-resolved
    # conflict (finalize) apart from a real conflict (fail) or a non-merge
    # failure such as a bad ref (fail).
    & git rev-parse --verify --quiet MERGE_HEAD > $null
    if ($LASTEXITCODE -ne 0) {
        throw "git merge failed for '$Ref' with no merge in progress (bad ref or persistent lock)."
    }

    $unmerged = @(& git diff --name-only --diff-filter=U)
    if ($unmerged.Count -gt 0) {
        & git merge --abort
        throw "merge conflict in '$Ref' not auto-resolved by rerere:`n    $($unmerged -join "`n    ")`n  Resolve it once (git add / git commit) to record it in rerere, then re-run."
    }

    # Zero unmerged paths: rerere replayed a recorded resolution and staged
    # every conflicted file. git merge left the commit pending; finalize it.
    Write-Warn "rerere replayed a recorded resolution for '$Ref'; committing the resolved merge"
    Invoke-Git 'commit' '--no-edit'
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

# Resolve a feature branch to the ref to merge, per $SourceMode:
#   snapshot -> the frozen tag  snapshot-<tag>/<branch>
#   origin   -> origin/<branch> (the pushed state; "push to publish")
#   local    -> the LOCAL branch when it exists (so unpushed work can be
#               built and tested before publishing), else origin/<branch>.
#               Warns when the local branch diverges from origin so nothing
#               enters the build unnoticed.
function Resolve-MergeRef {
    param([string]$Branch)
    switch ($SourceMode) {
        'snapshot' { return "snapshot-$Snapshot/$Branch" }
        'origin'   { return "$OriginRemote/$Branch" }
        default {
            if (Test-GitBranch $Branch) {
                $originRef = "$OriginRemote/$Branch"
                if (Test-GitRef "refs/remotes/$originRef") {
                    $rl = (& git rev-list --left-right --count "$originRef...$Branch")
                    if ($LASTEXITCODE -eq 0) {
                        $parts = @($rl -split '\s+' | Where-Object { $_ -ne '' })
                        $behind = [int]$parts[0]; $ahead = [int]$parts[1]
                        if ($ahead -ne 0 -or $behind -ne 0) {
                            Write-Warn "local '$Branch' diverges from $originRef (ahead $ahead / behind $behind) -- building LOCAL"
                        }
                    }
                } else {
                    Write-Warn "local '$Branch' has no origin counterpart -- building LOCAL"
                }
                return $Branch
            }
            if (Test-GitRef "refs/remotes/$OriginRemote/$Branch") {
                Write-Warn "no local '$Branch' -- falling back to $OriginRemote/$Branch"
                return "$OriginRemote/$Branch"
            }
            throw "branch '$Branch' not found locally or on '$OriginRemote'."
        }
    }
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

Write-Step "Resetting $IntegrationBranch to $MasterRef"
if (Test-GitBranch $IntegrationBranch) {
    Invoke-Git 'checkout' $IntegrationBranch
    Invoke-Git 'reset' '--hard' $MasterRef
} else {
    Invoke-Git 'checkout' '-b' $IntegrationBranch $MasterRef
}

# --------------------------------------------------------------------------
# Merge each feature branch in the configured order
# --------------------------------------------------------------------------

Write-Step "Merging $($MergeOrder.Count) feature branches"
$merged = @()
foreach ($branch in $MergeOrder) {
    Write-Host ''
    Write-Host "  * $branch" -ForegroundColor Magenta
    Invoke-Merge (Resolve-MergeRef $branch) "Integrate $branch"
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
# The wasm64 branch is defined as `local/merge-wasm32` + the single
# `add-wasm64-target` feature branch. This block runs only if the wasm32
# rebuild + CI gate succeeded (a prior throw would have terminated the
# script). The wasm64 branch is intentionally kept local-only, mirroring
# the wasm32 sibling's convention, and by construction
#   git diff $IntegrationBranch $Wasm64Branch
# equals exactly the wasm64 feature branch.
# --------------------------------------------------------------------------

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
Invoke-Merge (Resolve-MergeRef $Wasm64Feature) "Integrate $Wasm64Feature (Memory64)"
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
Write-Host "  After validating each build, tag each tip with your release-tag"
Write-Host "  standard (not yet fixed) and push the tags."
