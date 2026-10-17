$ErrorActionPreference = 'Stop'

# Use the normal DeckCraft settings so language, recent files, and preferences stay intact.
$env:DECKCRAFT_CONFIG_DIR = Join-Path $env:APPDATA 'DeckCraft'
$env:DECKCRAFT_NO_AUDIO = '1'
$exe = 'D:\Project\deckcraft-performance-0.6.2-performance.1-debug\deckcraft.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    throw "Debug executable not found: $exe"
}
Start-Process -FilePath $exe -WorkingDirectory (Split-Path -Parent $exe)
