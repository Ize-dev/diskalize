# Builds a release and collects everything a user needs into dist\Diskalize.
#
# The service binary cannot be overwritten while the service is running, so it
# is built into its own target directory rather than asking you to stop it.
#
#   powershell -ExecutionPolicy Bypass -File tools\package.ps1

$ErrorActionPreference = "Stop"
Set-Location (Split-Path $PSScriptRoot -Parent)

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$out = "dist\Diskalize"

Write-Host "Building $version ..."
cargo build --release --bin diskalize
cargo build --release --bin diskalize-service --target-dir target\pkg

Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$out\lang" | Out-Null

Copy-Item target\release\diskalize.exe            $out
Copy-Item target\pkg\release\diskalize-service.exe $out
Copy-Item target\release\diskalize.ico            $out   # the Explorer menu points at this
Copy-Item lang\*.lang                             "$out\lang"
Copy-Item README.md, LICENSE                      $out

$zip = "dist\Diskalize-$version-win-x64.zip"
Remove-Item $zip -Force -ErrorAction SilentlyContinue
Compress-Archive -Path "$out\*" -DestinationPath $zip

"{0}  ({1:N1} MB)" -f $zip, ((Get-Item $zip).Length / 1MB)
