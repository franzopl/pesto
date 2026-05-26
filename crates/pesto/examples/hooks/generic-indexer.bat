@echo off
:: Post-upload hook: send the NZB (and optional NFO) to a generic Newznab-compatible indexer.
::
:: Install:
::   copy generic-indexer.bat %APPDATA%\pesto\hooks\
::
:: Edit the variables below before use.
:: Requires curl (included in Windows 10 1803+ and Windows 11).

:: --- CONFIGURATION ---
set API_URL=https://indexer.example.com/v1/releases
set API_KEY=your-api-key
set CATEGORY_ID=0

:: --- pesto variables ---
:: PESTO_NZB  — path to the generated .nzb
:: PESTO_NFO  — path to the .nfo (empty when --nfo was not used)
:: PESTO_NAME — release name

if "%PESTO_NZB%"=="" goto err_no_nzb
if not exist "%PESTO_NZB%" goto err_no_nzb

for %%F in ("%PESTO_NZB%") do echo [Indexer] Sending: %%~nxF

set ARGS=-s -X POST "%API_URL%?apikey=%API_KEY%" -F "nzb_file=@%PESTO_NZB%" -F "category_id=%CATEGORY_ID%"

if not "%PESTO_NFO%"=="" (
    if exist "%PESTO_NFO%" (
        for %%F in ("%PESTO_NFO%") do echo [Indexer] With NFO: %%~nxF
        set ARGS=%ARGS% -F "nfo_file=@%PESTO_NFO%"
    )
)

for /f "delims=" %%R in ('curl %ARGS%') do set RESPONSE=%%R

echo %RESPONSE% | findstr /C:"public_id" >nul
if %errorlevel% neq 0 goto err_failed

for /f "tokens=2 delims=:," %%I in ('echo %RESPONSE%^| findstr "public_id"') do (
    set PUB_ID=%%~I
    set PUB_ID=!PUB_ID:"=!
)
echo [Indexer] OK — public_id: %PUB_ID%
exit /b 0

:err_no_nzb
echo [Indexer] Error: NZB not found (PESTO_NZB=%PESTO_NZB%).
exit /b 1

:err_failed
echo [Indexer] FAILED: %RESPONSE%
exit /b 1
