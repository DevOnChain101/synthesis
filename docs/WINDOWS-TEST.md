# Windows test checklist (for the owner)

Status: Windows is NOT verified. CI builds and tests `x86_64-pc-windows-msvc` on `windows-latest`, but nobody has run `synt` on a real Windows machine. This list is the first real run. There is no TUI or GUI; `synt` is a headless command line program. Everything below is PowerShell 7 (`pwsh`) unless noted.

Needs: Windows 10/11 x64, PowerShell 7+ (`winget install Microsoft.PowerShell`), Git for Windows (`winget install Git.Git`; the `Bash` tool uses its `bash.exe`), and a provider endpoint plus API key you can use from that machine.

## 0. Get the binary

No release exists yet, so use the dry run (docs/RELEASE.md, "Dry run"): GitHub, Actions, Release, Run workflow on `main`; when it is green download the artifact `release-assets` and unzip it. You need `synthesis-0.1.0-x86_64-pc-windows-msvc.zip` and `SHA256SUMS`.

```powershell
cd $HOME\Downloads\release-assets
# the hash must equal the line for the zip in SHA256SUMS
(Get-FileHash .\synthesis-0.1.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash.ToLower()
Select-String 'windows-msvc.zip' .\SHA256SUMS
```

After a real release: `irm https://github.com/DevOnChain101/synthesis/releases/latest/download/install.ps1 | iex` does steps 1 and 2 for you (it prints the PATH command when needed). Dry-run artifacts have no attestation, so for them use the manual steps.

## 1. Install (manual)

```powershell
$dir = "$env:LOCALAPPDATA\Programs\Synthesis"
New-Item -ItemType Directory -Force $dir | Out-Null
Expand-Archive .\synthesis-0.1.0-x86_64-pc-windows-msvc.zip -DestinationPath "$env:TEMP\synt-zip" -Force
Copy-Item "$env:TEMP\synt-zip\synthesis-0.1.0-x86_64-pc-windows-msvc\synt.exe" $dir -Force
# once, then OPEN A NEW TERMINAL
[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ";$dir", 'User')
```

SmartScreen: the binary is not Authenticode-signed. If Windows says "Windows protected your PC", choose More info, Run anyway, or run `Unblock-File $dir\synt.exe` before the first start. Write down whether it appeared.

## 2. Version

```powershell
synt --version
```
Expected: `synt 0.1.0`. If the shell says the command is not found: new terminal not opened, or `$dir` is not in `$env:Path`. Also run `Get-Command synt -All` and check that it is the only `synt` (the old Node CLI had the same name).

## 3. Doctor

```powershell
synt doctor --cwd $HOME
```
Expected (read these lines, then send the whole output on failure):
- first line `synt 0.1.0 on windows x86_64`;
- `shell: C:\Program Files\Git\bin\bash.exe kind=Posix is_posix=true`. If it says `kind=Cmd` or a `Git Bash was not found` note, Git for Windows is missing or in an unusual place; that is a finding, not a pass;
- the sandbox section says that this OS has no command confinement (Job Object only); that is expected. `synt run` then defaults to `--sandbox best-effort`;
- `SYNT_HOME: C:\Users\<you>\.synt`. If it points elsewhere, check the `HOME` variable (Git for Windows can set one; `HOME` wins over `USERPROFILE`);
- `git: git version ...`; `browser:` may be found or not (not needed here);
- `peers ... off`; no line may mention a path starting with `/` (POSIX path on Windows) or `/tmp`.

## 4. Help

```powershell
synt run --help
synt models
```
`run --help` lists `--cwd`, `--prompt`, `--sandbox`, `--permission-mode`. `synt models` prints nothing useful until step 5.

## 5. Provider config

Create `%USERPROFILE%\.synt\synt.json` (see docs/INSTALL.md, "Keys", for the same file with comments). Save it as UTF-8; a BOM is accepted. Put the key in a user environment variable, never in the file:

```powershell
New-Item -ItemType Directory -Force "$env:USERPROFILE\.synt" | Out-Null
notepad "$env:USERPROFILE\.synt\synt.json"      # paste the JSON, with your baseURL, model ids and {env:MY_GATEWAY_KEY}
[Environment]::SetEnvironmentVariable('MY_GATEWAY_KEY', '<your key>', 'User')   # then open a NEW terminal
synt models
synt doctor --cwd $HOME | Select-String 'config:'
```
Expected: `synt models` prints `<provider>/<model>` lines; `config: ok, ... default model: <provider>/<model>`. Do not paste the key anywhere you send results.

## 6. Read-only run (no approval needed)

```powershell
mkdir $HOME\synt-test; cd $HOME\synt-test
"hello" | Set-Content readme.txt
git init; git add .; git commit -m init
synt run --cwd . --prompt "Use the Glob tool to list the files here and name them." --output-format json
echo "exit=$LASTEXITCODE"
```
Expected: stderr shows a `synt: WARNING`/notice that commands run unconfined (Windows), then `synt: task completed (no verification applied)` (or `... (verified)`); the JSON has `"status":"completed"`, `"exit_code":0` and a result naming `readme.txt`; `exit=0`. Typical wall time is a few seconds.

## 7. Bash tool run

```powershell
synt run --cwd . --prompt "Run the Bash command: echo hello-from-windows. Report its output." --output-format json
synt run --cwd . --prompt "Run the Bash command: touch made-by-bash.txt" --output-format json; echo "exit=$LASTEXITCODE"
synt run --cwd . --permission-mode bypassPermissions --prompt "Run the Bash command: pwd; ls" --output-format json
```
Expected: 1) completes, the result contains `hello-from-windows` (a read-only `echo` needs no approval); 2) ends `blocked` with exit 4 and `approval needed for Bash(touch:*)`, and no file is created; 3) completes under `bypassPermissions` with a loud WARNING on stderr; `pwd` prints a path (Git Bash style like `/c/Users/...` is fine) and `ls` shows `readme.txt`.

Optional: `synt run --cwd . --sandbox enforce --prompt "say hi"` should refuse with exit 2, because Windows cannot confine commands (docs/CLI.md).

## 8. If something fails, send back

- the exact command and its exit code (`$LASTEXITCODE`), copied stdout and stderr (for `--output-format stream-json`, the first 30 lines are enough);
- `synt --version` and the full `synt doctor --cwd $HOME` output;
- Windows version (`winver`), `$PSVersionTable.PSVersion`, `git --version`, and whether SmartScreen, an antivirus or a proxy (`$env:HTTPS_PROXY`) is in play;
- for provider errors: the `baseURL` host (not the key), the model id, and the `reason` field of the JSON result;
- never include the API key or the contents of `%USERPROFILE%\.synt\` beyond `synt.json` with the key replaced.

Not covered by this list: npm install (`npm install -g @devonchain101/synthesis` on Windows, once published), `winget`, ARM64, Windows PowerShell 5.1 (`install.ps1` needs 7+), peers (they stay off unless `SYNT_HOME` is under `%USERPROFILE%`), and the browser tool.
