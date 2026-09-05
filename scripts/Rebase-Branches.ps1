#Requires -Version 5.1
<#
.SYNOPSIS
    Rebase a declared tree of feature branches onto an updated master.

.DESCRIPTION
    Automates the "restack the whole feature forest onto the latest upstream
    master" chore, and also the "I edited a mid-chain branch, cascade it to
    its descendants" chore. PowerShell 5.1+ compatible.

    Topology is DECLARED EXPLICITLY in $RebasePlan, not inferred from commit
    ancestry. Inference is unusable here: adding a commit to a mid-chain
    branch A makes A's tip no longer an ancestor of its child B, so any
    ancestry-based guess would wrongly treat B as master-based -- precisely
    in the case (propagating an internal edit) where automation matters most.

    Each plan entry is  'branch > base'  where base is `master` or another
    branch declared EARLIER in the list. Bases must precede their dependents.

    Rebase mechanics (per entry 'C > P'):
      * fork = git merge-base <P start tip> <C start tip>, captured BEFORE any
        rebase runs.
      * git rebase --onto P fork C   (P resolves to its already-rebased tip).
    Using the pre-captured fork point as the rebase upstream replays only C's
    own commits onto the new P, never duplicating P's commits and correctly
    propagating any new commits P gained. This is the standard stacked-rebase.

    Safety model:
      * Before any rebase, each branch's pre-rebase tip is snapshotted as a
        lightweight tag 'rebase-<timestamp>/<branch>' (unless -NoBackup). Tags
        are just pointers to existing commits -- no storage cost -- and, unlike
        origin/*, they survive a subsequent force-push, so they remain a valid
        recovery source even after you publish the rebased branches.
      * Local branches are used AS-IS (created from origin only if missing).
        They are never reset to origin, so unpushed edits you intend to
        cascade are preserved. origin/* is left untouched by this script and
        remains a manual recovery source until you force-push.
      * Nothing is pushed by default (rebasing rewrites history). The build
        reads origin/*, so rebased branches do not affect a build until you
        force-push them. Use -Push to force-push (with lease) in bulk.
      * On an unresolved conflict the script aborts THAT branch's rebase
        (restoring it), stops, and reports. Branches already rebased in the
        run are left rebased locally; branches after the failure are skipped.
      * git rerere is enabled: a pre-recorded resolution (e.g. the known
        xml-general vs xml-readonly conflict) is replayed and the rebase is
        auto-continued. Only genuinely new conflicts stop the run.

.PARAMETER DryRun
    Print the resolved plan (order, base, fork point) and intended commands,
    then exit without modifying anything.

.PARAMETER SkipMasterSync
    Do not sync master from upstream. Use for a pure internal re-cascade
    (propagate a mid-chain edit) without pulling new upstream commits. The
    existing local master is used as the base as-is.

.PARAMETER Push
    After a fully successful run, force-push (with lease) every rebased
    branch to origin. Default: off (print the push commands instead).

.PARAMETER NoBackup
    Skip creating the pre-rebase backup tags ('rebase-<timestamp>/<branch>').
    Default: off (backups are created).

.EXAMPLE
    powershell.exe -File .\scripts\Rebase-Branches.ps1 -DryRun
    Show the plan without touching anything.

.EXAMPLE
    powershell.exe -File .\scripts\Rebase-Branches.ps1
    Sync master from upstream, rebase all branches locally, print push commands.

.EXAMPLE
    powershell.exe -File .\scripts\Rebase-Branches.ps1 -SkipMasterSync
    Re-cascade after editing a mid-chain branch, without pulling upstream.

.NOTES
    Companion to scripts/Merge-Branches.ps1. This script keeps the feature
    branches rebased on master; that one merges them into the local
    merge-integration branches.
#>

[CmdletBinding()]
param(
    [switch]$DryRun,
    [switch]$SkipMasterSync,
    [switch]$Push,
    [switch]$NoBackup
)

$ErrorActionPreference = 'Stop'

# Never let git block on an interactive editor (rebase --continue, etc.).
$env:GIT_EDITOR = 'true'
$env:GIT_SEQUENCE_EDITOR = 'true'

# --------------------------------------------------------------------------
# Configuration
# --------------------------------------------------------------------------

$MasterBranch   = 'master'
$UpstreamRemote = 'upstream'
$OriginRemote   = 'origin'

# Rebase plan: 'branch > base'. base = master or a branch declared EARLIER.
# Declare bases before their dependents. Reflects the actual origin topology:
# a single 4-deep chain (xml-notifications -> xml-general -> xml-readonly ->
# add-export-instruments); everything else sits directly on master.
$RebasePlan = @(
    'fix-memory-leaks                        > master'
    'fix-blurry-grid-separators              > master'
    'fix-bitmapdata-filter-expansion         > master'
    'fix-mouse-events-clipping               > master'
    'fix-mouse-hover-on-mx-buttons           > master'
    'fix-mouse-click-release-outside-buttons > master'
    'fix-arcgis-flex-sdk-too-much-recursion  > master'
    'fix-collator-compare-normalization      > master'
    'fix-electron-instantiate-streaming      > master'
    'add-text-input-event-dispatch           > master'
    'add-wasm64-target                       > master'
    'tile-memory-allocator                   > master'
    # urlstream stack, declared base-first:
    'add-urlstream-implementation            > master'
    'add-urlstream-improvements              > add-urlstream-implementation'
    # xml stack, declared base-first:
    'xml-notifications-completion            > master'
    'xml-general-improvements                > xml-notifications-completion'
    'xml-readonly-improvements               > xml-general-improvements'
    'add-export-instruments                  > xml-readonly-improvements'
    # font rendering stack, declared base-first:
    'add-font-text-engine-implementation     > master'
    'pluggable-font-renderer                 > add-font-text-engine-implementation'
)

# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------

function Write-Step { param($Message) Write-Host ''; Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok   { param($Message) Write-Host "[OK] $Message" -ForegroundColor Green }
function Write-Warn { param($Message) Write-Host ''; Write-Host "[WARN] $Message" -ForegroundColor Yellow }
function Write-Err  { param($Message) Write-Host ''; Write-Host "[ERR] $Message" -ForegroundColor Red }

# Core git runner with retry on git's generic fatal exit (128), which covers
# transient .git/index.lock contention from other git tools (SmartGit, etc.).
# Returns the exit code; never throws.
function Invoke-GitRaw {
    param([string[]]$GitArgs)
    $delayMs = 200
    for ($attempt = 1; $attempt -le 6; $attempt++) {
        # Pipe stdout to Out-Host so git's output is DISPLAYED but does NOT
        # become this function's return value. Without it, PowerShell folds
        # native stdout into the output stream, so the caller's $code ends up
        # an array like @('Your branch is behind...', 0) instead of the int 0
        # (git checkout / rebase print status lines to stdout).
        & git @GitArgs | Out-Host
        $code = $LASTEXITCODE
        if ($code -eq 0) { return 0 }
        if ($code -eq 128 -and $attempt -lt 6) {
            Write-Host "  -> git exit 128 (possible transient lock); retry $($attempt + 1)/6 in ${delayMs}ms" -ForegroundColor DarkYellow
            Start-Sleep -Milliseconds $delayMs
            $delayMs *= 2
            continue
        }
        return $code
    }
    return $code
}

# Throwing wrapper for steps that must succeed. Honors -DryRun (prints only).
function Invoke-Git {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$GitArgs)
    $cmd = 'git ' + ($GitArgs -join ' ')
    if ($script:DryRun) { Write-Host "  (dry-run) $cmd" -ForegroundColor DarkGray; return }
    Write-Host "  -> $cmd" -ForegroundColor DarkGray
    $code = Invoke-GitRaw $GitArgs
    if ($code -ne 0) { throw "git command failed (exit $code): $cmd" }
}

# Non-throwing wrapper: returns the exit code. Used where a non-zero result is
# expected and must be inspected (e.g. rebase hitting a conflict).
function Invoke-GitSoft {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$GitArgs)
    $cmd = 'git ' + ($GitArgs -join ' ')
    if ($script:DryRun) { Write-Host "  (dry-run) $cmd" -ForegroundColor DarkGray; return 0 }
    Write-Host "  -> $cmd" -ForegroundColor DarkGray
    return (Invoke-GitRaw $GitArgs)
}

# Read-only capture (rev-parse, merge-base). Runs even under -DryRun.
function Get-GitOut {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$GitArgs)
    $out = & git @GitArgs
    if ($LASTEXITCODE -ne 0) { throw "git command failed: git $($GitArgs -join ' ')" }
    return ($out | Out-String).Trim()
}

function Test-GitRef {
    param([string]$Ref)
    & git rev-parse --verify --quiet $Ref > $null
    return ($LASTEXITCODE -eq 0)
}
function Test-GitBranch { param([string]$Branch) return (Test-GitRef "refs/heads/$Branch") }

# True if $Ancestor is an ancestor of (or equal to) $Descendant.
function Test-IsAncestor {
    param([string]$Ancestor, [string]$Descendant)
    & git merge-base --is-ancestor $Ancestor $Descendant 2>$null
    return ($LASTEXITCODE -eq 0)
}

function Test-RebaseInProgress {
    return (Test-Path (Join-Path $script:repoRoot '.git\rebase-merge')) -or `
           (Test-Path (Join-Path $script:repoRoot '.git\rebase-apply'))
}

# After a rebase stops, drive it to completion IF rerere resolved every
# conflict; otherwise report the unresolved files and give up on this branch.
# Returns $true if the rebase finished, $false if a manual conflict remains.
function Resolve-RebaseViaRerere {
    param([string]$Branch)
    for ($guard = 0; $guard -lt 200; $guard++) {
        if (-not (Test-RebaseInProgress)) { return $true }
        $unmerged = & git diff --name-only --diff-filter=U
        if (-not [string]::IsNullOrWhiteSpace(($unmerged | Out-String))) {
            Write-Err "Unresolved conflict on '$Branch' in:`n$(( $unmerged | ForEach-Object { '    ' + $_ }) -join "`n")"
            return $false
        }
        # rerere staged the resolution (rerere.autoUpdate); continue.
        & git add -A > $null 2>&1
        Write-Host "  -> conflict auto-resolved by rerere; git rebase --continue" -ForegroundColor DarkYellow
        $code = Invoke-GitRaw @('rebase', '--continue')
        if ($code -ne 0 -and -not (Test-RebaseInProgress)) { return $false }
    }
    Write-Err "Rebase of '$Branch' did not converge after 200 continue steps."
    return $false
}

# --------------------------------------------------------------------------
# Anchor at repo root
# --------------------------------------------------------------------------

$repoRoot = (& git rev-parse --show-toplevel).Trim()
if ($LASTEXITCODE -ne 0) { Write-Err 'Not inside a git repository.'; exit 1 }
Set-Location $repoRoot
Write-Ok "Repository root: $repoRoot"

# --------------------------------------------------------------------------
# Parse and validate the plan
# --------------------------------------------------------------------------

Write-Step 'Parsing rebase plan'
$edges = @()
$declared = @{}
foreach ($line in $RebasePlan) {
    if ([string]::IsNullOrWhiteSpace($line)) { continue }
    $parts = $line -split '>', 2
    if ($parts.Count -ne 2) { throw "Invalid plan line (expected 'branch > base'): $line" }
    $child = $parts[0].Trim()
    $base  = $parts[1].Trim()
    if (-not $child -or -not $base) { throw "Invalid plan line: $line" }
    if ($declared.ContainsKey($child)) { throw "Duplicate branch in plan: $child" }
    if ($base -ne $MasterBranch -and -not $declared.ContainsKey($base)) {
        throw "Branch '$child' declares base '$base', which is neither '$MasterBranch' nor a branch declared earlier. Declare bases before dependents, and include the whole chain."
    }
    $edges += [pscustomobject]@{ Child = $child; Base = $base }
    $declared[$child] = $true
}
Write-Ok "Plan parsed: $($edges.Count) branch(es)"

# --------------------------------------------------------------------------
# Preflight
# --------------------------------------------------------------------------

$remotes = @(& git remote)
if ($remotes -notcontains $OriginRemote) { throw "Remote '$OriginRemote' is not configured." }
if (-not $SkipMasterSync -and $remotes -notcontains $UpstreamRemote) {
    throw "Remote '$UpstreamRemote' is not configured (needed for master sync). Re-run with -SkipMasterSync to skip."
}

& git config rerere.enabled true    | Out-Null
& git config rerere.autoUpdate true | Out-Null
Write-Ok 'rerere enabled with autoUpdate'

# --------------------------------------------------------------------------
# Step 1 + 2: sync master (origin <- upstream, local <- origin)
# --------------------------------------------------------------------------

if (-not $SkipMasterSync) {
    Write-Step "Syncing ${MasterBranch}: $UpstreamRemote -> $OriginRemote -> local"
    Invoke-Git 'fetch' $UpstreamRemote '--prune'
    # origin/master <- upstream/master, fast-forward only (no --force): a
    # non-ff here means someone committed on the fork's master -- fail loudly.
    $src = "$UpstreamRemote/$MasterBranch"
    $dst = "refs/heads/$MasterBranch"
    Invoke-Git 'push' $OriginRemote "${src}:${dst}"
    Invoke-Git 'fetch' $OriginRemote '--prune'
} else {
    Write-Warn "Skipping master sync (-SkipMasterSync); using local $MasterBranch as-is"
}

if (Test-GitBranch $MasterBranch) {
    Invoke-Git 'checkout' $MasterBranch
    if (-not $SkipMasterSync) { Invoke-Git 'merge' '--ff-only' "$OriginRemote/$MasterBranch" }
} elseif (Test-GitRef "$OriginRemote/$MasterBranch") {
    Invoke-Git 'checkout' '-b' $MasterBranch "$OriginRemote/$MasterBranch"
} else {
    throw "No local '$MasterBranch' and no '$OriginRemote/$MasterBranch' to create it from."
}

# --------------------------------------------------------------------------
# Phase 0: ensure local branches, capture start tips and fork points
# --------------------------------------------------------------------------

Write-Step 'Preparing local branches and capturing fork points'

# One timestamp for the whole run, so all backups group under one namespace.
$backupNs = "rebase-$(Get-Date -Format 'yyyyMMdd-HHmmss')"
if (-not $NoBackup) { Write-Host "  Pre-rebase snapshots -> tags '$backupNs/<branch>'" -ForegroundColor DarkGray }

$startTip = @{}
$startTip[$MasterBranch] = Get-GitOut 'rev-parse' $MasterBranch

foreach ($e in $edges) {
    $b = $e.Child
    if (Test-GitBranch $b) {
        if (Test-GitRef "$OriginRemote/$b") {
            $ahead  = Get-GitOut 'rev-list' '--count' "$OriginRemote/$b..$b"
            $behind = Get-GitOut 'rev-list' '--count' "$b..$OriginRemote/$b"
            if ($ahead -ne '0' -or $behind -ne '0') {
                Write-Warn "Local '$b' differs from $OriginRemote/$b (ahead $ahead, behind $behind); using LOCAL as-is"
            }
        }
        $startTip[$b] = Get-GitOut 'rev-parse' $b
    } elseif (Test-GitRef "$OriginRemote/$b") {
        if ($DryRun) {
            $startTip[$b] = Get-GitOut 'rev-parse' "$OriginRemote/$b"
        } else {
            Write-Host "  creating local '$b' from $OriginRemote/$b" -ForegroundColor DarkGray
            Invoke-Git 'branch' $b "$OriginRemote/$b"
            $startTip[$b] = Get-GitOut 'rev-parse' $b
        }
    } else {
        throw "Branch '$b' not found locally or on $OriginRemote."
    }

    # Snapshot the pre-rebase tip (a tag is just a pointer: no storage cost).
    if (-not $NoBackup) {
        Invoke-Git 'tag' "$backupNs/$b" $startTip[$b]
    }
}

$forkPoint = @{}
foreach ($e in $edges) {
    # Fork reference: the tip of the base branch the child is CURRENTLY built on.
    # A stacked child can be forked from either the local base tip or the origin
    # base tip depending on how far the run has progressed, and picking the wrong
    # one collapses the merge-base down to old master (replaying the parent's
    # commits onto the child). Choose whichever base tip is an ancestor of the
    # child right now:
    #   * local base is an ancestor  -> child already restacked on the rebased
    #     base (this or a prior run) -> fork = local tip -> a no-op rebase;
    #   * else origin/<base> is an ancestor -> child still on the pre-rebase
    #     base -> fork = origin tip (stable during a no-push session) -> a real
    #     rebase onto the freshly rebased local base.
    # (Do not push chain bases mid-session, or origin stops being the old tip.)
    if ($e.Base -eq $MasterBranch) {
        $baseRef = $startTip[$e.Base]
    } else {
        $localBase  = $startTip[$e.Base]
        $originBase = "$OriginRemote/$($e.Base)"
        if (Test-IsAncestor $localBase $startTip[$e.Child]) {
            $baseRef = $localBase
        } elseif ((Test-GitRef $originBase) -and (Test-IsAncestor $originBase $startTip[$e.Child])) {
            $baseRef = $originBase
        } else {
            $baseRef = $localBase
        }
    }
    $forkPoint[$e.Child] = Get-GitOut 'merge-base' $baseRef $startTip[$e.Child]
}

# --------------------------------------------------------------------------
# Show the resolved plan
# --------------------------------------------------------------------------

Write-Step 'Resolved rebase plan (in order)'
foreach ($e in $edges) {
    $fp = $forkPoint[$e.Child]
    $fpShort = if ($fp.Length -ge 9) { $fp.Substring(0, 9) } else { $fp }
    Write-Host ("    {0,-42} onto {1,-30} fork {2}" -f $e.Child, $e.Base, $fpShort)
}

if ($DryRun) {
    Write-Warn 'DryRun: no branches were modified.'
    return
}

# --------------------------------------------------------------------------
# Phase 1: rebase in declared order
# --------------------------------------------------------------------------

Write-Step 'Rebasing'
$done = @()
$failed = $null
foreach ($e in $edges) {
    $child = $e.Child
    $base  = $e.Base
    $fork  = $forkPoint[$child]

    Write-Host ''
    Write-Host "  * $child  onto  $base" -ForegroundColor Magenta
    Invoke-Git 'checkout' $child

    $code = Invoke-GitSoft 'rebase' '--onto' $base $fork
    if ($code -ne 0) {
        if (-not (Resolve-RebaseViaRerere -Branch $child)) {
            if (Test-RebaseInProgress) { Invoke-GitSoft 'rebase' '--abort' | Out-Null }
            $failed = $child
            break
        }
    }
    $done += $child
    Write-Ok "$child rebased onto $base"
}

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------

Write-Step 'Summary'
if ($done.Count -gt 0) {
    Write-Host "  Rebased OK ($($done.Count)):"
    foreach ($b in $done) { Write-Host "    - $b" }
} else {
    Write-Host '  Rebased OK: none'
}

if (-not $NoBackup) {
    Write-Host ''
    Write-Host "  Pre-rebase backups: tags '$backupNs/<branch>' (survive a force-push)"
    Write-Host "    restore one: git branch -f <branch> $backupNs/<branch>"
    Write-Host "    delete all : git tag -l '$backupNs/*' | ForEach-Object { git tag -d `$_ }"
}

if ($failed) {
    $skipped = @()
    $reached = $false
    foreach ($e in $edges) {
        if ($e.Child -eq $failed) { $reached = $true; continue }
        if ($reached) { $skipped += $e.Child }
    }
    Write-Err "STOPPED at '$failed': unresolved conflict. Its rebase was aborted (branch restored)."
    if ($skipped.Count -gt 0) { Write-Host "  Not processed: $($skipped -join ', ')" }
    Write-Host ''
    Write-Host "  Resolve manually, e.g.:"
    Write-Host "    git checkout $failed"
    Write-Host "    git rebase --onto $($edges | Where-Object { $_.Child -eq $failed } | ForEach-Object { $_.Base }) $($forkPoint[$failed]) $failed"
    Write-Host "    # fix conflicts, git add, git rebase --continue, then re-run this script"
    Write-Host ''
    Write-Host "  Recover any branch to its pre-rebase state:"
    if (-not $NoBackup) {
        Write-Host "    git checkout <branch>; git reset --hard $backupNs/<branch>   # pre-rebase snapshot (push-proof)"
    }
    Write-Host "    git checkout <branch>; git reset --hard $OriginRemote/<branch>   # last pushed state"
    exit 1
}

Write-Ok 'All branches rebased'

if ($Push) {
    Write-Step 'Force-pushing rebased branches (with lease)'
    foreach ($b in $done) { Invoke-Git 'push' $OriginRemote $b '--force-with-lease' }
    Write-Ok 'Push complete'
    Write-Host ''
    Write-Host "  Note: origin/$MasterBranch was already updated during the master sync step."
} else {
    Write-Warn 'No push (default). Rebased branches are LOCAL only.'
    Write-Host '  The build reads origin/*, so nothing changes for the build until you push.'
    Write-Host '  Review the branches, then force-push (with lease):'
    foreach ($b in $done) { Write-Host "    git push $OriginRemote $b --force-with-lease" }
}
