# Builds the installer for the version in tauri.conf.json, signed for the
# updater, and writes the `latest.json` that installed copies of Pier read to
# find it. Publishes nothing: it prints the command that does.
#
#   .\scripts\release.ps1
#
# The signing key is not in the repository. It is looked for in
# %UserProfile%\.tauri\pier.key (or wherever PIER_SIGNING_KEY points). Without
# the key that made the public key in tauri.conf.json, an installed Pier will
# refuse the update.
param([string]$Repo = 'tws9797/pier')

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root

$key = if ($env:PIER_SIGNING_KEY) { $env:PIER_SIGNING_KEY } else { Join-Path $env:USERPROFILE '.tauri\pier.key' }
if (-not (Test-Path $key)) { throw "No signing key at $key" }

$version = (Get-Content src-tauri\tauri.conf.json -Raw | ConvertFrom-Json).version
$env:TAURI_SIGNING_PRIVATE_KEY = $key
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ''
try {
    npm run tauri build
    if ($LASTEXITCODE -ne 0) { throw "tauri build failed ($LASTEXITCODE)" }
} finally {
    Remove-Item Env:TAURI_SIGNING_PRIVATE_KEY, Env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -ErrorAction SilentlyContinue
}

$dir = Join-Path $root 'src-tauri\target\release\bundle\nsis'
$name = "Pier_${version}_x64-setup.exe"
$installer = Join-Path $dir $name
$signature = "$installer.sig"
if (-not (Test-Path $installer)) { throw "No installer at $installer" }
if (-not (Test-Path $signature)) { throw "No signature at ${signature}: was the build signed?" }

# What an installed Pier downloads from the newest release to learn of this one.
$latest = Join-Path $dir 'latest.json'
[ordered]@{
    version   = $version
    pub_date  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = (Get-Content $signature -Raw).Trim()
            url       = "https://github.com/$Repo/releases/download/v$version/$name"
        }
    }
} | ConvertTo-Json -Depth 4 | Set-Content $latest -Encoding utf8NoBOM

"installer: $installer ($((Get-Item $installer).Length) bytes)"
"sha256:    $((Get-FileHash $installer -Algorithm SHA256).Hash.ToLower())"
"latest:    $latest"
''
'To publish (both files must be in the release, or installed copies will not see it):'
"  gh release create v$version `"$installer`" `"$latest`" --repo $Repo --title `"Pier $version`" --notes-file <notes.md> --latest"
