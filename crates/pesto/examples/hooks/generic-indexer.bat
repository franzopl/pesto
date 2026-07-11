@echo off
:: Post-upload hook: send the NZB (and optional NFO) to a Newznab-compatible indexer.
:: For video files, captures 6 screenshots with ffmpeg, uploads them to ImgBB,
:: and includes the URLs in the release submission.
::
:: Install:
::   copy generic-indexer.bat %APPDATA%\pesto\hooks\
::
:: Any script placed in that folder runs automatically after every upload, so
:: this is all you need to do — do NOT also add a `post_hook` entry pointing
:: at this same file in config.toml, or it will run twice per upload (once
:: from post_hook, once from the directory scan). Pick exactly one mechanism.
::
:: Edit the variables below before use.
:: Requires curl (Windows 10 1803+), ffmpeg (only required for video files).

:: ============================================================
::                      CONFIGURATION
:: ============================================================

set IMGBB_API_KEY=YOUR_IMGBB_API_KEY

set INDEXER_API_URL=https://indexer.example.com/v1/releases
set INDEXER_API_KEY=YOUR_API_KEY

set CATEGORY_ID=0

:: ============================================================
::                  END OF CONFIGURATION
:: ============================================================

:: --- pesto variables available in this hook ---
:: PESTO_NZB          -- path to the generated .nzb
:: PESTO_NFO          -- path to the .nfo (empty when --nfo was not used)
:: PESTO_NAME         -- release name
:: PESTO_INPUT_PATHS  -- colon-separated list of uploaded file paths
:: PESTO_BYTES        -- total uploaded bytes
:: PESTO_SERVER       -- server hostname
:: PESTO_GROUP        -- Usenet group
:: PESTO_PASSWORD     -- yEnc password (if any)

setlocal enabledelayedexpansion

if "%PESTO_NZB%"=="" goto err_no_nzb
if not exist "%PESTO_NZB%" goto err_no_nzb

for %%F in ("%PESTO_NZB%") do echo [Indexer] Sending: %%~nxF

:: ── detect video file ────────────────────────────────────────────────────────

set VIDEO_FILE=
set SHOT_URLS=

if "%PESTO_INPUT_PATHS%"=="" goto skip_screenshots

:: Iterate colon-separated paths
for %%P in ("%PESTO_INPUT_PATHS::=" "%") do (
    if exist "%%~P" (
        set "_EXT=%%~xP"
        call :check_video_ext "%%~P" "!_EXT!"
        if defined VIDEO_FILE goto try_screenshots
    )
)
goto skip_screenshots

:check_video_ext
set "_PATH=%~1"
set "_E=%~2"
for %%E in (.mkv .mp4 .avi .mov .m2ts .ts .wmv .flv .webm .mpg .mpeg .m4v .vob .m2v .mts) do (
    if /i "!_E!"=="%%E" (
        set "VIDEO_FILE=!_PATH!"
        exit /b
    )
)
exit /b

:try_screenshots
where ffmpeg  >nul 2>&1
if errorlevel 1 (
    echo [Indexer] WARNING: ffmpeg not found in PATH -- skipping screenshots.
    goto skip_screenshots
)
where ffprobe >nul 2>&1
if errorlevel 1 (
    echo [Indexer] WARNING: ffprobe not found in PATH -- skipping screenshots.
    goto skip_screenshots
)
if "%IMGBB_API_KEY%"=="YOUR_IMGBB_API_KEY" (
    echo [Indexer] WARNING: ImgBB API key is not set -- skipping screenshots.
    goto skip_screenshots
)

echo [Indexer] Video file detected: %VIDEO_FILE%
echo [Indexer] Capturing 6 screenshots...

:: Get duration via PowerShell (ffprobe output may have decimals)
for /f "delims=" %%D in ('powershell -NoProfile -Command ^
    "& ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 '%VIDEO_FILE%' 2>$null | ForEach-Object { [int][double]$_ }"') do (
    set DURATION=%%D
)

if not defined DURATION goto warn_duration
if %DURATION% LSS 30 goto warn_duration
goto do_screenshots

:warn_duration
echo [Indexer] WARNING: Could not determine video duration -- skipping screenshots.
goto skip_screenshots

:do_screenshots
set TMPDIR=%TEMP%\pesto-shots-%RANDOM%
mkdir "%TMPDIR%"

