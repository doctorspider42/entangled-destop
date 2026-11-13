@echo off
rem The way back from wddm-on.cmd: the display-only driver (with its vsync) on
rem the display function again. Run as administrator.
set "dir=%ProgramFiles%\Entangled"
"%dir%\gpu-test.exe" install "%dir%\owngpu\entangled-display.inf" "PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01"
