@echo off
REM Disbatch batch demo. Uses %1 and %2 as positional arguments, so Disbatch
REM generates two ordered controls. It also emits the @progress/@status protocol.
echo Disbatch batch demo starting...
echo   Argument 1 (source) = %1
echo   Argument 2 (mode)   = %2
echo @status Working
echo @progress 25
echo   ...step one
echo @progress 50
echo   ...step two
echo @progress 75
echo   ...step three
echo @progress 100
echo @status Done
echo Finished: processed %1 in %2 mode.