set SHOT_INDEX=0
set SHOT_URLS=

:: Offsets: 10% 24% 38% 52% 66% 80%
for %%O in (10 24 38 52 66 80) do (
    set /a SEEK=!DURATION! * %%O / 100
    set "SHOT_FILE=!TMPDIR!\shot_!SHOT_INDEX!.jpg"

    ffmpeg -ss !SEEK! -i "%VIDEO_FILE%" -vframes 1 -q:v 2 "!SHOT_FILE!" -y -loglevel error >nul 2>&1

    if exist "!SHOT_FILE!" (
        :: Upload to ImgBB. curl handles the multipart POST itself --
        :: Invoke-RestMethod's -Form parameter needs PowerShell 6+, which is
        :: not guaranteed to be installed (issue #41); PowerShell is only used
        :: afterwards, to parse the (nested) JSON response reliably.
        for /f "delims=" %%U in ('curl -s -X POST "https://api.imgbb.com/1/upload" -F "key=%IMGBB_API_KEY%" -F "image=@!SHOT_FILE!" ^| powershell -NoProfile -Command "($input | Out-String | ConvertFrom-Json).data.url"') do (
            set "IMG_URL=%%U"
        )
        if defined IMG_URL (
            set /a SHOT_NUM=!SHOT_INDEX! + 1
            echo [Indexer] Screenshot !SHOT_NUM!/6 uploaded: !IMG_URL!
            if defined SHOT_URLS (
                set "SHOT_URLS=!SHOT_URLS!,\"!IMG_URL!\""
            ) else (
                set "SHOT_URLS=\"!IMG_URL!\""
            )
        ) else (
            echo [Indexer] WARNING: ImgBB upload failed for shot !SHOT_INDEX!.
        )
    ) else (
        echo [Indexer] WARNING: ffmpeg failed at offset !SEEK!s.
    )
    set /a SHOT_INDEX=!SHOT_INDEX! + 1
)

rmdir /s /q "%TMPDIR%" 2>nul

:skip_screenshots

:: ── build indexer request ─────────────────────────────────────────────────────

set "ARGS=-s -X POST "%INDEXER_API_URL%?apikey=%INDEXER_API_KEY%" -F "nzb_file=@%PESTO_NZB%" -F "category_id=%CATEGORY_ID%""

if not "%PESTO_NFO%"=="" (
    if exist "%PESTO_NFO%" (
        for %%F in ("%PESTO_NFO%") do echo [Indexer] With NFO: %%~nxF
        set "ARGS=!ARGS! -F "nfo_file=@%PESTO_NFO%""
    )
)

if not "%PESTO_NAME%"=="" (
    set "ARGS=!ARGS! -F "name=%PESTO_NAME%""
)

if defined SHOT_URLS (
    set "ARGS=!ARGS! -F "screenshot_urls=[!SHOT_URLS!]""
    for /f %%C in ('echo !SHOT_URLS!^| find /c ","') do set /a SHOTS_COUNT=%%C+1
    echo [Indexer] Attaching !SHOTS_COUNT! screenshot URL(s).
)

:: ── submit ───────────────────────────────────────────────────────────────────

for /f "delims=" %%R in ('curl !ARGS!') do set "RESPONSE=%%R"

echo !RESPONSE! | findstr /C:"public_id" >nul 2>&1
if !errorlevel! equ 0 goto parse_id
echo !RESPONSE! | findstr /C:"""id""" >nul 2>&1
if !errorlevel! equ 0 goto parse_id
goto err_failed

:parse_id
for /f "tokens=2 delims=:," %%I in ('echo !RESPONSE!^| findstr "public_id"') do (
    set PUB_ID=%%~I
    set "PUB_ID=!PUB_ID:"=!"
)
if not defined PUB_ID (
    for /f "tokens=2 delims=:," %%I in ('echo !RESPONSE!^| findstr """id"""') do (
        set PUB_ID=%%~I
        set "PUB_ID=!PUB_ID:"=!"
    )
)
echo [Indexer] OK -- release id: !PUB_ID!
exit /b 0

:err_no_nzb
echo [Indexer] Error: NZB not found (PESTO_NZB=%PESTO_NZB%).
exit /b 1

:err_failed
echo [Indexer] FAILED: !RESPONSE!
exit /b 1
