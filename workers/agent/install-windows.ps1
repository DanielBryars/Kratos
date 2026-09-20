[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$DisplayName = $env:COMPUTERNAME,

    [ValidatePattern("^[a-zA-Z0-9][a-zA-Z0-9.-]{0,62}$")]
    [string]$AgentHostname = $env:COMPUTERNAME,

    [ValidateNotNullOrEmpty()]
    [string]$ControlPlane = "https://kratos.bryars.com",

    [ValidateNotNullOrEmpty()]
    [string]$Image = "ghcr.io/danielbryars/kratos-agent:edge",

    [ValidateNotNullOrEmpty()]
    [string]$ContainerName = "kratos-agent",

    [ValidateNotNullOrEmpty()]
    [string]$StateVolume = "kratos-agent-state",

    [string]$HealthCheckImage = ""
)

$ErrorActionPreference = "Stop"

if ($HealthCheckImage -and $HealthCheckImage -notmatch "^(?:[^\s@]+@)?sha256:[0-9a-f]{64}$") {
    throw "The health-check image must use an immutable sha256 digest."
}

function Invoke-Docker {
    param([Parameter(Mandatory)][string[]]$Arguments)

    & docker @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Docker failed with exit code $LASTEXITCODE."
    }
}

if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    throw "Docker is not installed or is not available on PATH."
}

Invoke-Docker -Arguments @("version", "--format", "{{.Server.Version}}") | Out-Null

$existingContainer = & docker ps -a --filter "name=^/$ContainerName$" --format "{{.Names}}"
if ($LASTEXITCODE -ne 0) {
    throw "Docker could not inspect existing containers."
}
if ($existingContainer) {
    throw "Container '$ContainerName' already exists. This installer will not replace it or its state."
}
Write-Host "Pulling $Image ..."
Invoke-Docker -Arguments @("pull", $Image) | Out-Host

$resolvedImage = & docker image inspect $Image --format "{{index .RepoDigests 0}}"
if ($LASTEXITCODE -ne 0 -or -not $resolvedImage) {
    throw "The pulled image did not expose an immutable repository digest."
}
$resolvedImage = $resolvedImage.Trim()

Write-Host "Checking GPU visibility in the Linux agent container ..."
$capabilityJson = & docker run --rm --gpus all $resolvedImage inspect
if ($LASTEXITCODE -ne 0) {
    throw "The agent image could not inspect this host with GPU access."
}
$capabilities = ($capabilityJson -join [Environment]::NewLine) | ConvertFrom-Json
if (-not $capabilities.gpus -or $capabilities.gpus.Count -eq 0) {
    throw "No NVIDIA GPU was visible inside the Linux agent container."
}
Write-Host "Detected: $($capabilities.gpus.name -join ', ')"

$runArguments = @(
    "run",
    "--detach",
    "--restart", "unless-stopped",
    "--gpus", "all",
    "--name", $ContainerName,
    "--hostname", $AgentHostname
)
if ($HealthCheckImage) {
    Write-Host "Pulling immutable GPU health check $HealthCheckImage ..."
    Invoke-Docker -Arguments @("pull", $HealthCheckImage) | Out-Host
    $runArguments += @(
        "--group-add", "0",
        "--mount", "type=bind,source=/var/run/docker.sock,target=/var/run/docker.sock"
    )
}
$runArguments += @(
    "--mount", "type=volume,source=$StateVolume,target=/var/lib/kratos-agent",
    $resolvedImage,
    "run",
    "--control-plane", $ControlPlane,
    "--display-name", $DisplayName,
    # The agent mounts a per-attempt subdirectory of this volume into each job that declares
    # outputs. It never exposes the volume root, which holds the worker credential.
    "--state-volume", $StateVolume
)
if ($HealthCheckImage) {
    $runArguments += @("--health-check-image", $HealthCheckImage)
}

Invoke-Docker -Arguments @("volume", "create", $StateVolume) | Out-Null
Invoke-Docker -Arguments $runArguments | Out-Null

$running = & docker inspect $ContainerName --format "{{.State.Running}}"
if ($LASTEXITCODE -ne 0 -or $running -ne "true") {
    throw "The agent container was created but did not remain running. Run 'docker logs $ContainerName' for details."
}

Write-Host "Kratos agent started from $resolvedImage"
Write-Host "Its identity is stored in Docker volume '$StateVolume'. Keep this volume across upgrades."
Write-Host "Compare the code below with the Kratos console, then approve the machine:"
Invoke-Docker -Arguments @("logs", "--tail", "30", $ContainerName) | Out-Host
