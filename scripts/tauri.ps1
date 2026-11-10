$ErrorActionPreference = 'Stop'

$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
$cargoExe = Join-Path $cargoBin 'cargo.exe'

if (-not (Test-Path -LiteralPath $cargoExe)) {
    throw "Rust toolchain was not found at $cargoExe. Install the stable MSVC toolchain first."
}

$env:Path = "$cargoBin;$env:Path"

$tauri = Join-Path $PSScriptRoot '..\node_modules\.bin\tauri.cmd'
if (-not (Test-Path -LiteralPath $tauri)) {
    throw 'Tauri CLI was not found. Run npm install in the project root first.'
}

& $tauri @args
exit $LASTEXITCODE
