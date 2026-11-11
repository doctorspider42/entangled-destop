@echo off
rem Puts the WDDM driver on the display function (ADR-0010, opt-in): the
rem desktop is then composed on the host GPU - DWM through our Direct3D
rem user-mode driver, over Vulkan - instead of on the guest's CPU. Run as
rem administrator; wddm-off.cmd is the way back, and a boot that will not come
rem up with it is undone from Safe Mode with the same script.
set "dir=%ProgramFiles%\Entangled"
if not exist "%dir%\wddm\entangled-wddm.inf" (echo this guest's 3D package has no WDDM driver & exit /b 1)
"%dir%\gpu-test.exe" install "%dir%\wddm\entangled-wddm.inf" "PCI\VEN_1AF4&DEV_1050&SUBSYS_11001AF4&REV_01"
