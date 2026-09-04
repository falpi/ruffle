#Requires -Version 5.1
<#
.SYNOPSIS
    Freeze the current fork state into a snapshot-<tag>/ tag namespace.

.DESCRIPTION
    Creates the immutable tag set that a frozen merge
    (Merge-Branches.ps1 -Snapshot <tag>) consumes:

        snapshot-<tag>/master              -> the pinned upstream master
        snapshot-<tag>/ruffle-enterprise   -> the fork home / build tooling
        snapshot-<tag>/<branch>            -> each feature-branch snapshot

    The <tag> label is supplied explicitly (see -Snapshot); it is an opaque
    name and need NOT be a date -- whatever release/versioning standard the
    fork adopts.

    Snapshots are lightweight tags: pure pointers to commits that already
    exist (origin/<branch>), so no history is copied. The tags are LOCAL;
    push them to make the fallback durable off-machine (the command is
    printed at the end).

    Assumption: every snapshot branch is already based on the snapshot
    master. A branch sitting on a DIFFERENT base is captured AS-IS -- this
    tool does not rebase. Align such a branch first if the snapshot must
    build conflict-free (e.g. rebase a branch stuck on a newer master back
    onto this one before tagging).

.PARAMETER Snapshot
    The snapshot tag label (mandatory). An opaque name -- a date, a version,
    or any standard the fork adopts; it is NOT derived from anything.

.PARAMETER Master
    Ref to pin as the release master. Default: master.

.PARAMETER Enterprise
    Ref to snapshot as snapshot-<tag>/ruffle-enterprise (the fork home,
    carrying this release's build tooling). Default: ruffle-enterprise.

.PARAMETER Remote
    Remote whose <branch> refs are snapshotted. Default: origin. Falls back
    to a local branch of the same name when the remote ref is absent.

.PARAMETER Force
    Overwrite tags that already exist in the target namespace.

.PARAMETER DryRun
    Print what would be tagged without creating anything.

.EXAMPLE
    powershell.exe -File .\scripts\Tag-Snapshot.ps1 -Snapshot 20260811 -DryRun
    Preview the snapshot-20260811/* tag set without creating anything.

.EXAMPLE
    powershell.exe -File .\scripts\Tag-Snapshot.ps1 -Snapshot 20260811
    Freeze snapshot-20260811/* from origin/* + master + ruffle-enterprise.

.NOTES
    Companion: Merge-Branches.ps1 -Snapshot <tag> consumes exactly the tag
    set this script creates.
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Snapshot,
    [string]$Master     = 'master',
    [string]$Enterprise = 'ruffle-enterprise',
    [string]$Remote     = 'origin',
    [switch]$Force,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

# --------------------------------------------------------------------------
# Full fork feature-branch set to snapshot.
#
# Deliberately a SUPERSET of the live Merge-Branches.ps1
# $MergeOrder: a snapshot must capture every branch that any build (active
# now, or re-enabled later) may reference, so nothing is lost once origin
# advances. Branches missing on the remote/locally are skipped with a warn.
# --------------------------------------------------------------------------
$SnapshotBranches = @(
    'fix-memory-leaks',
    'fix-blurry-grid-separators',
    'fix-mouse-events-clipping',
    'fix-mouse-hover-on-mx-buttons',
    'fix-mouse-click-release-outside-buttons',
    'fix-bitmapdata-filter-expansion',
    'fix-collator-compare-normalization',
    'fix-arcgis-flex-sdk-too-much-recursion',
    'fix-electron-instantiate-streaming',
    'add-text-input-event-dispatch',
    'add-mouse-cursor-property',
    'add-urlstream-implementation',
    'add-urlstream-improvements',
    'add-font-text-engine-implementation',
    'add-export-instruments',
    'tile-memory-allocator',
    'pluggable-font-renderer',
    'add-wasm64-target',
    'xml-general-improvements',
    'xml-readonly-improvements',
    'xml-notifications-completion'
)

# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------

function Write-Step { param($Message) Write-Host ''; Write-Host "==> $Message" -ForegroundColor Cyan }
function Write-Ok   { param($Message) Write-Host "[OK] $Message" -ForegroundColor Green }
function Write-Warn { param($Message) Write-Host ''; Write-Host "[WARN] $Message" -ForegroundColor Yellow }
function Write-Err  { param($Message) Write-Host ''; Write-Host "[ERR] $Message" -ForegroundColor Red }

# Silent existence check for a git ref. Returns $true / $false.
function Test-GitRef {
    param([string]$Ref)
    & git rev-parse --verify --quiet $Ref > $null
    return ($LASTEXITCODE -eq 0)
}

# Resolve a branch to the best available ref: prefer <remote>/<branch>,
# fall back to a local branch of the same name. Returns $null if neither.
function Resolve-BranchRef {
    param([string]$Branch)
    if (Test-GitRef "refs/remotes/$Remote/$Branch") { return "$Remote/$Branch" }
    if (Test-GitRef "refs/heads/$Branch")           { return $Branch }
    return $null
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
# Validate pins
# --------------------------------------------------------------------------

if (-not (Test-GitRef $Master))     { Write-Err "Master ref '$Master' not found.";         exit 1 }
if (-not (Test-GitRef $Enterprise)) { Write-Err "Enterprise ref '$Enterprise' not found."; exit 1 }

$ns = "snapshot-$Snapshot"

# --------------------------------------------------------------------------
# Build the tag plan (ref -> tag)
# --------------------------------------------------------------------------

Write-Step "Freezing $ns/*  (master=$Master, enterprise=$Enterprise, remote=$Remote)"

$plan = @()
$plan += [pscustomobject]@{ Tag = "$ns/master";            Source = $Master }
$plan += [pscustomobject]@{ Tag = "$ns/ruffle-enterprise"; Source = $Enterprise }

$missing = @()
foreach ($b in $SnapshotBranches) {
    $src = Resolve-BranchRef $b
    if ($null -eq $src) { $missing += $b; continue }
    $plan += [pscustomobject]@{ Tag = "$ns/$b"; Source = $src }
}
if ($missing.Count -gt 0) {
    Write-Warn "Branch(es) not found on '$Remote' or locally, skipped: $($missing -join ', ')"
}

# Refuse to clobber an existing snapshot namespace unless -Force
$existing = @($plan | Where-Object { Test-GitRef "refs/tags/$($_.Tag)" })
if ($existing.Count -gt 0 -and -not $Force) {
    Write-Err "Refusing to overwrite existing tags (pass -Force to replace):"
    $existing | ForEach-Object { Write-Host "    $($_.Tag)" }
    exit 1
}

# --------------------------------------------------------------------------
# Apply
# --------------------------------------------------------------------------

$created = 0
foreach ($p in $plan) {
    $sha = (& git rev-parse --short $p.Source).Trim()
    if ($DryRun) {
        Write-Host "  (dry-run) tag $($p.Tag) -> $($p.Source) ($sha)" -ForegroundColor DarkGray
        continue
    }
    $tagArgs = @('tag')
    if ($Force) { $tagArgs += '-f' }
    $tagArgs += @($p.Tag, $p.Source)
    & git @tagArgs
    if ($LASTEXITCODE -ne 0) { throw "git tag failed for $($p.Tag)" }
    Write-Host "  $($p.Tag) -> $sha"
    $created++
}

# --------------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------------

Write-Host ''
if ($DryRun) {
    Write-Ok "Dry-run: $($plan.Count) tag(s) would be created under $ns/"
} else {
    Write-Ok "$created tag(s) created under $ns/"
    Write-Host ''
    Write-Host "  Reproduce this snapshot:"
    Write-Host "    git show $ns/ruffle-enterprise:scripts/Merge-Branches.ps1 > run.ps1"
    Write-Host "    powershell.exe -File run.ps1 -Snapshot $Snapshot"
    Write-Host ''
    Write-Host "  Make it durable (push the snapshot tags):"
    Write-Host "    git push $Remote `"refs/tags/$ns/*:refs/tags/$ns/*`""
}
